//! Tool pipeline (tools.md §3): parse -> validate -> policy/approval -> PreToolUse -> budget ->
//! execute -> PostToolUse -> result. Also the harness-run tools: `Task`, `AskUserQuestion`,
//! `ExitPlanMode`.

use std::hash::{Hash, Hasher};
use std::sync::Mutex;
use std::sync::atomic::Ordering;
use std::time::Instant;

use futures_util::future::join_all;
use serde_json::{Value, json};
use synthesis_events::{Event, Todo, ToolStatus};
use synthesis_hooks::{Decision as HookDecision, HookEvent};
use synthesis_permissions::automode::ClassifierDecision;
use synthesis_permissions::{Decision, Mode, Suggestion, Verdict};
use synthesis_providers::Block;
use synthesis_tools::{
    Block as ToolBlock, Content as ToolContent, NeedsUser, SideEffectClass, ToolRuntime, ToolSpec,
};
use synthesis_verification::{
    Check, CheckAuth, CheckAuthorizer, CheckKind, DEFAULT_TIMEOUT_MS, Declarer, NEEDS_APPROVAL,
    is_verification_command,
};

use crate::agent::{Call, Problem};
use crate::context::wrap_tool_output;
use crate::harness::{AgentCtx, Harness, TaskRun};
use crate::live::{Live, result_artifact};
use crate::sink::Scope;
use crate::toolbox::validate;
use crate::traits::{Approval, ApprovalDecision, ApprovalRequest, DENY_MESSAGE, UserReply};
use crate::util::{cap_bytes, cap_text, lock, preview, sanitize_for_display, uuid};

/// `tool.completed.output` is the model-facing text up to this many bytes.
const OUTPUT_INLINE: usize = 64 * 1024;

/// What a tool says about its own full log, for `tool.completed`.
#[derive(Default)]
pub(crate) struct OutExtra {
    pub origin: Option<&'static str>,
    pub log: Option<std::path::PathBuf>,
    pub total_bytes: Option<u64>,
    pub tool_cut: bool,
    /// `meta.diff` of a file-changing tool, for `tool.completed.diff`.
    pub diff: Option<Value>,
}

/// Event size bound for `meta.diff` (the tool already caps its line text at 64 KiB; the rest is
/// JSON framing). A bigger one keeps its counts and loses its hunks.
const DIFF_EVENT_MAX: usize = 256 * 1024;

fn file_diff(meta: &Value) -> Option<Value> {
    let mut d = meta.get("diff").filter(|d| d.is_object())?.clone();
    if d.to_string().len() > DIFF_EVENT_MAX {
        d["hunks"] = json!([]);
        d["truncated"] = json!(true);
    }
    Some(d)
}

/// The gate's view of the permission engine: each check command is judged like a `Bash` call.
pub(crate) struct GateAuth<'a> {
    pub harness: &'a Harness,
    pub task: &'a TaskRun,
    pub agent: &'a AgentCtx,
    /// Commands of this evaluation whose set the person refused.
    pub denied: Mutex<std::collections::HashSet<String>>,
}

impl CheckAuthorizer for GateAuth<'_> {
    fn prepare<'a>(
        &'a self,
        checks: &'a [Check],
        _: &'a tokio_util::sync::CancellationToken,
    ) -> crate::traits::BoxFut<'a, ()> {
        Box::pin(
            self.harness
                .approve_check_set(self.task, self.agent, checks, &self.denied),
        )
    }

    fn authorize<'a>(
        &'a self,
        check: &'a Check,
        _: &'a tokio_util::sync::CancellationToken,
    ) -> crate::traits::BoxFut<'a, CheckAuth> {
        Box::pin(
            self.harness
                .authorize_check(self.task, self.agent, check, &self.denied),
        )
    }
}

/// A harness-inferred check that is a recognised verification command runs without an explicit
/// allow in `default` and `acceptEdits` (verification.md §4.3). Only an `ask` is lifted: deny
/// rules and protected paths still block, and the runner still sandboxes the command.
fn trusted_inferred(c: &Check, cmd: &str, mode: Mode) -> bool {
    c.declared_by == Declarer::Harness
        && matches!(mode, Mode::Default | Mode::AcceptEdits)
        && is_verification_command(cmd)
}

/// `Bash(cargo test:*)` for each rule a suggestion would add. Edit tools offer a mode, not a
/// rule: the tool name stands for the allow rule then.
pub(crate) fn rule_strings(tool: &str, s: &[Suggestion]) -> Vec<String> {
    let mut v: Vec<String> = vec![];
    for s in s {
        match s {
            Suggestion::AddRules { rules, .. } => {
                v.extend(rules.iter().map(|r| match r.rule_content.as_deref() {
                    Some(c) if !c.is_empty() => format!("{}({c})", r.tool_name),
                    _ => r.tool_name.clone(),
                }));
            }
            Suggestion::SetMode(_) => v.push(tool.to_string()),
        }
    }
    v.dedup();
    v
}

pub(crate) struct BatchOutcome {
    pub blocks: Vec<Block>,
    pub fingerprint: u64,
    pub all_unavailable: bool,
}

/// What the model sees for one call, plus what the loop needs to know about it.
pub(crate) struct Res {
    pub content: Value,
    pub text: String,
    pub is_error: bool,
    pub status: ToolStatus,
    /// Denied because nobody could approve.
    pub unavailable: bool,
}

struct Denial {
    text: String,
    unavailable: bool,
    /// Hook output goes to the model, never into the event log (events.md §5.1).
    from_hook: bool,
}

enum Plan {
    Done(Res),
    Run { input: Value, spec: ToolSpec },
    Special { input: Value },
}

type Pending = (usize, String, String, Value, ToolSpec);

fn side_effect_str(s: SideEffectClass) -> &'static str {
    match s {
        SideEffectClass::Read => "read",
        SideEffectClass::Write => "write",
        SideEffectClass::Execute => "execute",
        SideEffectClass::External => "external",
    }
}

fn suggestion_json(s: &Suggestion) -> Value {
    match s {
        Suggestion::AddRules { rules, behavior } => json!({
            "kind": "add_rules",
            "behavior": format!("{behavior:?}").to_lowercase(),
            "rules": rules.iter().map(|r| json!({"tool_name": r.tool_name, "rule_content": r.rule_content})).collect::<Vec<_>>(),
        }),
        Suggestion::SetMode(m) => json!({"kind": "set_mode", "mode": m.as_str()}),
    }
}

/// What the approver sees of the call. Never truncated: a cut command could hide the part that
/// matters. Callers pass the redacted input.
fn summarize(input: &Value) -> String {
    let s = ["command", "file_path", "path", "url", "pattern"]
        .iter()
        .find_map(|k| input.get(*k).and_then(Value::as_str))
        .map_or_else(|| input.to_string(), String::from);
    sanitize_for_display(&s)
}

impl Harness {
    fn agent_spec(&self, agent: &AgentCtx, name: &str) -> Option<ToolSpec> {
        self.tools
            .spec(name)
            .filter(|s| agent.allowed.as_ref().is_none_or(|a| a.contains(&s.name)))
    }

    /// Emits `tool.completed` and returns the model-visible result.
    #[allow(clippy::too_many_arguments)]
    fn complete(
        &self,
        task: &TaskRun,
        agent: &AgentCtx,
        turn: &str,
        id: &str,
        name: &str,
        t0: Option<Instant>,
        res: Res,
        not_run: bool,
        truncated: bool,
    ) -> Res {
        self.complete_ex(
            task,
            agent,
            turn,
            id,
            name,
            t0,
            res,
            not_run,
            truncated,
            OutExtra::default(),
        )
    }

    /// `output`, `output_truncated`, `output_bytes` and `result_ref` of `tool.completed`: the
    /// model-facing text whole up to 64 KiB, else its head and tail plus the whole as an artifact.
    pub(crate) fn result_fields(
        &self,
        task: &TaskRun,
        id: &str,
        text: &str,
        ex: &OutExtra,
    ) -> (
        Option<String>,
        bool,
        u64,
        Option<synthesis_events::ArtifactRef>,
    ) {
        let total = ex.total_bytes.unwrap_or(0).max(text.len() as u64);
        if text.is_empty() && total == 0 {
            return (None, false, 0, None);
        }
        let (inline, cut) = cap_bytes(text, OUTPUT_INLINE);
        let truncated = cut || ex.tool_cut || total > text.len() as u64;
        let aref = truncated
            .then(|| result_artifact(&task.workspace.artifact_root, id, text, ex.log.as_deref()))
            .flatten();
        (Some(inline), truncated, total, aref)
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn complete_ex(
        &self,
        task: &TaskRun,
        agent: &AgentCtx,
        turn: &str,
        id: &str,
        name: &str,
        t0: Option<Instant>,
        res: Res,
        not_run: bool,
        truncated: bool,
        ex: OutExtra,
    ) -> Res {
        let (output, output_truncated, output_bytes, result_ref) =
            self.result_fields(task, id, &res.text, &ex);
        self.emit(
            task,
            agent,
            Some(turn),
            Event::ToolCompleted {
                tool_call_id: id.into(),
                name: name.into(),
                status: res.status,
                is_error: res.is_error,
                not_run,
                duration_ms: t0.map_or(0, |t| t.elapsed().as_millis() as u64),
                preview: preview(&res.text),
                bytes: res.text.len() as u64,
                truncated,
                result_ref,
                origin: ex.origin.map(Into::into),
                output,
                output_truncated,
                output_bytes,
                diff: ex.diff,
            },
        );
        res
    }

    /// A harness-authored error (not tool output, so not wrapped as data).
    #[allow(clippy::too_many_arguments)]
    fn not_run(
        &self,
        task: &TaskRun,
        agent: &AgentCtx,
        turn: &str,
        id: &str,
        name: &str,
        status: ToolStatus,
        text: String,
        unavailable: bool,
    ) -> Res {
        let res = Res {
            content: Value::String(text.clone()),
            text,
            is_error: true,
            status,
            unavailable,
        };
        self.complete(task, agent, turn, id, name, None, res, true, false)
    }

    pub(crate) async fn run_batch(
        &self,
        task: &TaskRun,
        agent: &AgentCtx,
        turn: &str,
        calls: Vec<Call>,
    ) -> BatchOutcome {
        let mut slots: Vec<Option<Res>> = calls.iter().map(|_| None).collect();
        let mut group: Vec<Pending> = vec![];
        let parallel = task.caps.parallel_tools;
        for (i, call) in calls.iter().enumerate() {
            self.emit(
                task,
                agent,
                Some(turn),
                Event::ToolRequested {
                    tool_call_id: call.id.clone(),
                    name: call.name.clone(),
                    input: call
                        .input
                        .as_ref()
                        .map_or_else(|| json!({}), |i| self.tools.redact_input(&call.name, i)),
                    input_truncated: call.problem.is_some(),
                    origin: None,
                },
            );
            if agent.cancel.is_cancelled() {
                let r = self.not_run(
                    task,
                    agent,
                    turn,
                    &call.id,
                    &call.name,
                    ToolStatus::Interrupted,
                    "Interrupted before this tool ran.".into(),
                    false,
                );
                slots[i] = Some(r);
                continue;
            }
            match self.plan_call(task, agent, turn, call).await {
                Plan::Done(r) => slots[i] = Some(r),
                Plan::Run { input, spec } => {
                    if parallel && spec.side_effect == SideEffectClass::Read {
                        group.push((i, call.id.clone(), call.name.clone(), input, spec));
                    } else {
                        self.flush(task, agent, turn, &mut group, &mut slots).await;
                        slots[i] = Some(
                            self.exec_call(task, agent, turn, &call.id, &call.name, input, &spec)
                                .await,
                        );
                    }
                }
                Plan::Special { input } => {
                    self.flush(task, agent, turn, &mut group, &mut slots).await;
                    slots[i] = Some(self.run_special(task, agent, turn, call, input).await);
                }
            }
        }
        self.flush(task, agent, turn, &mut group, &mut slots).await;

        let mut h = std::collections::hash_map::DefaultHasher::new();
        let mut blocks = vec![];
        let mut all_unavailable = !calls.is_empty();
        for (call, slot) in calls.iter().zip(slots) {
            let r = slot.unwrap_or_else(|| Res {
                content: Value::String("Tool did not run.".into()),
                text: "Tool did not run.".into(),
                is_error: true,
                status: ToolStatus::Error,
                unavailable: false,
            });
            (
                &call.name,
                call.input.as_ref().map(Value::to_string),
                &r.text,
                r.is_error,
            )
                .hash(&mut h);
            all_unavailable &= r.unavailable;
            blocks.push(Block::ToolResult {
                tool_use_id: call.id.clone(),
                content: Some(r.content),
                is_error: Some(r.is_error),
            });
        }
        BatchOutcome {
            blocks,
            fingerprint: h.finish(),
            all_unavailable,
        }
    }

    async fn flush(
        &self,
        task: &TaskRun,
        agent: &AgentCtx,
        turn: &str,
        group: &mut Vec<Pending>,
        slots: &mut [Option<Res>],
    ) {
        let items = std::mem::take(group);
        let futs = items.iter().map(|(i, id, name, input, spec)| async move {
            (
                *i,
                self.exec_call(task, agent, turn, id, name, input.clone(), spec)
                    .await,
            )
        });
        for (i, r) in join_all(futs).await {
            slots[i] = Some(r);
        }
    }

    /// Counts the call against `max_tool_calls` / the wall clock; `Some` = not allowed to run.
    fn charge(&self, task: &TaskRun, agent: &AgentCtx, turn: &str, call: &Call) -> Option<Res> {
        let mut c = lock(&task.counters);
        let wall = task.started.elapsed().as_millis() as u64;
        let over = if c.tool_calls >= task.budget.max_tool_calls {
            Some((
                "max_tool_calls",
                f64::from(task.budget.max_tool_calls),
                f64::from(c.tool_calls),
            ))
        } else if task.budget.max_wall_ms.is_some_and(|m| wall >= m) {
            Some((
                "max_wall_ms",
                task.budget.max_wall_ms.unwrap_or(0) as f64,
                wall as f64,
            ))
        } else {
            c.tool_calls += 1;
            None
        };
        drop(c);
        let (kind, limit, used) = over?;
        self.mark_budget(task, agent, kind, limit, used);
        Some(self.not_run(
            task,
            agent,
            turn,
            &call.id,
            &call.name,
            ToolStatus::BudgetExceeded,
            format!(
                "Budget exceeded ({kind} = {limit}): the tool was not run and the task is stopping."
            ),
            false,
        ))
    }

    async fn plan_call(&self, task: &TaskRun, agent: &AgentCtx, turn: &str, call: &Call) -> Plan {
        let err = |text: String| {
            Plan::Done(self.not_run(
                task,
                agent,
                turn,
                &call.id,
                &call.name,
                ToolStatus::Error,
                text,
                false,
            ))
        };
        match &call.problem {
            Some(Problem::Truncated) => {
                return err("Tool input was cut off (output truncated at the max_tokens limit or invalid JSON). Retry with a smaller step.".into());
            }
            Some(Problem::Malformed(e)) => {
                return err(format!(
                    "Tool input was not valid JSON ({e}). Send the call again with a JSON object as input."
                ));
            }
            None => {}
        }
        let Some(mut input) = call.input.clone() else {
            return err("Tool input is missing.".into());
        };
        let Some(spec) = self.agent_spec(agent, &call.name) else {
            return err(format!("Unknown tool \"{}\"", call.name));
        };
        if let Err(e) = validate(&spec, &input) {
            // Schema messages quote the offending value.
            return err(if self.tools.is_sensitive(&call.name, &input) {
                format!(
                    "{}: invalid input (details withheld: sensitive input)",
                    call.name
                )
            } else {
                e
            });
        }
        // The interface tools only describe a question; they are not policy-checked here.
        if spec.runtime == ToolRuntime::Harness
            && !matches!(
                call.name.as_str(),
                "Task" | "SendMessage" | "ListAgents" | "ListSessions"
            )
        {
            return Plan::Special { input };
        }
        let denied = |d: Denial| {
            let shown = if d.from_hook {
                "Blocked by a PreToolUse hook (reason withheld from the event log).".to_string()
            } else {
                d.text.clone()
            };
            let mut r = self.not_run(
                task,
                agent,
                turn,
                &call.id,
                &call.name,
                ToolStatus::Denied,
                shown,
                d.unavailable,
            );
            r.content = Value::String(d.text.clone());
            r.text = d.text;
            Plan::Done(r)
        };

        // Policy, then PreToolUse; whenever the input changes after the hook saw it (an approver
        // edit), the hook runs again and the policy rechecks.
        let mut forced: Option<String> = None;
        let mut settled = false;
        for round in 0..4 {
            let before = input.clone();
            match self.mcp_check(task, &call.name, &input) {
                crate::mcp_guard::Check::Pass => {}
                crate::mcp_guard::Check::Reject(m) => return err(m),
                crate::mcp_guard::Check::Ask(r) => forced = forced.or(Some(r)),
            }
            if let Err(d) = self
                .authorize(
                    task,
                    agent,
                    turn,
                    call,
                    &spec,
                    &mut input,
                    forced.take(),
                    round > 0,
                )
                .await
            {
                return denied(d);
            }
            if round > 0 && input == before {
                settled = true;
                break;
            }
            let Some(pre) = self
                .run_hook(
                    task,
                    agent,
                    Some(turn),
                    HookEvent::PreToolUse,
                    json!({"tool_name": call.name, "tool_use_id": call.id, "tool_input": self.tools.redact_input(&call.name, &input)}),
                    Some(&call.name),
                )
                .await
            else {
                settled = true;
                break;
            };
            let why = |d: &str| pre.reason.clone().unwrap_or_else(|| d.to_string());
            if pre.blocked || pre.hook_decision == Some(HookDecision::Deny) {
                return denied(Denial {
                    text: format!("Blocked by a PreToolUse hook: {}", why("denied by hook")),
                    unavailable: false,
                    from_hook: true,
                });
            }
            // A hook may tighten (ask) but `allow` from a hook is advisory and ignored.
            forced = (pre.hook_decision == Some(HookDecision::Ask))
                .then(|| "a PreToolUse hook requested confirmation".to_string());
            let rewritten = pre.input_rewritten;
            if let Some(new) = pre.updated_input.clone().filter(|_| rewritten) {
                // The hook only saw the redacted input; its rewrite would carry the placeholder.
                if self.tools.is_sensitive(&call.name, &input) {
                    return err(
                        "A PreToolUse hook cannot rewrite a call with sensitive input.".into(),
                    );
                }
                if let Err(e) = validate(&spec, &new) {
                    return err(format!(
                        "A PreToolUse hook rewrote the input into an invalid one. {e}"
                    ));
                }
                input = new;
            }
            if !rewritten && forced.is_none() {
                settled = true;
                break;
            }
        }
        if !settled {
            return denied(Denial {
                text: "Permission denied: the input kept changing during approval.".into(),
                unavailable: false,
                from_hook: false,
            });
        }
        if spec.runtime == ToolRuntime::Harness {
            return Plan::Special { input };
        }
        if let Some(r) = self.charge(task, agent, turn, call) {
            return Plan::Done(r);
        }
        Plan::Run { input, spec }
    }

    /// Policy decision plus approval. `Err` = the call must not run. Input edited by the
    /// approver is validated and re-checked from scratch, bounded to a few rounds.
    #[allow(clippy::too_many_arguments)]
    async fn authorize(
        &self,
        task: &TaskRun,
        agent: &AgentCtx,
        turn: &str,
        call: &Call,
        spec: &ToolSpec,
        input: &mut Value,
        mut forced: Option<String>,
        recheck: bool,
    ) -> Result<(), Denial> {
        for round in 0..4 {
            let mode = *lock(&agent.mode);
            // SendMessage: deny/ask rules also see every spelling of the session `to` resolves to.
            let hinted = (call.name == "SendMessage").then(|| self.peer_check_input(input));
            let checked = hinted.as_ref().unwrap_or(&*input);
            let mut verdict = {
                let p = lock(&agent.perms);
                if recheck || round > 0 {
                    p.recheck(&call.name, checked, mode)
                } else {
                    p.check(&call.name, checked, mode)
                }
            };
            if verdict.decision == Decision::Classify {
                self.resolve_classify(task, agent, turn, call, input, mode, &mut verdict)
                    .await;
            }
            // A peer session the allow list permits may be asked without a prompt (B7); plan and
            // dontAsk, deny rules and every other reason stay as decided.
            if call.name == "SendMessage"
                && matches!(verdict.decision, Decision::Ask | Decision::Deny)
                && verdict.reason.contains("SendMessage requires confirmation")
                && self.peer_preapproved(input)
            {
                verdict.decision = Decision::Allow;
            }
            if verdict.decision == Decision::Allow
                && let Some(why) = forced.take()
            {
                verdict.decision = Decision::Ask;
                verdict.reason = why;
            }
            match verdict.decision {
                Decision::Allow => return Ok(()),
                Decision::Deny if verdict.reason.starts_with("non-interactive") => {
                    return Err(Denial {
                        unavailable: true,
                        from_hook: false,
                        text: self.approval_unavailable(
                            task,
                            agent,
                            call,
                            spec,
                            input,
                            &verdict.reason,
                        ),
                    });
                }
                Decision::Deny => {
                    return Err(Denial {
                        unavailable: false,
                        from_hook: false,
                        text: verdict.message.map_or_else(
                            || {
                                format!(
                                    "Permission denied: {}. The tool was NOT run. This is a policy decision (a rule or mode), not a missing approval: do not retry the same call and do not ask the user to run it manually; choose another approach, or stop, name the permission or rule that blocks it and report the task as blocked.{}",
                                    verdict.reason,
                                    bash_hint(&call.name, input)
                                )
                            },
                            String::from,
                        ),
                    });
                }
                _ => {}
            }
            if !lock(&agent.perms).env.interactive {
                return Err(Denial {
                    unavailable: true,
                    from_hook: false,
                    text: self.approval_unavailable(
                        task,
                        agent,
                        call,
                        spec,
                        input,
                        &verdict.reason,
                    ),
                });
            }
            let a = self.approve(task, agent, turn, call, input, &verdict).await;
            if a.decision != ApprovalDecision::Allow {
                let auto = a.decision == ApprovalDecision::AutoDenied;
                return Err(Denial {
                    unavailable: auto,
                    from_hook: false,
                    text: if auto {
                        self.approval_unavailable(task, agent, call, spec, input, &verdict.reason)
                    } else {
                        a.message.unwrap_or_else(|| DENY_MESSAGE.to_string())
                    },
                });
            }
            self.apply_accepted(agent, &call.name, input, &a);
            match a.updated_input {
                Some(new) if new != *input => {
                    // The approver was shown the redacted input; an edit would carry the placeholder.
                    if self.tools.is_sensitive(&call.name, input) {
                        return Err(Denial {
                            text: "Input of a call with sensitive values cannot be edited during approval."
                                .into(),
                            unavailable: false,
                            from_hook: false,
                        });
                    }
                    validate(spec, &new).map_err(|e| Denial {
                        text: e,
                        unavailable: false,
                        from_hook: false,
                    })?;
                    *input = new;
                    forced = None;
                }
                _ => return Ok(()),
            }
        }
        Err(Denial {
            text: "Permission denied: the input kept changing during approval.".into(),
            unavailable: false,
            from_hook: false,
        })
    }

    /// Settles a `Classify` verdict. Protected paths are a hard stop and never reach the judge;
    /// outside auto mode (plan) nobody classifies. Otherwise the judge's allow/deny stands and
    /// anything else is an ask (a person decides; headless: denied). `forced` asks (destructive
    /// MCP calls) are applied after this, so an allow here cannot lift them.
    #[allow(clippy::too_many_arguments)]
    async fn resolve_classify(
        &self,
        task: &TaskRun,
        agent: &AgentCtx,
        turn: &str,
        call: &Call,
        input: &Value,
        mode: Mode,
        verdict: &mut Verdict,
    ) {
        if verdict.reason.starts_with("protected path") {
            verdict.decision = Decision::Ask;
            verdict.hard = true;
            return;
        }
        if mode != Mode::Auto {
            verdict.decision = Decision::Ask;
            verdict.reason = format!("{} (classified only in auto mode)", verdict.reason);
            return;
        }
        let (d, why) = self
            .judge(task, agent, turn, call, input, &verdict.reason)
            .await;
        let note = sanitize_for_display(&why);
        match d {
            ClassifierDecision::Allow => {
                verdict.decision = Decision::Allow;
                verdict.reason = format!("auto mode: {note}");
            }
            ClassifierDecision::Block => {
                verdict.decision = Decision::Deny;
                verdict.reason = format!("auto mode judge blocked it: {note}");
            }
            ClassifierDecision::Ask => {
                verdict.decision = Decision::Ask;
                verdict.reason = format!("{} ({note})", verdict.reason);
            }
        }
    }

    /// Result text for a call that needs an approval nobody can give. Remembers the rule that
    /// would allow it (`task.ended.needed_permissions`) and that the task may be blocked on it.
    fn approval_unavailable(
        &self,
        task: &TaskRun,
        agent: &AgentCtx,
        call: &Call,
        spec: &ToolSpec,
        input: &Value,
        reason: &str,
    ) -> String {
        let rules = rule_strings(
            &call.name,
            &lock(&agent.perms).suggestions(&call.name, input),
        );
        // A denied change (write, command, MCP write tool) is what the task cannot go on
        // without; a denied read stays evidence the model may work around.
        if spec.side_effect != SideEffectClass::Read
            || matches!(call.name.as_str(), "Bash" | "verify_run" | "terminal_spawn")
        {
            task.pending_approval.store(true, Ordering::SeqCst);
        }
        lock(&task.needed).extend(rules.iter().cloned());
        let needed = if rules.is_empty() {
            format!("approval of this {} call", call.name)
        } else {
            rules.join(", ")
        };
        format!(
            "{} requires approval; no approver available in this run; do not ask the user to run it manually; report status blocked and name the permission needed: {needed} ({}). The tool was NOT run.{}",
            call.name,
            sanitize_for_display(reason),
            bash_hint(&call.name, input)
        )
    }

    /// A gate check is a command the harness runs for the task: it needs the same permission a
    /// `Bash` call with that command would. Allow = run; ask = the approver (interactive) or
    /// `needs_approval:<rule>` (nobody to ask); deny = blocked by policy.
    pub(crate) async fn authorize_check(
        &self,
        task: &TaskRun,
        agent: &AgentCtx,
        c: &Check,
        denied: &Mutex<std::collections::HashSet<String>>,
    ) -> CheckAuth {
        let Some(cmd) = c.command.as_deref() else {
            return CheckAuth::Allow;
        };
        let input = json!({ "command": cmd });
        let mode = *lock(&agent.mode);
        let mut verdict = lock(&agent.perms).check("Bash", &input, mode);
        let headless = verdict.reason.starts_with("non-interactive");
        let was_ask = verdict.decision == Decision::Ask || headless;
        match verdict.decision {
            Decision::Allow => return CheckAuth::Allow,
            _ if was_ask && trusted_inferred(c, cmd, mode) => return CheckAuth::Allow,
            Decision::Ask | Decision::Deny if lock(denied).contains(cmd) => {
                return CheckAuth::Blocked("denied by the user".into());
            }
            Decision::Ask if lock(&self.verify_ok).contains(cmd) => return CheckAuth::Allow,
            Decision::Deny if !headless => {
                return CheckAuth::Blocked(format!("denied by policy: {}", verdict.reason));
            }
            _ => {}
        }
        let call = Call {
            id: format!("verify-{}", c.id),
            name: "Bash".into(),
            input: Some(input.clone()),
            problem: None,
        };
        let interactive = lock(&agent.perms).env.interactive;
        if interactive && !headless {
            verdict.reason = format!("verification check {}: {}", c.id, verdict.reason);
            let a = self.approve(task, agent, "", &call, &input, &verdict).await;
            match a.decision {
                ApprovalDecision::Allow => {
                    self.apply_accepted(agent, "Bash", &input, &a);
                    return CheckAuth::Allow;
                }
                ApprovalDecision::AutoDenied => {}
                _ => return CheckAuth::Blocked("denied by the user".into()),
            }
        }
        let rules = rule_strings("Bash", &lock(&agent.perms).suggestions("Bash", &input));
        task.gate_blocked.store(true, Ordering::SeqCst);
        lock(&task.needed).extend(rules.iter().cloned());
        let rule = rules
            .first()
            .cloned()
            .unwrap_or_else(|| format!("Bash({cmd})"));
        CheckAuth::Blocked(format!("{NEEDS_APPROVAL}{rule}"))
    }

    /// One dialog for every check of the set that would ask (allow once / always for the set).
    /// Approved commands are remembered for the session in `verify_ok` (the gate only; a Bash
    /// call still follows the normal rules), refused ones in `denied`. Nothing to ask when
    /// non-interactive: `authorize_check` then reports `needs_approval`.
    pub(crate) async fn approve_check_set(
        &self,
        task: &TaskRun,
        agent: &AgentCtx,
        checks: &[Check],
        denied: &Mutex<std::collections::HashSet<String>>,
    ) {
        if !lock(&agent.perms).env.interactive {
            return;
        }
        let mode = *lock(&agent.mode);
        let asks: Vec<&str> = checks
            .iter()
            .filter_map(|c| c.command.as_deref().map(|cmd| (c, cmd)))
            .filter(|(c, cmd)| !trusted_inferred(c, cmd, mode))
            .map(|(_, cmd)| cmd)
            .filter(|cmd| {
                let v = lock(&agent.perms).check("Bash", &json!({ "command": cmd }), mode);
                v.decision == Decision::Ask
                    && !v.reason.starts_with("non-interactive")
                    && !lock(&self.verify_ok).contains(*cmd)
            })
            .collect();
        if asks.is_empty() {
            return;
        }
        let input = json!({ "command": asks.join("\n") });
        let mut verdict = lock(&agent.perms).check("Bash", &input, mode);
        verdict.reason = format!("verification runs {} check(s) for this task", asks.len());
        let call = Call {
            id: "verify-set".into(),
            name: "Bash".into(),
            input: Some(input.clone()),
            problem: None,
        };
        let a = self.approve(task, agent, "", &call, &input, &verdict).await;
        if a.decision == ApprovalDecision::Allow {
            self.apply_accepted(agent, "Bash", &input, &a);
            lock(&self.verify_ok).extend(asks.iter().map(|s| s.to_string()));
        } else if a.decision != ApprovalDecision::AutoDenied {
            lock(denied).extend(asks.iter().map(|s| s.to_string()));
        }
    }

    /// Applies "allow always" options, but only ones that were offered for this call.
    fn apply_accepted(&self, agent: &AgentCtx, tool: &str, input: &Value, a: &Approval) {
        if a.accepted.is_empty() {
            return;
        }
        let mut p = lock(&agent.perms);
        // Re-derive the offer: an approver cannot smuggle in rules or modes.
        let offered = p.suggestions(tool, input);
        let ok: Vec<Suggestion> = a
            .accepted
            .iter()
            .filter(|s| offered.contains(s))
            .cloned()
            .collect();
        if let Some(m) = p.apply(&ok) {
            *lock(&agent.mode) = m;
        }
        if let Some(store) = &self.rule_store {
            let rules: Vec<String> = ok
                .iter()
                .filter(|s| {
                    matches!(
                        s,
                        Suggestion::AddRules {
                            behavior: Decision::Allow,
                            ..
                        }
                    )
                })
                .flat_map(|s| rule_strings(tool, std::slice::from_ref(s)))
                .collect();
            if !rules.is_empty() {
                store(&rules);
            }
        }
    }

    async fn approve(
        &self,
        task: &TaskRun,
        agent: &AgentCtx,
        turn: &str,
        call: &Call,
        input: &Value,
        verdict: &Verdict,
    ) -> Approval {
        let suggestions = lock(&agent.perms).suggestions(&call.name, input);
        let approval_id = uuid();
        let dialog = if verdict.outside_read.is_some() {
            "outside_read"
        } else {
            "permission"
        };
        let shown = self.tools.redact_input(&call.name, input);
        let reason = sanitize_for_display(&verdict.reason);
        let prompt = format!(
            "{} wants to run {}: {}",
            sanitize_for_display(&call.name),
            summarize(&shown),
            reason
        );
        let mut options = vec!["allow".to_string(), "deny".to_string()];
        if !suggestions.is_empty() {
            options.insert(1, "allow_always".into());
        }
        let turn = (!turn.is_empty()).then_some(turn);
        self.emit(
            task,
            agent,
            turn,
            Event::ToolApprovalRequired {
                approval_id: approval_id.clone(),
                tool_call_id: call.id.clone(),
                dialog: dialog.into(),
                prompt: prompt.clone(),
                options,
                suggestions: suggestions.iter().map(suggestion_json).collect(),
                deadline: None,
            },
        );
        // Claude Code's `permission_prompt` notification: the run is waiting on an approval
        let _ = self
            .run_hook(
                task,
                agent,
                turn,
                HookEvent::Notification,
                json!({"message": prompt, "notification_type": "permission_prompt"}),
                Some("permission_prompt"),
            )
            .await;
        // An approval during verification returns to `verifying`, not `running`.
        let before = *lock(&task.status);
        self.set_status(
            task,
            synthesis_core::SessionStatus::WaitingApproval,
            "approval required",
        );
        let req = ApprovalRequest {
            approval_id: approval_id.clone(),
            agent_id: agent.agent_id.clone(),
            tool_call_id: call.id.clone(),
            tool: call.name.clone(),
            input: shown,
            dialog,
            prompt,
            reason,
            suggestions,
        };
        self.waiting(task, agent, true);
        let a = tokio::select! {
            biased;
            () = agent.cancel.cancelled() => Approval {
                reviewer: "policy",
                ..Approval::deny("Cancelled before the approval was answered.")
            },
            a = self.approver.approve(req, &agent.cancel) => a,
        };
        self.waiting(task, agent, false);
        self.emit(
            task,
            agent,
            turn,
            Event::ToolApprovalResolved {
                approval_id,
                decision: a.decision.as_str().into(),
                reviewer: a.reviewer.into(),
                input_changed: a.updated_input.is_some(),
                message: a.message.clone(),
            },
        );
        if !agent.cancel.is_cancelled() {
            let back = if before == synthesis_core::SessionStatus::Verifying {
                before
            } else {
                synthesis_core::SessionStatus::Running
            };
            self.set_status(task, back, "approval resolved");
        }
        a
    }

    fn render(&self, name: &str, c: &ToolContent) -> (Value, String, bool) {
        let max = self.config.max_tool_result_chars;
        match c {
            ToolContent::Text(t) => {
                let (capped, cut) = cap_text(t, max);
                (
                    Value::String(wrap_tool_output(name, &capped)),
                    t.clone(),
                    cut,
                )
            }
            ToolContent::Blocks(bs) => {
                let (mut vals, mut text, mut cut_any) = (vec![], String::new(), false);
                for b in bs {
                    match b {
                        ToolBlock::Text { text: t } => {
                            let (capped, cut) = cap_text(t, max);
                            vals.push(
                                json!({"type": "text", "text": wrap_tool_output(name, &capped)}),
                            );
                            text.push_str(t);
                            cut_any |= cut;
                        }
                        other => vals.push(serde_json::to_value(other).unwrap_or(Value::Null)),
                    }
                }
                (Value::Array(vals), text, cut_any)
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn exec_call(
        &self,
        task: &TaskRun,
        agent: &AgentCtx,
        turn: &str,
        id: &str,
        name: &str,
        input: Value,
        spec: &ToolSpec,
    ) -> Res {
        let ctx = self.tool_ctx(task, agent, turn, id);
        self.emit(
            task,
            agent,
            Some(turn),
            Event::ToolStarted {
                tool_call_id: id.into(),
                name: name.into(),
                side_effect: side_effect_str(spec.side_effect).into(),
                cwd: task.workspace.cwd.to_string_lossy().into_owned(),
            },
        );
        let t0 = Instant::now();
        let deadline = task.deadline;
        let live = Live::new(
            self.sink.clone(),
            Scope {
                task_id: Some(task.task_id.clone()),
                turn_id: Some(turn.to_string()),
                agent_id: agent.agent_id.clone(),
            },
            id,
            None,
        );
        let run = synthesis_tools::LIVE_OUTPUT.scope(
            live.output_sink(),
            self.tools.execute(name, input.clone(), &ctx),
        );
        let out = tokio::select! {
            biased;
            () = agent.cancel.cancelled() => Err(ToolStatus::Interrupted),
            () = async {
                match deadline {
                    Some(d) => tokio::time::sleep_until(tokio::time::Instant::from_std(d)).await,
                    None => std::future::pending().await,
                }
            } => Err(ToolStatus::Timeout),
            o = run => Ok(o),
        };
        live.finish();
        let out = match out {
            Ok(o) => o,
            Err(status) => {
                // The dropped tool future may have started work of its own: stop it through the
                // call's token. (A child of the task token: the task token itself is still needed
                // for the final verification.) Services a tool started on purpose, such as
                // background terminals or the browser, belong to the session and are closed by
                // the host with `Registry::close_session`, not by this timeout.
                ctx.cancel.cancel();
                let text = if status == ToolStatus::Interrupted {
                    "Interrupted.".to_string()
                } else {
                    "Tool exceeded the task time budget (max_wall_ms) and was stopped.".to_string()
                };
                let res = Res {
                    content: Value::String(text.clone()),
                    text,
                    is_error: true,
                    status,
                    unavailable: false,
                };
                return self.complete(task, agent, turn, id, name, Some(t0), res, false, false);
            }
        };

        // Execute-class tools may have changed anything even when they fail.
        match spec.side_effect {
            SideEffectClass::Write if !out.is_error => {
                task.side_effects.store(true, Ordering::SeqCst);
                task.pending_user_need.store(false, Ordering::SeqCst);
                task.pending_approval.store(false, Ordering::SeqCst);
                self.record_changed(task, &input);
                self.record_edit(task, &out.meta);
            }
            SideEffectClass::Execute => {
                // Read-only / build / test / lint commands change nothing the gate should verify.
                let inert = name == "Bash"
                    && input
                        .get("command")
                        .and_then(Value::as_str)
                        .is_some_and(synthesis_permissions::is_auto_safe_bash);
                if !inert {
                    task.side_effects.store(true, Ordering::SeqCst);
                }
                task.pending_user_need.store(false, Ordering::SeqCst);
                task.pending_approval.store(false, Ordering::SeqCst);
                // A shell may have changed anything, even when it fails.
                for f in self.tracker.changed(&task.workspace, &task.task_id).await {
                    self.add_changed(task, f);
                }
            }
            _ => {}
        }
        if name == "verify_run" && !out.is_error {
            self.register_model_checks(task, &out.meta);
        }
        if let Some(todos) = out.meta.get("todos").and_then(Value::as_array) {
            let s = |t: &Value, k: &str| {
                t.get(k)
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string()
            };
            let todos = todos
                .iter()
                .map(|t| Todo {
                    id: t.get("id").and_then(Value::as_str).map(String::from),
                    content: s(t, "content"),
                    status: s(t, "status"),
                    active_form: s(t, "activeForm"),
                })
                .collect();
            self.emit(task, agent, Some(turn), Event::TodoUpdated { todos });
        }

        let (mut content, text, cut) = self.render(name, &out.content);
        self.mcp_observe(task, name, &input, &text, out.is_error);
        if let Some(post) = self
            .run_hook(
                task,
                agent,
                Some(turn),
                HookEvent::PostToolUse,
                json!({"tool_name": name, "tool_use_id": id, "tool_input": self.tools.redact_input(name, &input), "tool_response": text}),
                Some(name),
            )
            .await
            && !post.additional_context.is_empty()
        {
            let extra = post.additional_context.join("\n");
            match &mut content {
                Value::String(s) => {
                    s.push_str("\n\n");
                    s.push_str(&extra);
                }
                Value::Array(a) => a.push(json!({"type": "text", "text": extra})),
                _ => {}
            }
        }
        let status = crate::agent::tool_status_for(out.is_error, out.soft);
        let res = Res {
            content,
            text,
            is_error: out.is_error,
            status,
            unavailable: false,
        };
        let ex = OutExtra {
            origin: None,
            log: out
                .meta
                .get("artifact_path")
                .and_then(Value::as_str)
                .map(Into::into),
            total_bytes: out.meta.get("total_bytes").and_then(Value::as_u64),
            tool_cut: out.meta.get("truncated").and_then(Value::as_bool) == Some(true),
            diff: file_diff(&out.meta),
        };
        self.complete_ex(task, agent, turn, id, name, Some(t0), res, false, cut, ex)
    }

    fn record_changed(&self, task: &TaskRun, input: &Value) {
        let Some(p) = ["file_path", "notebook_path"]
            .iter()
            .find_map(|k| input.get(*k).and_then(Value::as_str))
        else {
            return;
        };
        let ws = &task.workspace;
        let shown = ws
            .resolve(p)
            .ok()
            .and_then(|abs| {
                abs.strip_prefix(&ws.root)
                    .ok()
                    .map(|r| r.to_string_lossy().into_owned())
            })
            .unwrap_or_else(|| p.to_string());
        self.add_changed(task, shown);
    }

    /// Keeps the added/removed lines of an Edit/Write diff (hidden or truncated diffs have none).
    fn record_edit(&self, task: &TaskRun, meta: &Value) {
        let Some(d) = meta.get("diff").filter(|d| d.is_object()) else {
            return;
        };
        let mut e = synthesis_verification::FileEdit {
            path: d["path"].as_str().unwrap_or_default().into(),
            ..Default::default()
        };
        let lines = d["hunks"].as_array().into_iter().flatten();
        for l in lines.flat_map(|h| h["lines"].as_array().into_iter().flatten()) {
            let text = l["text"].as_str().unwrap_or_default().to_string();
            match l["tag"].as_str() {
                Some("+") => e.added.push(text),
                Some("-") => e.removed.push(text),
                _ => {}
            }
        }
        lock(&task.edits).push(e);
    }

    fn add_changed(&self, task: &TaskRun, file: String) {
        let mut c = lock(&task.changed);
        if !c.contains(&file) {
            c.push(file);
        }
    }

    /// Checks the model declared through `verify_run` (`meta.checks`, `declared_by: model`) go
    /// to the completion gate, which re-runs them itself. Same command = same check.
    fn register_model_checks(&self, task: &TaskRun, meta: &Value) {
        let mut all = lock(&task.model_checks);
        for c in meta
            .get("checks")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter(|c| c.get("declared_by").and_then(Value::as_str) == Some("model"))
        {
            let Some(cmd) = c.get("command").and_then(Value::as_str) else {
                continue;
            };
            if all.iter().any(|k| k.command.as_deref() == Some(cmd)) {
                continue;
            }
            let id = c.get("id").and_then(Value::as_str).unwrap_or("m");
            let id = if all.iter().any(|k| k.id == id) {
                format!("m{}", all.len() + 1)
            } else {
                id.to_string()
            };
            all.push(Check {
                id,
                kind: CheckKind::Custom,
                command: Some(cmd.to_string()),
                cwd: task.workspace.cwd.clone(),
                timeout_ms: DEFAULT_TIMEOUT_MS,
                declared_by: Declarer::Model,
                criteria_ids: vec![],
            });
        }
    }

    // ---- harness-run tools ---------------------------------------------------------------

    async fn run_special(
        &self,
        task: &TaskRun,
        agent: &AgentCtx,
        turn: &str,
        call: &Call,
        input: Value,
    ) -> Res {
        if let Some(r) = self.charge(task, agent, turn, call) {
            return r;
        }
        self.emit(
            task,
            agent,
            Some(turn),
            Event::ToolStarted {
                tool_call_id: call.id.clone(),
                name: call.name.clone(),
                // a message to a peer session leaves this one: `external`
                side_effect: if call.name == "SendMessage"
                    && input
                        .get("to")
                        .and_then(Value::as_str)
                        .is_some_and(|t| synthesis_peers::address::parse(t).is_some())
                {
                    "external"
                } else {
                    "read"
                }
                .into(),
                cwd: task.workspace.cwd.to_string_lossy().into_owned(),
            },
        );
        let t0 = Instant::now();
        let res = if call.name == "Task" {
            self.run_subagent(task, agent, turn, &call.id, input).await
        } else if call.name == "SendMessage"
            && input
                .get("to")
                .and_then(Value::as_str)
                .is_some_and(|t| synthesis_peers::address::parse(t).is_some())
        {
            self.run_peer_send(task, agent, &input).await
        } else if call.name == "ListSessions"
            || (call.name == "ListAgents"
                && input.get("scope").and_then(Value::as_str) == Some("all"))
        {
            self.run_peer_list(call.name == "ListSessions").await
        } else if matches!(call.name.as_str(), "SendMessage" | "ListAgents") {
            self.run_comm(task, agent, &call.name, &input)
        } else {
            self.run_interface(task, agent, turn, call, input).await
        };
        self.complete(
            task,
            agent,
            turn,
            &call.id,
            &call.name,
            Some(t0),
            res,
            false,
            false,
        )
    }

    pub(crate) fn plain(&self, text: impl Into<String>, is_error: bool, status: ToolStatus) -> Res {
        let text = text.into();
        Res {
            content: Value::String(text.clone()),
            text,
            is_error,
            status,
            unavailable: false,
        }
    }

    /// AskUserQuestion / ExitPlanMode: the tool only describes what is needed (`needs_user`);
    /// the answer comes from the `InputProvider`.
    async fn run_interface(
        &self,
        task: &TaskRun,
        agent: &AgentCtx,
        turn: &str,
        call: &Call,
        input: Value,
    ) -> Res {
        let ctx = self.tool_ctx(task, agent, turn, &call.id);
        let out = tokio::select! {
            biased;
            () = agent.cancel.cancelled() => return self.plain("Interrupted.", true, ToolStatus::Interrupted),
            o = self.tools.execute(&call.name, input, &ctx) => o,
        };
        let need = out
            .meta
            .get("needs_user")
            .and_then(|v| serde_json::from_value::<NeedsUser>(v.clone()).ok());
        let Some(need) = need.filter(|_| !out.is_error) else {
            let (content, text, _) = self.render(&call.name, &out.content);
            return Res {
                content,
                text,
                is_error: out.is_error,
                status: crate::agent::tool_status_for(out.is_error, out.soft),
                unavailable: false,
            };
        };

        let (dialog, prompt, options) = match &need {
            NeedsUser::AskUserQuestion { questions } => (
                "question",
                questions
                    .first()
                    .map(|q| q.question.clone())
                    .unwrap_or_default(),
                questions
                    .first()
                    .map(|q| q.options.iter().map(|o| o.label.clone()).collect())
                    .unwrap_or_default(),
            ),
            NeedsUser::ExitPlanMode { plan } => (
                "plan",
                preview(plan),
                vec!["approve".to_string(), "keep_planning".to_string()],
            ),
        };
        let approval_id = uuid();
        self.emit(
            task,
            agent,
            Some(turn),
            Event::ToolApprovalRequired {
                approval_id: approval_id.clone(),
                tool_call_id: call.id.clone(),
                dialog: dialog.into(),
                prompt,
                options,
                suggestions: vec![],
                deadline: None,
            },
        );
        self.set_status(
            task,
            synthesis_core::SessionStatus::WaitingApproval,
            "user input required",
        );
        let reply = tokio::select! {
            biased;
            () = agent.cancel.cancelled() => None,
            r = self.input.ask(&need, &agent.cancel) => r,
        };
        let resolved =
            |decision: &str, reviewer: &str, message: Option<String>| Event::ToolApprovalResolved {
                approval_id: approval_id.clone(),
                decision: decision.into(),
                reviewer: reviewer.into(),
                input_changed: false,
                message,
            };
        if agent.cancel.is_cancelled() {
            self.emit(
                task,
                agent,
                Some(turn),
                resolved("deny", "user", Some("cancelled".into())),
            );
            return self.plain("Interrupted.", true, ToolStatus::Interrupted);
        }
        self.set_status(
            task,
            synthesis_core::SessionStatus::Running,
            "user input resolved",
        );
        match (reply, &need) {
            (None, _) => {
                self.emit(
                    task,
                    agent,
                    Some(turn),
                    resolved("auto_denied", "headless", Some("no user available".into())),
                );
                task.pending_user_need.store(true, Ordering::SeqCst);
                self.plain(
                    "No user is available to answer in this run (headless). Do not ask again: make a reasonable assumption, or stop and state exactly what is blocked.",
                    true,
                    ToolStatus::SoftError,
                )
            }
            (Some(UserReply::Answers(a)), NeedsUser::AskUserQuestion { .. }) => {
                self.emit(task, agent, Some(turn), resolved("allow", "user", None));
                task.pending_user_need.store(false, Ordering::SeqCst);
                self.plain(json!({"answers": a}).to_string(), false, ToolStatus::Ok)
            }
            (
                Some(UserReply::Plan {
                    approved: true,
                    mode,
                    ..
                }),
                NeedsUser::ExitPlanMode { .. },
            ) => {
                self.emit(task, agent, Some(turn), resolved("allow", "user", None));
                let cur = *lock(&agent.mode);
                if let Some(m) = mode.or((cur == Mode::Plan).then_some(Mode::Default)) {
                    *lock(&agent.mode) = m;
                    lock(&agent.perms).set_mode(m);
                }
                self.plain("approved", false, ToolStatus::Ok)
            }
            (
                Some(UserReply::Plan {
                    approved: false,
                    message,
                    ..
                }),
                NeedsUser::ExitPlanMode { .. },
            ) => {
                self.emit(
                    task,
                    agent,
                    Some(turn),
                    resolved("deny", "user", message.clone()),
                );
                self.plain(
                    message.unwrap_or_else(|| "kept planning".into()),
                    true,
                    ToolStatus::SoftError,
                )
            }
            (Some(_), _) => {
                self.emit(
                    task,
                    agent,
                    Some(turn),
                    resolved("deny", "user", Some("mismatched reply".into())),
                );
                self.plain(
                    "The reply did not match the request.",
                    true,
                    ToolStatus::SoftError,
                )
            }
        }
    }

    /// Runs a child agent with its own history, now (the result is the tool result) or in the
    /// background (`run_in_background`: the result arrives later as a message). Boxed: the loop
    /// calls back into this function.
    fn run_subagent<'a>(
        &'a self,
        task: &'a TaskRun,
        parent: &'a AgentCtx,
        turn: &'a str,
        call_id: &'a str,
        input: Value,
    ) -> crate::traits::BoxFut<'a, Res> {
        Box::pin(async move {
            const DEPTH_LIMIT: u32 = 2;
            if parent.depth >= DEPTH_LIMIT {
                return self.plain(
                    "Task: subagent nesting depth limit reached.",
                    true,
                    ToolStatus::Error,
                );
            }
            let kind = input
                .get("subagent_type")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let Some(def) = self.agents.get(kind) else {
                return self.plain(
                    format!("Task: unknown subagent type \"{kind}\"."),
                    true,
                    ToolStatus::Error,
                );
            };
            let prompt = input
                .get("prompt")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let background = input
                .get("run_in_background")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let (child, mut st) = match self.new_child(
                task,
                parent,
                def,
                input.get("name").and_then(Value::as_str),
                prompt,
            ) {
                Ok(c) => c,
                Err(e) => return self.plain(e, true, ToolStatus::Error),
            };
            self.emit(
                task,
                &child,
                Some(turn),
                Event::SubagentStarted(synthesis_events::SubagentInfo {
                    agent_id: child.agent_id.clone().unwrap_or_default(),
                    agent_type: def.name.clone(),
                    tool_call_id: Some(call_id.to_string()),
                    reason: None,
                    name: Some(child.name.clone()),
                    background,
                }),
            );
            let first = (turn.to_string(), call_id.to_string());
            if background {
                let info = json!({
                    "agent_id": child.agent_id,
                    "name": child.name,
                    "kind": def.name,
                    "status": "running",
                });
                let job = crate::hub::Job {
                    name: child.name.clone(),
                    agent_id: child.agent_id.clone().unwrap_or_default(),
                    kind: def.name.clone(),
                    parent: parent.name.clone(),
                    retained: crate::hub::Retained {
                        depth: child.depth,
                        mode: *lock(&child.mode),
                        perms: child.perms.clone(),
                        allowed: child.allowed.clone(),
                        prompt: child.prompt.clone(),
                        messages: std::mem::take(&mut st.messages),
                    },
                    cancel: child.cancel.clone(),
                    first: Some(first),
                };
                return if self.hub.spawn(job) {
                    self.plain(info.to_string(), false, ToolStatus::Ok)
                } else {
                    self.hub.set_status(&child.name, crate::hub::CANCELLED);
                    self.plain(
                        "Task: background agents need a running task.",
                        true,
                        ToolStatus::Error,
                    )
                };
            }
            let end = self.run_agent(task, &child, &mut st).await;
            let (text, is_error, _, status) = self
                .conclude(
                    task,
                    &child,
                    &def.name,
                    Some(&first),
                    Some(turn),
                    false,
                    None,
                    st.messages,
                    end,
                )
                .await;
            let content = if is_error {
                Value::String(text.clone())
            } else {
                Value::String(wrap_tool_output("Task", &text))
            };
            Res {
                content,
                text,
                is_error,
                status,
                unavailable: false,
            }
        })
    }
}

/// Tool-specific advice appended to a denied `Bash` call: the dedicated tool that does the job
/// without a shell trick. Keyed on the command text; advice only, never a permission decision.
fn bash_hint(tool: &str, input: &Value) -> &'static str {
    let cmd = input.get("command").and_then(Value::as_str).unwrap_or("");
    if tool != "Bash" {
        return "";
    }
    let stripped = cmd
        .replace("2>&1", "")
        .replace("&>/dev/null", "")
        .replace(">/dev/null", "");
    let inline = ["python -c", "python3 -c", "node -e", "perl -e", "ruby -e"];
    if cmd.contains("<<")
        || stripped.contains('>')
        || cmd.contains("tee ")
        || cmd.contains("sed -i")
        || inline.iter().any(|i| cmd.contains(i))
    {
        " Use the Write/Edit tool for file changes instead of redirects, heredocs, tee, sed -i or inline interpreters; report blocked only if no dedicated tool can do it."
    } else if cmd.contains("-exec")
        || cmd.trim_start().starts_with("for ")
        || cmd.contains("| xargs")
    {
        " Use the Read, Grep or Glob tools to read and search files instead of find -exec, loops or pipelines; report blocked only if no dedicated tool can do it."
    } else {
        ""
    }
}

#[cfg(test)]
mod hint_tests {
    use super::bash_hint;
    use serde_json::json;

    #[test]
    fn hints_follow_the_denied_pattern() {
        let h = |c: &str| bash_hint("Bash", &json!({ "command": c }));
        assert!(h("cat > f <<EOF\nx\nEOF").contains("Write/Edit"));
        assert!(h("python3 -c 'open(1)'").contains("Write/Edit"));
        assert!(h("find . -exec cat {} +").contains("Read, Grep"));
        assert!(h("for f in *; do cat $f; done").contains("Read, Grep"));
        assert_eq!(h("cargo test 2>&1"), "");
        assert_eq!(bash_hint("Write", &json!({ "command": "a > b" })), "");
    }
}
