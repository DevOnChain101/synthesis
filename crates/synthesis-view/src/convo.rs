//! The conversation of one session as the window shows it: a reducer over the typed event stream
//! (`synthesis-events`), the same events `synt run` writes as JSONL. Pure state, no GPUI.

use serde_json::Value;
use synthesis_events::{Envelope, Event, PeerAddr, TaskStatus, ToolStatus};
use synthesis_harness::sanitize_for_display;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolState {
    Running,
    Ok,
    Failed,
    NotRun,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckState {
    Running,
    Passed,
    Failed,
    Unverified,
    Blocked,
}

impl CheckState {
    fn of(status: &str) -> Self {
        match status {
            "passed" => Self::Passed,
            "failed" => Self::Failed,
            "blocked" => Self::Blocked,
            _ => Self::Unverified,
        }
    }
}

/// One check of the completion gate (`verification.check.*`).
#[derive(Debug, Clone, PartialEq)]
pub struct CheckRow {
    pub id: String,
    pub name: String,
    pub command: Option<String>,
    pub state: CheckState,
    pub duration_ms: u64,
    pub exit_code: Option<i64>,
    /// The end of the check's output (capped by the producer).
    pub tail: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApprovalState {
    Pending,
    Allowed,
    Denied,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Item {
    User(String),
    Assistant {
        text: String,
        streaming: bool,
    },
    Tool {
        id: String,
        name: String,
        summary: String,
        state: ToolState,
        preview: String,
        /// The result text (whole up to 64 KiB); while a command runs, its live output.
        output: String,
        duration_ms: u64,
        /// The file a Write/Edit-style tool changes (an evidence card with its diff).
        file: Option<String>,
        /// Structured line diff of that change (`ToolCompleted.diff`).
        diff: Option<Value>,
        /// What the call was given (`tool.requested.input`): the card shapes by it. `Null` in old logs.
        input: Box<Value>,
        /// How the call ended (`tool.completed.status`); None while it runs.
        status: Option<ToolStatus>,
        /// Exit code from `process.exited` of a foreground command (structured, not read from the text).
        exit_code: Option<i64>,
    },
    /// The model's reasoning; the window shows it only with `show_thinking`.
    Thinking(String),
    /// A background sub-agent (foreground ones show as their Task row).
    Agent {
        id: String,
        kind: String,
        running: bool,
    },
    /// How a task ended: `ok` for completed, else the status word (`failed`, `cancelled`,
    /// `blocked`, `unverified`, `ended`) and the reason; `checks` is the last verification report
    /// of that task (one check per line), kept only for a task that did not complete.
    TaskEnd {
        ok: bool,
        text: String,
        word: String,
        checks: String,
    },
    /// A verification report: `status` is `passed`, `failed`, ... and `summary` one check per line.
    Verification {
        status: String,
        summary: String,
    },
    /// The gate's checks as they run: one row per check. `done` is the report status once
    /// `verification.completed` closed the card (`ended` when the task ended first); `total` is
    /// how many the evaluation planned. Old sessions have no such events and show `Verification`.
    Checks {
        rows: Vec<CheckRow>,
        total: usize,
        done: Option<String>,
        summary: String,
    },
    Approval {
        id: String,
        tool_call_id: String,
        prompt: String,
        state: ApprovalState,
    },
    Notice {
        warning: bool,
        text: String,
    },
    /// An internal row (memory, peer and hook bookkeeping): drawn only with `synt.cli.verbose` (CLI) or `synt.tui.verbose` (desktop).
    Internal(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StepState {
    Done,
    Active,
    Todo,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Step {
    pub text: String,
    pub state: StepState,
}

#[derive(Debug, Default, Clone)]
pub struct Convo {
    pub items: Vec<Item>,
    /// `items[i]` came at `times[i]` (epoch ms); shorter than `items` when rows were added locally.
    pub times: Vec<Option<i64>>,
    /// A task is between `task.started` and `task.ended`.
    pub running: bool,
    pub tokens: u64,
    pub cost_usd: f64,
    /// The mission plan: the agent's todo list, latest version.
    pub plan: Vec<Step>,
    /// When the plan became all done (epoch ms); `None` while a task is open.
    pub plan_done_at: Option<i64>,
    /// Sub-agents that started and have not stopped (`agent_type`).
    pub agents: Vec<(String, String)>,
    /// Turns of the task in progress (for its end line).
    turns: u32,
    started_ms: Option<i64>,
    /// `tokens` when the task in progress started.
    task_tokens0: u64,
    /// The session's latest status (`session.status_changed` `to`: `running`, `verifying`, `waiting_approval`...).
    pub session_status: String,
    /// Summary of the last verification report of the task in progress.
    checks: Option<String>,
}

/// The file a Write/Edit-style tool call changes.
pub fn changed_file(name: &str, input: &Value) -> Option<String> {
    matches!(name, "Write" | "Edit" | "MultiEdit" | "NotebookEdit")
        .then(|| {
            input
                .get("file_path")
                .or_else(|| input.get("notebook_path"))
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .flatten()
}

/// A peer as a person knows it: its session name, else its workspace.
fn peer_name(a: &PeerAddr) -> String {
    sanitize_for_display(a.session_name.as_deref().unwrap_or(&a.workspace_label))
}

/// One line of a peer-supplied text, safe to draw.
fn peer_text(s: &str) -> String {
    sanitize_for_display(&crate::text::short(s, 100))
}

/// One line for a tool row: the command, the path, or the start of the JSON input.
pub fn summarize_input(name: &str, input: &Value) -> String {
    let s = |k: &str| input.get(k).and_then(Value::as_str);
    let text = match name {
        "Bash" => s("command"),
        _ => s("file_path")
            .or_else(|| s("path"))
            .or_else(|| s("pattern")),
    }
    .map_or_else(|| input.to_string(), str::to_string);
    text.lines()
        .next()
        .unwrap_or("")
        .chars()
        .take(120)
        .collect()
}

fn text_blocks(content: &[Value]) -> String {
    content
        .iter()
        .filter(|b| b.get("type").and_then(Value::as_str) == Some("text"))
        .filter_map(|b| b.get("text").and_then(Value::as_str))
        .collect::<Vec<_>>()
        .join("\n")
}

impl Convo {
    pub fn from_events(events: &[Envelope]) -> Self {
        let mut c = Self::default();
        for e in events {
            c.apply(e);
        }
        c
    }

    pub fn clear_plan(&mut self) {
        self.plan.clear();
        self.plan_done_at = None;
    }

    /// The task list to show at `now` (see [`crate::lifecycle::mission_visible`]).
    pub fn mission(&self, now: i64) -> &[Step] {
        if crate::lifecycle::mission_visible(self.plan.len(), self.plan_done_at, now) {
            &self.plan
        } else {
            &[]
        }
    }

    /// When the task in progress started (epoch ms).
    pub fn task_started_ms(&self) -> Option<i64> {
        self.started_ms.filter(|_| self.running)
    }

    /// Tokens the task in progress has used so far (from the usage of its finished turns).
    pub fn task_tokens(&self) -> u64 {
        self.tokens.saturating_sub(self.task_tokens0)
    }

    /// The tool that is running now, if one is.
    pub fn running_tool(&self) -> Option<&str> {
        self.items.iter().rev().find_map(|i| match i {
            Item::Tool {
                state: ToolState::Running,
                name,
                ..
            } => Some(name.as_str()),
            _ => None,
        })
    }

    /// What the agent is doing right now, for the activity line (None when no task runs).
    pub fn activity(&self) -> Option<String> {
        if !self.running {
            return None;
        }
        if let Some((_, t)) = self.agents.last() {
            return Some(format!("{t} agent is working"));
        }
        let tool = self.items.iter().rev().find_map(|i| match i {
            Item::Tool {
                state: ToolState::Running,
                name,
                summary,
                ..
            } => Some(format!("Synthesis is running {name}  {summary}")),
            _ => None,
        });
        Some(tool.unwrap_or_else(|| "Synthesis is thinking".into()))
    }

    /// Approvals waiting for the person: the "needs you" count.
    pub fn needs_you(&self) -> usize {
        self.items
            .iter()
            .filter(|i| {
                matches!(
                    i,
                    Item::Approval {
                        state: ApprovalState::Pending,
                        ..
                    }
                )
            })
            .count()
    }

    fn notice(&mut self, warning: bool, text: String) {
        self.items.push(Item::Notice { warning, text });
    }

    fn internal(&mut self, text: String) {
        self.items.push(Item::Internal(text));
    }

    /// The latest check card that no report has closed yet.
    fn open_checks(&mut self) -> Option<&mut Item> {
        self.items
            .iter_mut()
            .rev()
            .find(|i| matches!(i, Item::Checks { .. }))
            .filter(|i| matches!(i, Item::Checks { done: None, .. }))
    }

    fn close_stream(&mut self) {
        if let Some(Item::Assistant { streaming, .. }) = self.items.last_mut() {
            *streaming = false;
        }
    }

    fn tool(&mut self, id: &str) -> Option<&mut Item> {
        self.items
            .iter_mut()
            .rev()
            .find(|i| matches!(i, Item::Tool { id: t, .. } if t == id))
    }

    pub fn apply(&mut self, e: &Envelope) {
        self.apply_event(e);
        // every row the event added carries the event's time (a row streamed into later keeps its first)
        let ts = crate::model::parse_ts(&e.ts);
        while self.times.len() < self.items.len() {
            self.times.push(ts);
        }
    }

    /// Epoch ms of row `i`, when it came from an event (a local notice has none).
    pub fn time_of(&self, i: usize) -> Option<i64> {
        self.times.get(i).copied().flatten()
    }

    fn apply_event(&mut self, e: &Envelope) {
        // sub-agent chatter is shown on the Activity page, not in the main conversation
        let main = e.agent_id.is_none();
        match &e.event {
            Event::TaskStarted { .. } => {
                self.running = true;
                self.turns = 0;
                self.checks = None;
                self.started_ms = crate::model::parse_ts(&e.ts);
                self.task_tokens0 = self.tokens;
            }
            Event::SessionStatusChanged { to, .. } if main => self.session_status = to.clone(),
            Event::TurnStarted { .. } if main => self.turns += 1,
            Event::TaskEnded {
                status,
                reason,
                usage,
                ..
            } => {
                self.running = false;
                self.close_stream();
                if let Some(Item::Checks { rows, done, .. }) = self.open_checks() {
                    // the task ended inside the gate: nothing is running any more
                    for r in rows.iter_mut().filter(|r| r.state == CheckState::Running) {
                        r.state = CheckState::Blocked;
                    }
                    *done = Some("ended".into());
                }
                let secs = crate::model::parse_ts(&e.ts)
                    .zip(self.started_ms)
                    .map_or(0, |(end, start)| ((end - start) / 1000).max(0));
                let checks = self.checks.take();
                if let Some((_, msg)) = model_error_parts(reason)
                    && matches!(self.items.last(), Some(Item::Notice { text, .. }) if text == msg)
                {
                    self.items.pop();
                }
                self.items.push(if *status == TaskStatus::Completed {
                    Item::TaskEnd {
                        ok: true,
                        text: format!(
                            "Done \u{b7} {} turn{} \u{b7} {secs}s",
                            self.turns,
                            if self.turns == 1 { "" } else { "s" }
                        ),
                        word: "completed".into(),
                        checks: String::new(),
                    }
                } else {
                    // wording of the CLI: a task that is not `completed` is never a success
                    let word = match status {
                        TaskStatus::Failed => "failed",
                        TaskStatus::Cancelled => "cancelled",
                        TaskStatus::Blocked => "blocked",
                        TaskStatus::Unverified => "unverified",
                        _ => "ended",
                    };
                    let text = if *status == TaskStatus::Cancelled && reason == "cancelled" {
                        "interrupted".into()
                    } else if let Some((kind, msg)) = model_error_parts(reason) {
                        // one line: the model-error notice just above says the same thing
                        let hint = if kind == "auth" {
                            " \u{2014} check your API key env"
                        } else {
                            ""
                        };
                        format!("model error ({kind}): {}{hint}", sanitize_for_display(msg))
                    } else if reason.is_empty() || reason == word {
                        format!("task {word}")
                    } else {
                        format!("task {word}: {}", sanitize_for_display(reason))
                    };
                    Item::TaskEnd {
                        ok: false,
                        text,
                        word: word.into(),
                        checks: checks.unwrap_or_default(),
                    }
                });
                self.tokens = self.tokens.max(usage.input_tokens + usage.output_tokens);
                self.cost_usd = self.cost_usd.max(usage.cost_usd.unwrap_or(0.0));
            }
            // an empty display text marks the harness's own follow-up note to the model
            Event::UserMessage { display_text, .. }
                if crate::text::is_harness_note(display_text.as_deref()) => {}
            Event::UserMessage {
                text, display_text, ..
            } if main => self.items.push(Item::User(
                display_text.clone().unwrap_or_else(|| text.clone()),
            )),
            Event::ModelDelta { kind, text, .. } if main && kind == "thinking" => {
                match self.items.last_mut() {
                    Some(Item::Thinking(t)) => t.push_str(text),
                    _ => self.items.push(Item::Thinking(text.clone())),
                }
            }
            Event::ModelDelta { kind, text, .. } if main && kind == "text" => {
                match self.items.last_mut() {
                    Some(Item::Assistant {
                        text: t,
                        streaming: true,
                    }) => t.push_str(text),
                    _ => self.items.push(Item::Assistant {
                        text: text.clone(),
                        streaming: true,
                    }),
                }
            }
            Event::AssistantMessage { content, .. } if main => {
                let full = text_blocks(content);
                match self.items.last_mut() {
                    Some(Item::Assistant { text, streaming }) if *streaming => {
                        *text = full;
                        *streaming = false;
                    }
                    _ if !full.is_empty() => self.items.push(Item::Assistant {
                        text: full,
                        streaming: false,
                    }),
                    _ => {}
                }
            }
            Event::TurnCompleted { usage: Some(u), .. } => {
                self.tokens += u.input_tokens + u.output_tokens;
                self.cost_usd += u.cost_usd.unwrap_or(0.0);
            }
            Event::TodoUpdated { todos } => {
                let first_open = todos.iter().position(|t| t.status != "completed");
                self.plan = todos
                    .iter()
                    .enumerate()
                    .map(|(i, t)| Step {
                        text: if t.status == "in_progress" && !t.active_form.is_empty() {
                            t.active_form.clone()
                        } else {
                            t.content.clone()
                        },
                        state: if t.status == "completed" {
                            StepState::Done
                        } else if t.status == "in_progress"
                            || (Some(i) == first_open && self.running)
                        {
                            StepState::Active
                        } else {
                            StepState::Todo
                        },
                    })
                    .collect();
                self.plan_done_at = (!self.plan.is_empty()
                    && self.plan.iter().all(|s| s.state == StepState::Done))
                .then(|| crate::model::parse_ts(&e.ts))
                .flatten();
            }
            Event::SubagentStarted(i) => {
                self.agents.push((i.agent_id.clone(), i.agent_type.clone()));
                if i.background {
                    self.items.push(Item::Agent {
                        id: i.agent_id.clone(),
                        kind: i.agent_type.clone(),
                        running: true,
                    });
                }
            }
            Event::SubagentStopped(i) => {
                self.agents.retain(|(id, _)| *id != i.agent_id);
                for it in &mut self.items {
                    if let Item::Agent { id, running, .. } = it
                        && *id == i.agent_id
                    {
                        *running = false;
                    }
                }
            }
            // unverified and blocked runs are said by the task end line (with these checks);
            // failed and passed are shown
            Event::VerificationCheckStarted {
                check_id,
                name,
                command,
                index,
                total,
            } if main => {
                let row = CheckRow {
                    id: check_id.clone(),
                    name: sanitize_for_display(name),
                    command: command.as_deref().map(sanitize_for_display),
                    state: CheckState::Running,
                    duration_ms: 0,
                    exit_code: None,
                    tail: String::new(),
                };
                match self.open_checks().filter(|_| *index > 0) {
                    Some(Item::Checks { rows, total: t, .. }) => {
                        rows.retain(|r| r.id != row.id);
                        rows.push(row);
                        *t = (*total as usize).max(rows.len());
                    }
                    _ => self.items.push(Item::Checks {
                        rows: vec![row],
                        total: *total as usize,
                        done: None,
                        summary: String::new(),
                    }),
                }
            }
            Event::VerificationCheckFinished {
                check_id,
                name,
                command,
                status,
                duration_ms,
                exit_code,
                output_tail,
            } if main => {
                let row = CheckRow {
                    id: check_id.clone(),
                    name: sanitize_for_display(name),
                    command: command.as_deref().map(sanitize_for_display),
                    state: CheckState::of(status),
                    duration_ms: *duration_ms,
                    exit_code: *exit_code,
                    tail: sanitize_for_display(output_tail),
                };
                match self.open_checks() {
                    Some(Item::Checks { rows, total, .. }) => {
                        match rows.iter_mut().find(|r| r.id == row.id) {
                            Some(r) => *r = row,
                            None => rows.push(row),
                        }
                        *total = (*total).max(rows.len());
                    }
                    // the start was not seen (a stream that begins mid-run)
                    _ => self.items.push(Item::Checks {
                        rows: vec![row],
                        total: 1,
                        done: None,
                        summary: String::new(),
                    }),
                }
            }
            Event::VerificationCompleted {
                status, summary, ..
            } if main => {
                self.checks = Some(summary.clone());
                if let Some(Item::Checks {
                    done, summary: s, ..
                }) = self.open_checks()
                {
                    *done = Some(status.clone());
                    *s = summary.clone();
                } else if matches!(status.as_str(), "passed" | "failed") {
                    self.items.push(Item::Verification {
                        status: status.clone(),
                        summary: summary.clone(),
                    })
                }
            }
            Event::ToolRequested {
                tool_call_id,
                name,
                input,
                ..
            } if main => {
                self.close_stream();
                self.items.push(Item::Tool {
                    id: tool_call_id.clone(),
                    name: name.clone(),
                    summary: summarize_input(name, input),
                    state: ToolState::Running,
                    preview: String::new(),
                    output: String::new(),
                    duration_ms: 0,
                    file: changed_file(name, input),
                    diff: None,
                    input: Box::new(input.clone()),
                    status: None,
                    exit_code: None,
                });
            }
            Event::ToolCompleted {
                tool_call_id,
                status,
                not_run,
                preview,
                output: full,
                duration_ms: ms,
                diff: d,
                ..
            } => {
                if let Some(Item::Tool {
                    state,
                    preview: p,
                    output,
                    duration_ms,
                    diff,
                    status: st,
                    ..
                }) = self.tool(tool_call_id)
                {
                    *st = Some(*status);
                    if let Some(f) = full {
                        output.clone_from(f);
                    }
                    *duration_ms = *ms;
                    diff.clone_from(d);
                    *state = if *not_run {
                        ToolState::NotRun
                    } else if *status == ToolStatus::Ok {
                        ToolState::Ok
                    } else {
                        ToolState::Failed
                    };
                    p.clone_from(preview);
                }
            }
            // live output of a running command; the completed event replaces it with the result
            Event::ToolOutput {
                tool_call_id, text, ..
            } => {
                if let Some(Item::Tool {
                    output,
                    state: ToolState::Running,
                    ..
                }) = self.tool(tool_call_id)
                {
                    output.push_str(text);
                }
            }
            Event::ToolApprovalRequired {
                approval_id,
                tool_call_id,
                prompt,
                ..
            } => self.items.push(Item::Approval {
                id: approval_id.clone(),
                tool_call_id: tool_call_id.clone(),
                prompt: prompt.clone(),
                state: ApprovalState::Pending,
            }),
            Event::ToolApprovalResolved {
                approval_id,
                decision,
                ..
            } => {
                for i in &mut self.items {
                    if let Item::Approval { id, state, .. } = i
                        && id == approval_id
                    {
                        *state = if decision == "allow" {
                            ApprovalState::Allowed
                        } else {
                            ApprovalState::Denied
                        };
                    }
                }
            }
            Event::ModelError { message, .. } => self.items.push(Item::Notice {
                warning: true,
                text: message.clone(),
            }),
            Event::Notice { level, text } => self.items.push(Item::Notice {
                warning: level != "info",
                text: text.clone(),
            }),
            // The rows below are the CLI's wording (`synthesis-cli-ui` chat.rs): what a person
            // must see is a notice, bookkeeping is an `Internal` row (verbose only).
            Event::BudgetExceeded {
                budget,
                limit,
                used,
            } => self.notice(true, format!("budget exceeded: {budget} {used} / {limit}")),
            Event::HookRan {
                event,
                hook_name,
                outcome,
                ..
            } => {
                let t = sanitize_for_display(&format!("hook {hook_name} ({event}): {outcome}"));
                if outcome == "success" {
                    self.internal(t)
                } else {
                    self.notice(true, t)
                }
            }
            Event::BrowserLoginRequired { origin, .. } => self.notice(
                true,
                format!(
                    "log in to {} in the browser window, then confirm",
                    sanitize_for_display(origin)
                ),
            ),
            Event::MemoryProposalCreated { .. } => {
                self.notice(true, "a memory note is waiting for your review".into())
            }
            Event::MemoryReflectionFailed { error, .. } => self.notice(
                true,
                format!("memory reflection failed: {}", sanitize_for_display(error)),
            ),
            Event::MemoryRetrieved { ids, tokens, .. } => self.internal(format!(
                "memory: {} note(s) retrieved ({tokens} tokens)",
                ids.len()
            )),
            Event::MemoryWritten { op, kind, .. } => {
                self.internal(sanitize_for_display(&format!("memory: {kind} note {op}")))
            }
            Event::MemoryQuarantined { rule, .. } => self.internal(sanitize_for_display(&format!(
                "memory: a note was quarantined ({rule})"
            ))),
            Event::MemoryReflectionStarted { .. } => {
                self.internal("memory: reflection started".into())
            }
            Event::MemoryReflectionCompleted { ops, .. } => {
                self.internal(format!("memory: reflection saved {ops} note(s)"))
            }
            Event::PeerListening { .. } => self.internal("peers: listening for requests".into()),
            Event::PeerStopped { .. } => self.internal("peers: stopped listening".into()),
            Event::PeerRequestSent { to, summary, .. } => self.internal(format!(
                "peer request sent to {}: {}",
                peer_name(to),
                peer_text(summary)
            )),
            Event::PeerRequestReceived {
                from,
                summary,
                decision,
                ..
            } => {
                let t = format!(
                    "peer request {decision} from {}: {}",
                    peer_name(from),
                    peer_text(summary)
                );
                // an accepted ask is something the user must see; a refusal is bookkeeping
                if decision == "accepted" {
                    self.notice(false, t)
                } else {
                    self.internal(t)
                }
            }
            // the `<peer_reply>` block the model reads arrives with this event
            Event::PeerReply { status, text, .. } => self.notice(
                status != "answered",
                format!(
                    "peer reply: {}  {}",
                    sanitize_for_display(status),
                    // the whole answer, line by line (the front end clamps the card)
                    text.lines()
                        .map(sanitize_for_display)
                        .collect::<Vec<_>>()
                        .join("\n")
                ),
            ),
            Event::PeerThreadClosed { reason, .. } => self.internal(sanitize_for_display(
                &format!("peer thread closed ({reason})"),
            )),
            Event::PeerDelegationDenied { from, reason, .. } => self.notice(
                true,
                format!(
                    "peer request from {} refused: {}",
                    peer_name(from),
                    sanitize_for_display(reason)
                ),
            ),
            // work another session asked this one to do: always named, with who asked
            Event::PeerDelegationAccepted { from, mode, .. } => self.notice(
                false,
                format!(
                    "working for peer {} ({} mode)",
                    peer_name(from),
                    sanitize_for_display(mode)
                ),
            ),
            Event::PeerDelegationTool { name, from, .. } => self.internal(format!(
                "peer work: {} for {}",
                sanitize_for_display(name),
                sanitize_for_display(from)
            )),
            Event::PeerDelegationCompleted {
                status, tool_calls, ..
            } => self.internal(format!(
                "peer work {}: {tool_calls} tool call(s)",
                sanitize_for_display(status)
            )),
            Event::MessageSent {
                from,
                to,
                origin,
                kind,
                summary,
                text,
                ..
            } if origin != "user" => {
                let what = peer_text(summary.as_deref().unwrap_or(text));
                let (from, to) = (sanitize_for_display(from), sanitize_for_display(to));
                if origin == "system" {
                    self.notice(true, format!("\u{26a0} {what}"))
                } else if from == "main" {
                    self.notice(false, format!("\u{2192} @{to} {what}"))
                } else if to == "main" {
                    let label = if kind == "result" { " result:" } else { ":" };
                    self.notice(false, format!("\u{2190} @{from}{label} {what}"))
                } else {
                    self.notice(false, format!("@{from} \u{2192} @{to}: {what}"))
                }
            }
            // a foreground command's process id is its tool call id
            Event::ProcessExited(p) => {
                if let Some(Item::Tool { exit_code, .. }) = self.tool(&p.process_id) {
                    *exit_code = p.exit_code;
                }
            }
            Event::MessageDropped { reason, .. } => self.notice(
                true,
                format!("\u{26a0} message dropped: {}", sanitize_for_display(reason)),
            ),
            _ => {}
        }
    }
}

/// `model_error:<kind>: <message>` (the harness's task-end reason) as (kind, message).
fn model_error_parts(reason: &str) -> Option<(&str, &str)> {
    reason.strip_prefix("model_error:")?.split_once(": ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn ev(t: &str, payload: Value) -> Envelope {
        let mut v = json!({"v":1,"seq":1,"ts":"2026-01-01T00:00:00Z","session_id":"s","task_id":null,"turn_id":null,"type":t});
        v["payload"] = payload;
        serde_json::from_value(v).unwrap()
    }

    fn started(id: &str, index: u32, total: u32) -> Envelope {
        ev(
            "verification.check.started",
            json!({"check_id":id,"name":id,"command":"cargo test","index":index,"total":total}),
        )
    }

    fn finished(id: &str, status: &str) -> Envelope {
        ev(
            "verification.check.finished",
            json!({"check_id":id,"name":id,"status":status,"duration_ms":40,"exit_code":1,"output_tail":"boom"}),
        )
    }

    fn card(c: &Convo) -> (&[CheckRow], Option<&str>) {
        match c.items.last() {
            Some(Item::Checks { rows, done, .. }) => (rows, done.as_deref()),
            i => panic!("{i:?}"),
        }
    }

    #[test]
    fn check_progress_fills_one_card_that_the_report_closes() {
        let mut c = Convo::default();
        c.apply(&started("lint", 0, 2));
        assert_eq!(card(&c).0[0].state, CheckState::Running);
        c.apply(&finished("lint", "failed"));
        c.apply(&started("test", 1, 2));
        let (rows, done) = card(&c);
        assert_eq!((rows.len(), done), (2, None));
        assert_eq!(rows[0].state, CheckState::Failed);
        assert_eq!(
            (rows[0].exit_code, rows[0].tail.as_str()),
            (Some(1), "boom")
        );
        assert_eq!(rows[1].state, CheckState::Running);
        c.apply(&finished("test", "passed"));
        c.apply(&ev(
            "verification.completed",
            json!({"report_id":"r","status":"failed","attempt":1,"summary":"1 failed"}),
        ));
        assert_eq!(c.items.len(), 1, "the card replaces the one-line report");
        assert_eq!(card(&c).1, Some("failed"));
        // the next attempt starts a new card
        c.apply(&started("lint", 0, 2));
        assert_eq!(c.items.len(), 2);
    }

    #[test]
    fn check_events_missing_or_cut_short_are_tolerated() {
        // finished without started, and a task that ends inside the gate
        let mut c = Convo::default();
        c.apply(&finished("lint", "passed"));
        assert_eq!(card(&c).0.len(), 1);
        c.apply(&started("test", 0, 1));
        c.apply(&ev(
            "task.ended",
            json!({"status":"cancelled","reason":"cancelled","usage":{"input_tokens":0,"output_tokens":0,"cache_read_input_tokens":0,"cache_creation_input_tokens":0},"changed_files":[],"artifacts_root":""}),
        ));
        let Some(Item::Checks { rows, done, .. }) = c
            .items
            .iter()
            .rev()
            .find(|i| matches!(i, Item::Checks { .. }))
        else {
            panic!()
        };
        assert_eq!(rows[0].state, CheckState::Blocked);
        assert_eq!(done.as_deref(), Some("ended"));
        // an old session: only the report, drawn as before
        let mut c = Convo::default();
        c.apply(&ev(
            "verification.completed",
            json!({"report_id":"r","status":"passed","attempt":1,"summary":"ok"}),
        ));
        assert!(matches!(c.items[0], Item::Verification { .. }));
    }

    #[test]
    fn every_row_keeps_the_time_of_its_event() {
        let mut c = Convo::default();
        c.apply(&ev("user.message", json!({"text": "hi", "images": []})));
        let mut e = ev("notice", json!({"level": "info", "text": "n"}));
        e.ts = "2026-01-01T00:00:07Z".into();
        c.apply(&e);
        assert_eq!(c.time_of(0), crate::model::parse_ts("2026-01-01T00:00:00Z"));
        assert_eq!(c.time_of(1), crate::model::parse_ts("2026-01-01T00:00:07Z"));
        assert_eq!(c.time_of(5), None, "a row added locally has none");
    }

    #[test]
    fn deltas_stream_into_one_assistant_item_and_the_final_message_replaces_them() {
        let mut c = Convo::default();
        for t in ["Hel", "lo"] {
            c.apply(&ev(
                "model.delta",
                json!({"block_index":0,"kind":"text","text":t}),
            ));
        }
        assert_eq!(
            c.items,
            vec![Item::Assistant {
                text: "Hello".into(),
                streaming: true
            }]
        );
        c.apply(&ev(
            "assistant.message",
            json!({"content":[{"type":"text","text":"Hello!"}],"stop_reason":"end_turn"}),
        ));
        assert_eq!(
            c.items,
            vec![Item::Assistant {
                text: "Hello!".into(),
                streaming: false
            }]
        );
    }

    #[test]
    fn tool_rows_follow_requested_then_completed() {
        let mut c = Convo::default();
        c.apply(&ev(
            "tool.requested",
            json!({"tool_call_id":"t1","name":"Bash","input":{"command":"ls -la\nmore"},"input_truncated":false}),
        ));
        assert!(
            matches!(&c.items[0], Item::Tool { summary, state: ToolState::Running, .. } if summary == "ls -la")
        );
        c.apply(&ev(
            "tool.completed",
            json!({"tool_call_id":"t1","name":"Bash","status":"ok","is_error":false,"not_run":false,"duration_ms":3,"preview":"done","bytes":4,"truncated":false}),
        ));
        assert!(
            matches!(&c.items[0], Item::Tool { state: ToolState::Ok, preview, .. } if preview == "done")
        );
    }

    #[test]
    fn approval_flow_pending_then_resolved_updates_needs_you() {
        let mut c = Convo::default();
        c.apply(&ev(
            "tool.approval_required",
            json!({"approval_id":"a1","tool_call_id":"t1","dialog":"permission","prompt":"Run ls?","options":[],"suggestions":[]}),
        ));
        assert_eq!(c.needs_you(), 1);
        c.apply(&ev(
            "tool.approval_resolved",
            json!({"approval_id":"a1","decision":"deny","reviewer":"user","input_changed":false}),
        ));
        assert_eq!(c.needs_you(), 0);
        assert!(matches!(
            &c.items[0],
            Item::Approval {
                state: ApprovalState::Denied,
                ..
            }
        ));
    }

    #[test]
    fn the_task_list_clears_5_minutes_after_all_done_or_when_replaced_or_resumed() {
        let list = |st: &str| {
            ev(
                "todo.updated",
                json!({"todos":[{"content":"A","status":st,"active_form":"A"}]}),
            )
        };
        let t0 = crate::model::parse_ts("2026-01-01T00:00:00Z").unwrap();
        let mut c = Convo::default();
        c.apply(&list("in_progress"));
        assert_eq!(
            c.mission(t0 + 3_600_000).len(),
            1,
            "open tasks never expire"
        );
        c.apply(&list("completed"));
        assert_eq!(c.mission(t0 + 299_999).len(), 1);
        assert!(c.mission(t0 + 300_000).is_empty());
        // new tasks replace the finished list at once
        c.apply(&list("pending"));
        assert_eq!(c.mission(t0 + 600_000).len(), 1);
        // a resumed session drops it, even an open one
        c.clear_plan();
        assert!(c.mission(t0).is_empty());
    }

    #[test]
    fn plan_agents_cost_verification_and_changed_files_feed_the_mission_view() {
        let mut c = Convo {
            running: true,
            ..Default::default()
        };
        c.apply(&ev(
            "todo.updated",
            json!({"todos":[
                {"content":"Read","status":"completed","active_form":"Reading"},
                {"content":"Write","status":"in_progress","active_form":"Writing"},
                {"content":"Test","status":"pending","active_form":"Testing"}]}),
        ));
        let s: Vec<_> = c.plan.iter().map(|s| (s.text.as_str(), s.state)).collect();
        assert_eq!(
            s,
            [
                ("Read", StepState::Done),
                ("Writing", StepState::Active),
                ("Test", StepState::Todo)
            ]
        );
        c.apply(&ev(
            "subagent.started",
            json!({"agent_id":"a1","agent_type":"developer"}),
        ));
        assert_eq!(c.agents, [("a1".to_string(), "developer".to_string())]);
        c.apply(&ev(
            "subagent.stopped",
            json!({"agent_id":"a1","agent_type":"developer"}),
        ));
        assert!(c.agents.is_empty());
        c.apply(&ev(
            "verification.completed",
            json!({"report_id":"r","status":"passed","attempt":1,"summary":"tests ok"}),
        ));
        assert!(matches!(&c.items[0], Item::Verification { status, .. } if status == "passed"));
        assert_eq!(
            changed_file("Write", &json!({"file_path":"/a/b.rs"})).as_deref(),
            Some("/a/b.rs")
        );
        assert_eq!(changed_file("Bash", &json!({"command":"ls"})), None);
        assert_eq!(c.activity().as_deref(), Some("Synthesis is thinking"));
        c.apply(&ev(
            "tool.requested",
            json!({"tool_call_id":"t9","name":"Bash","input":{"command":"ls"},"input_truncated":false}),
        ));
        assert_eq!(
            c.activity().as_deref(),
            Some("Synthesis is running Bash  ls")
        );
    }

    #[test]
    fn sub_agent_text_stays_out_of_the_main_conversation() {
        let mut e = ev(
            "model.delta",
            json!({"block_index":0,"kind":"text","text":"x"}),
        );
        e.agent_id = Some("a".into());
        let mut c = Convo::default();
        c.apply(&e);
        assert!(c.items.is_empty());
    }

    #[test]
    fn thinking_tool_output_diff_and_the_task_end_line() {
        let mut c = Convo::default();
        c.apply(&ev(
            "task.started",
            json!({"request":"r","acceptance_criteria":[],"budget":{"max_turns":5,"max_tool_calls":5,"max_verify_attempts":1}}),
        ));
        c.apply(&ev(
            "turn.started",
            json!({"model":"m","provider":"p","iteration":1}),
        ));
        for t in ["hm", "mm"] {
            c.apply(&ev(
                "model.delta",
                json!({"block_index":0,"kind":"thinking","text":t}),
            ));
        }
        c.apply(&ev(
            "tool.requested",
            json!({"tool_call_id":"t1","name":"Edit","input":{"file_path":"/a/b.rs"},"input_truncated":false}),
        ));
        c.apply(&ev(
            "tool.output",
            json!({"tool_call_id":"t1","stream":"merged","text":"live","seq":1}),
        ));
        assert!(matches!(&c.items[1], Item::Tool { output, .. } if output == "live"));
        c.apply(&ev(
            "tool.completed",
            json!({"tool_call_id":"t1","name":"Edit","status":"ok","is_error":false,"not_run":false,
                "duration_ms":7,"preview":"p","bytes":4,"truncated":false,"output":"whole","diff":{"added":1}}),
        ));
        c.apply(&ev(
            "task.ended",
            json!({"status":"completed","reason":"done","usage":{"input_tokens":1,"output_tokens":1},
                "changed_files":[],"artifacts_root":"/x"}),
        ));
        assert_eq!(c.items[0], Item::Thinking("hmmm".into()));
        assert!(
            matches!(&c.items[1], Item::Tool { output, duration_ms: 7, diff: Some(_), .. } if output == "whole")
        );
        assert!(
            matches!(&c.items[2], Item::TaskEnd { ok: true, text, .. } if text.starts_with("Done \u{b7} 1 turn"))
        );
    }

    #[test]
    fn harness_follow_ups_and_unverified_reports_stay_out_of_the_chat() {
        let mut c = Convo::default();
        c.apply(&ev(
            "user.message",
            json!({"text": "verification feedback ...", "display_text": "verification feedback", "images": []}),
        ));
        c.apply(&ev(
            "verification.completed",
            json!({"report_id": "r", "status": "unverified", "attempt": 1, "summary": "s"}),
        ));
        assert!(c.items.is_empty(), "{:?}", c.items);
    }

    #[test]
    fn a_note_is_never_a_prompt_or_a_title() {
        let note = json!({"text": "[Synthesis verification report]", "display_text": "verification feedback", "images": []});
        let mut v = crate::state::ViewState::default();
        v.apply(&ev("user.message", json!({"text": "fix it", "images": []})));
        v.apply(&ev("user.message", note));
        assert_eq!(v.session.prompts, 1);
        assert_eq!(v.session.last_prompt, "fix it");
        assert_eq!(v.convo.items.len(), 1);
    }

    fn texts(c: &Convo) -> Vec<String> {
        c.items
            .iter()
            .map(|i| match i {
                Item::Notice { text, .. } | Item::Internal(text) => text.clone(),
                o => format!("{o:?}"),
            })
            .collect()
    }

    fn one(t: &str, payload: Value) -> Convo {
        let mut c = Convo::default();
        c.apply(&ev(t, payload));
        c
    }

    #[test]
    fn every_task_end_state_says_what_it_is_with_the_checks_that_ran() {
        let ended = |status: &str, reason: &str| {
            json!({"status":status,"reason":reason,"usage":{"input_tokens":0,"output_tokens":0},
                "changed_files":[],"artifacts_root":"/x"})
        };
        let word = |c: &Convo| match c.items.last() {
            Some(Item::TaskEnd {
                ok,
                text,
                word,
                checks,
            }) => (*ok, text.clone(), word.clone(), checks.clone()),
            o => panic!("{o:?}"),
        };
        // unverified: the report is not a row of its own, the end line carries its checks
        let mut c = Convo::default();
        c.apply(&ev(
            "verification.completed",
            json!({"report_id":"r","status":"unverified","attempt":1,"summary":"cargo test: not run"}),
        ));
        c.apply(&ev("task.ended", ended("unverified", "no_check_ran")));
        assert_eq!(c.items.len(), 1);
        assert_eq!(
            word(&c),
            (
                false,
                "task unverified: no_check_ran".into(),
                "unverified".into(),
                "cargo test: not run".into()
            )
        );
        // the checks belong to one task only
        c.apply(&ev("task.ended", ended("blocked", "needs a login")));
        assert_eq!(
            word(&c),
            (
                false,
                "task blocked: needs a login".into(),
                "blocked".into(),
                String::new()
            )
        );
        // a model error and its task end are one line
        c.items.clear();
        c.apply(&ev(
            "model.error",
            json!({"message":"Invalid API key","retryable":false,"context_overflow":false}),
        ));
        c.apply(&ev(
            "task.ended",
            ended("blocked", "model_error:auth: Invalid API key"),
        ));
        assert_eq!(c.items.len(), 1);
        assert_eq!(
            word(&c).1,
            "model error (auth): Invalid API key \u{2014} check your API key env"
        );
        c.apply(&ev("task.ended", ended("failed", "failed")));
        assert_eq!(word(&c).1, "task failed");
        c.apply(&ev("task.ended", ended("cancelled", "cancelled")));
        assert_eq!(word(&c).1, "interrupted");
        // failed verification is its own row, then the failed end line repeats its checks
        let mut c = Convo::default();
        c.apply(&ev(
            "verification.completed",
            json!({"report_id":"r","status":"failed","attempt":1,"summary":"tests: 2 failed"}),
        ));
        c.apply(&ev("task.ended", ended("failed", "verification_failed")));
        assert!(matches!(&c.items[0], Item::Verification { status, .. } if status == "failed"));
        assert_eq!(word(&c).3, "tests: 2 failed");
        // passed: a green Done line, no checks repeated
        let mut c = Convo::default();
        c.apply(&ev(
            "verification.completed",
            json!({"report_id":"r","status":"passed","attempt":1,"summary":"tests ok"}),
        ));
        c.apply(&ev("task.ended", ended("completed", "done")));
        assert!(matches!(&c.items[0], Item::Verification { status, .. } if status == "passed"));
        assert!(word(&c).0 && word(&c).3.is_empty());
    }

    #[test]
    fn budget_hook_and_login_rows_are_notices() {
        let c = one(
            "budget.exceeded",
            json!({"budget":"turns","limit":5.0,"used":5.0}),
        );
        assert_eq!(texts(&c), ["budget exceeded: turns 5 / 5"]);
        assert!(matches!(&c.items[0], Item::Notice { warning: true, .. }));
        let hook = |outcome: &str| {
            one(
                "hook.ran",
                json!({"event":"Stop","hook_name":"lint","exit_code":1,"outcome":outcome,"duration_ms":1}),
            )
        };
        let failed = hook("error");
        assert_eq!(texts(&failed), ["hook lint (Stop): error"]);
        assert!(matches!(
            &failed.items[0],
            Item::Notice { warning: true, .. }
        ));
        assert!(
            matches!(&hook("success").items[0], Item::Internal(t) if t == "hook lint (Stop): success")
        );
        let c = one(
            "browser.login_required",
            json!({"origin":"https://x.test\u{1b}[2J","profile":"p"}),
        );
        let t = texts(&c).remove(0);
        assert!(
            t.starts_with("log in to https://x.test")
                && t.ends_with("in the browser window, then confirm")
        );
        assert!(!t.contains('\u{1b}'), "{t:?}");
    }

    #[test]
    fn peer_rows_name_the_peer_and_hide_bookkeeping() {
        let addr = json!({"workspace_key":"k","workspace_label":"api","session_id":"s2","session_name":"backend","instance":"i"});
        let c = one(
            "peer.delegation.accepted",
            json!({"delegation_id":"d","request_id":"r","from":addr,"mode":"plan","cap":"plan"}),
        );
        assert_eq!(texts(&c), ["working for peer backend (plan mode)"]);
        let c = one(
            "peer.delegation.denied",
            json!({"request_id":"r","from":addr,"reason":"rejected: busy"}),
        );
        assert_eq!(
            texts(&c),
            ["peer request from backend refused: rejected: busy"]
        );
        assert!(matches!(&c.items[0], Item::Notice { warning: true, .. }));
        let c = one(
            "peer.reply",
            json!({"request_id":"r","status":"answered","text":"JWT_TTL sets it","bytes":3,"duration_ms":1}),
        );
        assert_eq!(texts(&c), ["peer reply: answered  JWT_TTL sets it"]);
        // the whole answer is the row (the front end clamps it), not its first 100 characters
        let long = format!("{}\nsecond line\u{1b}[2J", "x".repeat(150));
        let c = one(
            "peer.reply",
            json!({"request_id":"r","status":"answered","text":long,"bytes":3,"duration_ms":1}),
        );
        let t = texts(&c).remove(0);
        assert!(
            t.contains(&"x".repeat(150)) && t.contains("\nsecond line"),
            "{t:?}"
        );
        assert!(!t.contains('\u{1b}'));
        assert!(matches!(&c.items[0], Item::Notice { warning: false, .. }));
        let c = one(
            "peer.reply",
            json!({"request_id":"r","status":"timeout","text":"","bytes":0,"duration_ms":1}),
        );
        assert!(matches!(&c.items[0], Item::Notice { warning: true, .. }));
        let c = one(
            "peer.request_received",
            json!({"request_id":"r","from":addr,"to":addr,"summary":"ask","bytes":1,"decision":"accepted"}),
        );
        assert!(
            matches!(&c.items[..], [Item::Notice { warning: false, text }] if text == "peer request accepted from backend: ask"),
            "{:?}",
            c.items
        );
        let c = one(
            "peer.request_received",
            json!({"request_id":"r","from":addr,"to":addr,"summary":"ask","bytes":1,"decision":"rejected","reason":"busy"}),
        );
        assert!(matches!(&c.items[..], [Item::Internal(_)]));
        for (t, p, line) in [
            (
                "peer.listening",
                json!({"endpoint_kind":"unix"}),
                "peers: listening for requests",
            ),
            (
                "peer.thread_closed",
                json!({"thread_id":"t","reason":"cap"}),
                "peer thread closed (cap)",
            ),
            (
                "peer.request_sent",
                json!({"request_id":"r","from":addr,"to":addr,"summary":"ask","bytes":1,"chain_len":1}),
                "peer request sent to backend: ask",
            ),
            (
                "peer.delegation.tool",
                json!({"delegation_id":"d","tool_call_id":"t","name":"Read","origin":"peer","from":"api/backend"}),
                "peer work: Read for api/backend",
            ),
        ] {
            let c = one(t, p);
            assert!(
                matches!(&c.items[..], [Item::Internal(x)] if x == line),
                "{t}: {:?}",
                c.items
            );
        }
    }

    #[test]
    fn agent_messages_read_as_arrows_and_a_user_line_is_not_repeated() {
        let sent = |from: &str, to: &str, origin: &str, kind: &str| {
            texts(&one(
                "message.sent",
                json!({"message_id":"m","from":from,"to":to,"origin":origin,"kind":kind,"text":"hi","bytes":2}),
            ))
        };
        assert_eq!(
            sent("main", "dev", "agent", "message"),
            ["\u{2192} @dev hi"]
        );
        assert_eq!(
            sent("dev", "main", "agent", "result"),
            ["\u{2190} @dev result: hi"]
        );
        assert_eq!(sent("a", "b", "agent", "message"), ["@a \u{2192} @b: hi"]);
        assert_eq!(sent("x", "main", "system", "message"), ["\u{26a0} hi"]);
        assert!(sent("user", "dev", "user", "message").is_empty());
    }

    #[test]
    fn memory_rows_are_verbose_only_except_what_needs_the_person() {
        let internal = |t: &str, p: Value, line: &str| {
            let c = one(t, p);
            assert!(
                matches!(&c.items[..], [Item::Internal(x)] if x == line),
                "{t}: {:?}",
                c.items
            );
        };
        internal(
            "memory.written",
            json!({"id":"m1","op":"added","kind":"episode","scope":"workspace","trust":"untrusted","status":"active"}),
            "memory: episode note added",
        );
        internal(
            "memory.retrieved",
            json!({"ids":["m1"],"tokens":12,"candidates":3}),
            "memory: 1 note(s) retrieved (12 tokens)",
        );
        internal(
            "memory.quarantined",
            json!({"id":"m","rule":"r1"}),
            "memory: a note was quarantined (r1)",
        );
        let c = one("memory.proposal.created", json!({"id":"p-1"}));
        assert_eq!(texts(&c), ["a memory note is waiting for your review"]);
        let c = one(
            "memory.reflection.failed",
            json!({"job_id":"j","error":"boom"}),
        );
        assert_eq!(texts(&c), ["memory reflection failed: boom"]);
        assert!(matches!(&c.items[0], Item::Notice { warning: true, .. }));
    }
}
