//! Declarative command hooks (port of Node `src/native/hooks.js`): the Claude Code hooks JSON
//! contract (payload on stdin, exit code 2 blocks, JSON on stdout) so existing hook entries work.
//!
//! - Config types mirror the `hooks` section of settings.json; loading that file is not here.
//! - Commands run through `synthesis_terminal::TerminalRuntime::exec` (resolved shell, timeout, cancel,
//!   process-tree kill). The payload goes to the child's stdin (`ExecSpec::stdin`) and stderr is
//!   captured apart from stdout (`StderrMode::Separate`): exit 2 uses stderr, JSON uses stdout.
//! - The crate reads no process env: the child env is the injected map (the session env of the
//!   runtime) plus `SYNTHESIS_PROJECT_DIR` / `CLAUDE_PROJECT_DIR` = cwd.
//! - Fail closed (deviation from Node, which fails open everywhere): for `PreToolUse` and
//!   `UserPromptSubmit` a hook that times out, fails to start, exits with anything but 0 or 2, is
//!   killed, or prints malformed JSON BLOCKS with the reason (`HookOutcome::failed_closed`).
//!   Opt out per hook with `"fail_open": true`. Cancellation is not a failure. Other events
//!   keep Node's fail-open behaviour. Default timeout: 60 s for those two, 600 s otherwise.
//! - Hook output never grants permission: see [`HookOutcome::hook_decision`].
//! - Not ported: `stripSecretEnv` (the injected env is already stripped), `updatedInput` that is a
//!   non-object truthy JS value (only JSON objects count).

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use synthesis_core::WorkspaceContext;
use synthesis_terminal::{ExecSpec, StderrMode, TerminalRuntime};
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

/// Events Node fires (`hooks.run(...)` call sites in agent.js). Config keys are these names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HookEvent {
    PreToolUse,
    PostToolUse,
    UserPromptSubmit,
    Stop,
    SubagentStop,
    SessionStart,
    SessionEnd,
    Notification,
    PreCompact,
}

impl HookEvent {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::PreToolUse => "PreToolUse",
            Self::PostToolUse => "PostToolUse",
            Self::UserPromptSubmit => "UserPromptSubmit",
            Self::Stop => "Stop",
            Self::SubagentStop => "SubagentStop",
            Self::SessionStart => "SessionStart",
            Self::SessionEnd => "SessionEnd",
            Self::Notification => "Notification",
            Self::PreCompact => "PreCompact",
        }
    }
}

/// One hook entry. Unknown fields (Node's `source`) are ignored.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct HookCommand {
    #[serde(default, rename = "type")]
    pub kind: String,
    #[serde(default)]
    pub command: String,
    /// Seconds; absent or 0 means [`default_timeout_secs`] for the event.
    #[serde(default)]
    pub timeout: Option<u64>,
    /// Failures of this hook do not block `PreToolUse` / `UserPromptSubmit` (default false).
    #[serde(default)]
    pub fail_open: bool,
    /// Permission-rule filter (`Bash(git *)`): the hook runs only for a matching tool call.
    #[serde(default, rename = "if")]
    pub condition: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct HookGroup {
    /// Full-match regex on the matcher target; absent, empty or `*` match everything.
    #[serde(default)]
    pub matcher: Option<String>,
    #[serde(default)]
    pub hooks: Vec<HookCommand>,
}

/// The `hooks` object of settings.json: event name to groups.
pub type HooksConfig = BTreeMap<String, Vec<HookGroup>>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Decision {
    Allow,
    Ask,
    Deny,
}

impl Decision {
    fn priority(self) -> u8 {
        match self {
            Self::Allow => 1,
            Self::Ask => 2,
            Self::Deny => 3,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HookRun {
    pub command: String,
    /// None on timeout, cancel, kill by signal or spawn failure.
    pub exit_code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    pub ms: u64,
    pub timed_out: bool,
    pub cancelled: bool,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct HookOutcome {
    pub blocked: bool,
    pub reason: Option<String>,
    /// What the hooks asked for: `blocked` forces Deny; otherwise the strictest of deny > ask >
    /// allow. ADVISORY ONLY: `Allow` does not bypass policy. The harness must still call
    /// `Permissions::check` (and `recheck` on `updated_input`) and may only use Deny/Ask to
    /// tighten the result.
    pub hook_decision: Option<Decision>,
    /// Blocked because a hook failed (see crate docs), not because a hook asked for it.
    pub failed_closed: bool,
    pub updated_input: Option<Value>,
    /// True when `updated_input` is set: the harness MUST re-run permission checks on it.
    pub input_rewritten: bool,
    pub additional_context: Vec<String>,
    pub stop: bool,
    pub stop_reason: Option<String>,
    pub results: Vec<HookRun>,
    /// Config problems found while matching (an invalid matcher regex).
    pub warnings: Vec<String>,
}

pub struct Hooks {
    runtime: Arc<TerminalRuntime>,
    session_id: String,
    workspace: Arc<WorkspaceContext>,
    config: HooksConfig,
    /// Added to the runtime session env for hook children (SYNTHESIS_* names only; see crate docs).
    env: BTreeMap<String, String>,
    pub transcript_path: String,
    pub disabled: bool,
}

/// Claude Code semantics: letters, digits, `_` and `|` are an exact list of names; anything else
/// is an unanchored regex. An invalid pattern MATCHES (fail closed: a guard hook with a typo in
/// its matcher must still run); `run` reports it in `HookOutcome::warnings`.
pub fn matcher_matches(pattern: Option<&str>, value: Option<&str>) -> bool {
    let Some(p) = pattern.filter(|p| !p.is_empty() && *p != "*") else {
        return true;
    };
    if p.chars()
        .all(|c| c.is_alphanumeric() || c == '_' || c == '|')
    {
        return p.split('|').any(|n| Some(n) == value);
    }
    Regex::new(p).map_or(true, |re| re.is_match(value.unwrap_or("")))
}

/// A matcher that is a regex but does not compile.
pub fn matcher_invalid(pattern: Option<&str>) -> bool {
    pattern.is_some_and(|p| {
        !p.chars().all(|c| c.is_alphanumeric() || c == '_' || c == '|') && Regex::new(p).is_err()
    })
}

/// The per-hook `if` field: `Tool` or `Tool(spec)` against the call in `payload`. A hook with
/// an `if` never runs for an event that has no tool call.
fn condition_holds(cond: &str, payload: &Value) -> bool {
    let Some(tool) = payload.get("tool_name").and_then(Value::as_str) else {
        return false;
    };
    let (name, spec) = match cond.strip_suffix(')').and_then(|c| c.split_once('(')) {
        Some((n, s)) => (n, Some(s)),
        None => (cond, None),
    };
    if name != tool {
        return false;
    }
    let Some(spec) = spec else { return true };
    let input = payload.get("tool_input");
    let arg = ["command", "file_path", "path", "pattern", "url"]
        .iter()
        .find_map(|k| input.and_then(|i| i.get(k)).and_then(Value::as_str))
        .unwrap_or("");
    if tool == "Bash" {
        // the permission engine's matcher: every segment, wrappers unwrapped (`cd r && git push`,
        // `time git push`, `FOO=1 git push`, `sh -c 'git push'`); unparsable counts as a match
        return synthesis_permissions::bash::bash_any_match(&[format!("Bash({spec})")], arg);
    }
    // `git:*` and `git *` are the prefix rule `git`
    let spec = spec
        .strip_suffix(":*")
        .map_or(spec.to_string(), |p| format!("{p}*"));
    if let Some(p) = spec.strip_suffix(" *").filter(|p| !p.contains('*')) {
        return arg == p || arg.starts_with(&format!("{p} "));
    }
    synthesis_permissions::bash::glob_match(&spec, arg, false)
}

impl Hooks {
    /// `session_id` must be open on `runtime` (its env is the base env of the children).
    pub fn new(
        runtime: Arc<TerminalRuntime>,
        session_id: impl Into<String>,
        workspace: Arc<WorkspaceContext>,
        config: HooksConfig,
    ) -> Self {
        Self {
            runtime,
            session_id: session_id.into(),
            workspace,
            config,
            env: BTreeMap::new(),
            transcript_path: String::new(),
            disabled: false,
        }
    }

    /// Extra child env; names outside `SYNTHESIS_*` are dropped.
    pub fn with_env(mut self, env: BTreeMap<String, String>) -> Self {
        self.env = env
            .into_iter()
            .filter(|(k, _)| k.starts_with("SYNTHESIS_"))
            .collect();
        self
    }

    pub async fn run(
        &self,
        event: HookEvent,
        payload: Value,
        matcher_target: Option<&str>,
        cancel: &CancellationToken,
    ) -> HookOutcome {
        let mut out = HookOutcome::default();
        let Some(groups) = self.config.get(event.as_str()).filter(|_| !self.disabled) else {
            return out;
        };
        // Dedupe by (matcher, command): the same entry merged in twice runs once.
        let mut seen = std::collections::HashSet::new();
        let mut commands = Vec::new();
        // the matcher does not apply to events that have no tool or source to match
        let matchless = matches!(event, HookEvent::UserPromptSubmit | HookEvent::Stop);
        for g in groups {
            if !matchless && matcher_invalid(g.matcher.as_deref()) {
                out.warnings.push(format!(
                    "hook matcher {:?} is not a valid regex: treated as matching",
                    g.matcher.as_deref().unwrap_or("")
                ));
            }
            if !matchless && !matcher_matches(g.matcher.as_deref(), matcher_target) {
                continue;
            }
            for h in &g.hooks {
                if h.condition
                    .as_deref()
                    .is_some_and(|c| !condition_holds(c, &payload))
                {
                    continue;
                }
                if h.kind == "command"
                    && !h.command.is_empty()
                    && seen.insert((g.matcher.clone().unwrap_or_default(), h.command.clone()))
                {
                    commands.push(h);
                }
            }
        }
        if commands.is_empty() {
            return out;
        }

        let cwd = self.workspace.cwd.to_string_lossy().into_owned();
        let mut stdin = match payload {
            Value::Object(m) => m,
            _ => Map::new(),
        };
        stdin.insert("session_id".into(), json!(self.session_id));
        stdin.insert("transcript_path".into(), json!(self.transcript_path));
        stdin.insert("cwd".into(), json!(cwd));
        stdin.insert("hook_event_name".into(), json!(event.as_str()));
        let stdin = Value::Object(stdin).to_string();

        let mut env = self.env.clone();
        env.insert("CLAUDE_PROJECT_DIR".into(), cwd.clone());
        env.insert("SYNTHESIS_PROJECT_DIR".into(), cwd);

        let mut set = JoinSet::new();
        for (i, h) in commands.iter().enumerate() {
            let (rt, sid) = (self.runtime.clone(), self.session_id.clone());
            let (command, env, stdin, cancel) = (
                h.command.clone(),
                env.clone(),
                stdin.clone(),
                cancel.clone(),
            );
            let secs = h
                .timeout
                .filter(|t| *t > 0)
                .unwrap_or(default_timeout_secs(event));
            set.spawn(async move {
                (
                    i,
                    run_command(&rt, &sid, command, env, stdin, secs, &cancel).await,
                )
            });
        }
        let mut raw: Vec<Option<HookRun>> = vec![None; commands.len()];
        while let Some(Ok((i, r))) = set.join_next().await {
            raw[i] = Some(r);
        }

        let mut decision_reason = None;
        let closed_event = matches!(event, HookEvent::PreToolUse | HookEvent::UserPromptSubmit);
        for (r, h) in raw.into_iter().zip(&commands) {
            let Some(r) = r else { continue };
            let closed = closed_event && !h.fail_open;
            apply(&mut out, &mut decision_reason, event, &r, closed);
            out.results.push(r);
        }
        if out.blocked {
            out.hook_decision = Some(Decision::Deny);
        } else {
            out.reason = decision_reason;
        }
        out.input_rewritten = out.updated_input.is_some();
        out
    }
}

/// Default hook timeout: 60 s for the gating events (PreToolUse, UserPromptSubmit), which fail
/// closed and so hold the turn; 600 s (Claude Code's default) for the rest (owner decision D3).
/// A per-hook `timeout` always wins.
pub fn default_timeout_secs(event: HookEvent) -> u64 {
    if matches!(event, HookEvent::PreToolUse | HookEvent::UserPromptSubmit) {
        60
    } else {
        600
    }
}

/// Folds one finished hook into the merged outcome (Node `run()` result loop).
fn apply(
    out: &mut HookOutcome,
    decision_reason: &mut Option<String>,
    event: HookEvent,
    r: &HookRun,
    closed: bool,
) {
    let fail = |out: &mut HookOutcome, why: String| {
        if closed && !r.cancelled {
            out.blocked = true;
            out.failed_closed = true;
            out.reason
                .get_or_insert(format!("hook failed ({why}): {}", r.command));
        }
    };
    if r.timed_out {
        fail(out, "timed out".into());
        return;
    }
    if r.exit_code == Some(2) {
        out.blocked = true;
        if out.reason.is_none() {
            let e = r.stderr.trim();
            out.reason = Some(if e.is_empty() { "blocked by hook" } else { e }.into());
        }
        return;
    }
    if r.exit_code != Some(0) {
        let e = r.stderr.trim();
        let code = r
            .exit_code
            .map_or("no exit code".into(), |c| format!("exit code {c}"));
        fail(
            out,
            if e.is_empty() {
                code
            } else {
                format!("{code}: {e}")
            },
        );
        return;
    }
    let text = r.stdout.trim();
    if text.is_empty() {
        return;
    }
    if !text.starts_with('{') {
        if matches!(event, HookEvent::UserPromptSubmit | HookEvent::SessionStart) {
            out.additional_context.push(text.into());
        }
        return;
    }
    let Ok(j) = serde_json::from_str::<Value>(text) else {
        fail(out, "invalid JSON output".into());
        return;
    };
    let non_empty = |v: Option<&Value>| {
        v.and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(String::from)
    };
    fn set_decision(
        out: &mut HookOutcome,
        dr: &mut Option<String>,
        d: Decision,
        reason: Option<String>,
    ) {
        if out
            .hook_decision
            .is_none_or(|cur| d.priority() > cur.priority())
        {
            out.hook_decision = Some(d);
            *dr = reason;
        }
    }
    if j.get("continue") == Some(&Value::Bool(false)) {
        out.stop = true;
        out.stop_reason = non_empty(j.get("stopReason")).or(out.stop_reason.take());
    }
    if j.get("decision").and_then(Value::as_str) == Some("block")
        && let Some(reason) = non_empty(j.get("reason"))
    {
        out.blocked = true;
        out.reason.get_or_insert(reason);
    }
    if j.get("decision").and_then(Value::as_str) == Some("approve") {
        set_decision(out, decision_reason, Decision::Allow, None);
    }
    if let Some(h) = j.get("hookSpecificOutput").filter(|h| h.is_object()) {
        let d = match h.get("permissionDecision").and_then(Value::as_str) {
            Some("deny") => Some(Decision::Deny),
            Some("ask") => Some(Decision::Ask),
            Some("allow") => Some(Decision::Allow),
            _ => None,
        };
        if let Some(d) = d {
            set_decision(
                out,
                decision_reason,
                d,
                h.get("permissionDecisionReason")
                    .and_then(Value::as_str)
                    .map(String::from),
            );
        }
        if h.get("updatedInput").is_some_and(Value::is_object) {
            out.updated_input = h.get("updatedInput").cloned();
        }
        out.additional_context
            .extend(non_empty(h.get("additionalContext")));
    }
    out.additional_context
        .extend(non_empty(j.get("additionalContext")));
}

async fn run_command(
    rt: &TerminalRuntime,
    sid: &str,
    command: String,
    env: BTreeMap<String, String>,
    stdin: String,
    timeout_secs: u64,
    cancel: &CancellationToken,
) -> HookRun {
    let t0 = Instant::now();
    let fail = |msg: String| HookRun {
        command: command.clone(),
        exit_code: None,
        stdout: String::new(),
        stderr: msg,
        ms: t0.elapsed().as_millis() as u64,
        timed_out: false,
        cancelled: false,
    };
    let mut spec = ExecSpec::new(command.clone());
    spec.env = env;
    spec.stdin = Some(stdin.into_bytes());
    spec.stderr = StderrMode::Separate;
    spec.timeout = Duration::from_secs(timeout_secs);
    spec.max_output_bytes = 4 << 20;
    spec.max_output_lines = usize::MAX;
    match rt.exec(sid, spec, cancel).await {
        Err(e) => fail(e.to_string()),
        Ok(r) => HookRun {
            command,
            exit_code: if r.timed_out || r.cancelled {
                None
            } else {
                r.exit_code
            },
            stdout: r.output,
            stderr: r.stderr,
            ms: t0.elapsed().as_millis() as u64,
            timed_out: r.timed_out,
            cancelled: r.cancelled,
        },
    }
}

#[cfg(all(test, unix))]
mod tests;
