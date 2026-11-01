//! Event stream -> conversation items of the agent pane. The UI keeps no truth of its own: every
//! row is derived from an `Envelope`. Items mutate in place while streaming, so every mutation
//! bumps `Item::rev` (the renderer caches rows per item, see `synthesis_tui_render::agent::Item`).

use std::collections::HashMap;

use serde_json::Value;
use synthesis_events::{Envelope, Event, TaskStatus, ToolStatus};
use synthesis_tui_render::agent::{Agent, Item};
use synthesis_tui_render::model::parse_ts;

use crate::sanitize::{clean_input_lines, clean_line, clean_text};
use crate::status::ContextStatus;

/// Running totals the status line shows (`BudgetUsage` only exists at task end).
#[derive(Clone, Debug, Default)]
pub struct Totals {
    pub turns: u32,
    pub tool_calls: u32,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cost_usd: Option<f64>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Verification {
    /// passed / failed / unverified / blocked
    pub status: String,
    pub summary: String,
}

/// A sub-agent as the events describe it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AgentRow {
    pub id: String,
    pub name: String,
    pub kind: String,
    /// `running | waiting_approval | idle | cancelled`
    pub status: String,
    pub tools: u32,
    pub queued: u32,
    /// Its replies are drawn in the transcript (background agents and agents that run a task).
    pub show: bool,
}

#[derive(Default)]
pub struct Chat {
    pub agent: Agent,
    pub agents: Vec<AgentRow>,
    /// `message_id -> recipient` of messages that are queued and not delivered yet.
    queued: HashMap<String, String>,
    /// Lines the person sent to a running agent that it has not read yet: `(message_id, text)`.
    pub inflight: Vec<(String, String)>,
    /// Lines that were dropped undelivered (the prompt takes them back).
    pub dropped_lines: Vec<String>,
    tools: HashMap<String, usize>,
    text: Option<usize>,
    think: Option<usize>,
    /// Event time (ms) of the first delta of the open thinking block.
    think_t0: Option<i64>,
    turn_text: bool,
    /// The one transcript row of TodoWrite calls (each call updates it, the live panel has the rest).
    todo_row: Option<usize>,
    /// A `model.error` was shown during this task (the task's end then says no more about it).
    model_error_shown: bool,
    pub totals: Totals,
    pub verification: Option<Verification>,
    pub ctx: ContextStatus,
    /// Task counters start fresh with every task.
    pub task_started: Option<i64>,
    /// Startup notes whose `Notice` event has not come through the event channel yet.
    /// `(tone, text)`: a later notice with the same text but another severity is a different event.
    startup: Vec<(String, String)>,
}

/// Most live output text kept per running tool row (the tail).
const MAX_LIVE: usize = 256 * 1024;

/// The 6-character id the tool rows of a sub-agent carry.
fn short_id(id: &str) -> String {
    clean_line(&id.chars().take(6).collect::<String>())
}

/// First line of a message, short enough for one transcript row.
fn head(text: &str) -> String {
    let line = text.lines().next().unwrap_or_default();
    let mut s: String = line.chars().take(100).collect();
    if line.chars().count() > 100 || text.lines().count() > 1 {
        s.push('…');
    }
    clean_line(&s)
}

fn tok_k(n: u64) -> String {
    if n >= 1000 {
        format!("{:.1}k", n as f64 / 1000.0)
    } else {
        n.to_string()
    }
}

fn touch(it: &mut Item) {
    it.rev += 1;
}

/// `tool.completed.diff` with every text made safe to draw (file content is untrusted).
pub(crate) fn clean_diff(d: &Value) -> Value {
    match d {
        Value::String(s) => Value::String(clean_text(s)),
        Value::Array(a) => Value::Array(a.iter().map(clean_diff).collect()),
        Value::Object(o) => {
            Value::Object(o.iter().map(|(k, v)| (k.clone(), clean_diff(v))).collect())
        }
        v => v.clone(),
    }
}

/// The compaction error as a sentence (the context crate reports its enum with `{:?}`) and the
/// tone to show it in: "nothing to do" is a warning, a real failure an error.
pub(crate) fn friendly_compact_error(e: &str) -> (String, &'static str) {
    if e.contains("NoProgress") || e.contains("Nothing") {
        (
            "nothing to compact yet (a summary would not be smaller)".into(),
            "warn",
        )
    } else if e.contains("EmptySummary") {
        (
            "the model returned an empty summary, try again".into(),
            "error",
        )
    } else if e.contains("cancelled") {
        ("cancelled".into(), "warn")
    } else if let Some(i) = e.find("Summarizer") {
        (
            format!(
                "the model could not summarise the conversation ({})",
                e[i..]
                    .trim_start_matches("Summarizer")
                    .trim_matches(|c| "() ".contains(c))
            ),
            "error",
        )
    } else {
        (e.to_string(), "error")
    }
}

fn note(text: impl Into<String>, tone: &str) -> Item {
    item! {
        kind: "note".into(),
        text: clean_text(&text.into()),
        tone: tone.into(),
    }
}

impl Chat {
    fn row_mut(&mut self, id: &str) -> Option<&mut AgentRow> {
        self.agents.iter_mut().find(|r| r.id == id)
    }

    fn name_of(&self, id: &str) -> String {
        self.agents
            .iter()
            .find(|r| r.id == id)
            .map_or_else(|| short_id(id), |r| r.name.clone())
    }

    fn unqueue(&mut self, message_id: &str) {
        if let Some(to) = self.queued.remove(message_id)
            && let Some(r) = self.agents.iter_mut().find(|r| r.name == to)
        {
            r.queued = r.queued.saturating_sub(1);
        }
    }

    pub fn push(&mut self, it: Item) -> usize {
        self.agent.items.push(it);
        self.agent.items.len() - 1
    }

    pub fn note(&mut self, text: impl Into<String>, tone: &str) {
        self.push(note(text, tone));
    }

    /// Startup notes are shown now; the same `Notice` event arriving later is not shown again.
    pub fn startup_note(&mut self, text: &str) {
        let t = clean_line(text);
        self.startup.push(("warn".into(), t.clone()));
        self.note(t, "warn");
    }

    fn take_startup(&mut self, tone: &str, text: &str) -> bool {
        let t = clean_line(text);
        self.startup
            .iter()
            .position(|(k, s)| k == tone && *s == t)
            .map(|i| self.startup.remove(i))
            .is_some()
    }

    fn finish_streams(&mut self, now: i64) {
        if let Some(i) = self.text.take() {
            let it = &mut self.agent.items[i];
            it.streaming = false;
            touch(it);
        }
        if let Some(i) = self.think.take() {
            let it = &mut self.agent.items[i];
            it.streaming = false;
            it.end = Some(now);
            touch(it);
        }
    }

    /// `now` is the UI clock (ms), the same one the renderer gets.
    pub fn apply(&mut self, env: &Envelope, now: i64) {
        let sub = env.agent_id.clone();
        match &env.event {
            Event::TaskStarted { .. } => {
                self.model_error_shown = false;
                self.task_started = Some(now);
                self.agent.turn_start = now;
                self.agent.turn_tokens = 0;
                self.agent.status = "running".into();
                self.agent.interrupting = false;
                self.verification = None;
                self.totals = Totals::default();
            }
            Event::UserMessage {
                text, display_text, ..
            } => {
                let t = display_text.as_deref().unwrap_or(text);
                // `Some("")`: a note from the harness to the model, not something typed
                if !t.is_empty() {
                    // a line to a sub-agent reads as typed with its address
                    let t = match &sub {
                        Some(a) => format!("@{} {t}", self.name_of(a)),
                        None => t.to_string(),
                    };
                    self.push(item! {
                        kind: "user".into(),
                        text: clean_text(&t),
                    });
                }
            }
            Event::TurnStarted { .. } => {
                self.turn_text = false;
                self.totals.turns += 1;
            }
            Event::ModelDelta { kind, text, .. } if sub.is_none() => match kind.as_str() {
                "text" => {
                    let i = match self.text {
                        Some(i) => i,
                        None => {
                            let i = self.push(item! {
                                kind: "text".into(),
                                streaming: true,
                            });
                            self.text = Some(i);
                            i
                        }
                    };
                    self.turn_text = true;
                    let it = &mut self.agent.items[i];
                    it.text.push_str(&clean_text(text));
                    touch(it);
                }
                "thinking" => {
                    let i = match self.think {
                        Some(i) => i,
                        None => {
                            let i = self.push(item! {
                                kind: "thinking".into(),
                                streaming: true,
                                start: Some(now),
                            });
                            self.think = Some(i);
                            self.think_t0 = None;
                            i
                        }
                    };
                    let it = &mut self.agent.items[i];
                    it.text.push_str(&clean_text(text));
                    // the block's duration is first to last delta, so a replay shows the real one
                    if let Some(t) = parse_ts(&env.ts) {
                        it.ms = t - *self.think_t0.get_or_insert(t);
                    }
                    touch(it);
                }
                _ => {}
            },
            Event::AssistantMessage { content, .. }
                if sub
                    .as_deref()
                    .is_some_and(|a| self.agents.iter().any(|r| r.id == a && r.show)) =>
            {
                let name = self.name_of(sub.as_deref().unwrap_or_default());
                for b in content {
                    if b.get("type").and_then(Value::as_str) == Some("text")
                        && let Some(t) = b.get("text").and_then(Value::as_str)
                        && !t.trim().is_empty()
                    {
                        self.push(item! {
                            kind: "text".into(),
                            text: format!("**@{name}** {}", clean_text(t)),
                        });
                    }
                }
            }
            Event::AssistantMessage { content, .. } if sub.is_none() => {
                self.finish_streams(now);
                if !self.turn_text {
                    for b in content {
                        if b.get("type").and_then(Value::as_str) == Some("text")
                            && let Some(t) = b.get("text").and_then(Value::as_str)
                            && !t.trim().is_empty()
                        {
                            self.push(item! {
                                kind: "text".into(),
                                text: clean_text(t),
                            });
                        }
                    }
                }
            }
            Event::TurnCompleted { usage, .. } if sub.is_none() => {
                self.finish_streams(now);
                if let Some(u) = usage {
                    self.totals.input_tokens += u.input_tokens;
                    self.totals.output_tokens += u.output_tokens;
                    self.agent.turn_tokens += u.output_tokens;
                    if let Some(c) = u.cost_usd {
                        *self.totals.cost_usd.get_or_insert(0.0) += c;
                    }
                }
            }
            Event::ModelRetry {
                attempt,
                max,
                error,
                ..
            } => self.note(format!("model retry {attempt}/{max}: {error}"), "warn"),
            Event::ModelError { message, .. } => {
                self.model_error_shown = true;
                self.note(format!("model error: {message}"), "error")
            }
            Event::ContextUsage {
                used_tokens,
                window_tokens,
                percent,
                stale,
                ..
            } if sub.is_none() => {
                self.ctx
                    .on_usage(*used_tokens, *window_tokens, Some(*percent), *stale)
            }
            Event::ContextCompacting { .. } if sub.is_none() => self.ctx.on_compacting(),
            Event::ContextCompactionFailed { reason, error } => {
                self.ctx.on_compaction_failed();
                // a manual /compact reports its own failure once, in a sentence
                if reason != "manual" {
                    let (why, _) = friendly_compact_error(error);
                    self.note(format!("context compaction failed: {why}"), "warn");
                }
            }
            Event::ContextCompacted {
                trigger,
                tokens_before,
                before_tokens,
                after_tokens,
                window_tokens,
                ..
            } => {
                let from_to = if *after_tokens > 0 {
                    let b = if *before_tokens > 0 {
                        *before_tokens
                    } else {
                        tokens_before.unwrap_or(0)
                    };
                    format!(": {} → {} tokens", tok_k(b), tok_k(*after_tokens))
                } else {
                    String::new()
                };
                self.note(format!("context compacted ({trigger}){from_to}"), "");
                if sub.is_none() {
                    // logs from before the numbers existed carry only `tokens_before`
                    let before = if *before_tokens > 0 {
                        *before_tokens
                    } else {
                        tokens_before.unwrap_or(0)
                    };
                    self.ctx
                        .on_compacted(before, *after_tokens, *window_tokens, now);
                }
            }
            Event::ToolRequested {
                tool_call_id,
                name,
                input,
                ..
            } => {
                self.finish_streams(now);
                self.totals.tool_calls += 1;
                let name = clean_line(name);
                if name == "TodoWrite"
                    && sub.is_none()
                    && let Some(i) = self.todo_row.filter(|i| {
                        self.agent
                            .items
                            .get(*i)
                            .is_some_and(|it| it.name == "TodoWrite")
                    })
                {
                    let it = &mut self.agent.items[i];
                    it.input = clean_input_lines(input);
                    it.status = "running".into();
                    it.start = Some(now);
                    touch(it);
                    self.tools.insert(tool_call_id.clone(), i);
                    return;
                }
                let name = match &sub {
                    Some(a) => {
                        if let Some(r) = self.row_mut(a) {
                            r.tools += 1;
                        }
                        format!("@{}:{name}", self.name_of(a))
                    }
                    None => name,
                };
                let name_is_todo = name == "TodoWrite";
                let i = self.push(item! {
                    kind: "tool".into(),
                    name,
                    input: clean_input_lines(input),
                    status: "running".into(),
                    start: Some(now),
                });
                if name_is_todo {
                    self.todo_row = Some(i);
                }
                self.tools.insert(tool_call_id.clone(), i);
            }
            Event::ToolOutput {
                tool_call_id, text, ..
            } => {
                if let Some(&i) = self.tools.get(tool_call_id) {
                    let it = &mut self.agent.items[i];
                    it.output.push_str(&clean_text(text));
                    if it.output.len() > MAX_LIVE {
                        // the live view is the tail; the whole result arrives with tool.completed
                        let mut cut = it.output.len() - MAX_LIVE;
                        while !it.output.is_char_boundary(cut) {
                            cut += 1;
                        }
                        it.output.drain(..cut);
                    }
                    touch(it);
                }
            }
            Event::ToolCompleted {
                tool_call_id,
                status,
                preview,
                output,
                output_truncated,
                output_bytes,
                result_ref,
                diff,
                ..
            } => {
                if let Some(&i) = self.tools.get(tool_call_id) {
                    let it = &mut self.agent.items[i];
                    it.status = match status {
                        ToolStatus::Ok => "done",
                        ToolStatus::Interrupted => "interrupted",
                        _ => "error",
                    }
                    .into();
                    it.soft = *status == ToolStatus::SoftError;
                    // the whole model-facing text; the 200-char preview only for older logs
                    it.result = clean_text(output.as_deref().unwrap_or(preview));
                    if *output_truncated {
                        let whole = match result_ref {
                            Some(a) => format!(
                                "full output ({output_bytes} bytes): artifact {}",
                                clean_line(&a.path)
                            ),
                            None => format!("the whole ({output_bytes} bytes) could not be saved"),
                        };
                        it.result = format!("[output truncated · {whole}]\n{}", it.result);
                    }
                    if let Some(d) = diff {
                        it.diff = clean_diff(d);
                    }
                    let live = std::mem::take(&mut it.output);
                    // an interrupted command keeps what it had printed
                    if *status == ToolStatus::Interrupted && !live.trim().is_empty() {
                        it.result = live;
                    }
                    it.end = Some(now);
                    touch(it);
                }
            }
            Event::ToolApprovalResolved {
                decision, reviewer, ..
            } if reviewer != "user" || decision == "expired" => {
                self.note(format!("approval {decision} ({reviewer})"), "warn");
            }
            Event::VerificationCompleted {
                status, summary, ..
            } => {
                self.verification = Some(Verification {
                    status: clean_line(status),
                    summary: clean_text(summary),
                });
                let tone = match status.as_str() {
                    "passed" => "goal",
                    "failed" => "error",
                    _ => "warn",
                };
                self.note(format!("verification {status}: {summary}"), tone);
            }
            Event::BudgetExceeded {
                budget,
                limit,
                used,
            } => self.note(
                format!("budget exceeded: {budget} {used} / {limit}"),
                "warn",
            ),
            Event::SubagentStarted(i) => {
                let name = i
                    .name
                    .clone()
                    .map_or_else(|| short_id(&i.agent_id), |n| clean_line(&n));
                self.agents.retain(|r| r.id != i.agent_id);
                self.agents.push(AgentRow {
                    id: i.agent_id.clone(),
                    name: name.clone(),
                    kind: clean_line(&i.agent_type),
                    status: "running".into(),
                    show: i.background || i.tool_call_id.is_none(),
                    ..AgentRow::default()
                });
                self.note(
                    format!(
                        "agent @{name} started ({}){}",
                        i.agent_type,
                        if i.background { " · background" } else { "" }
                    ),
                    "",
                );
            }
            Event::SubagentStopped(i) => {
                let name = self.name_of(&i.agent_id);
                self.note(format!("agent @{name} stopped ({})", i.agent_type), "");
            }
            Event::SubagentStatusChanged { agent_id, to, .. } => {
                if to == "evicted" {
                    self.agents.retain(|r| r.id != *agent_id);
                } else if let Some(r) = self.row_mut(agent_id) {
                    r.status.clone_from(to);
                }
            }
            Event::MessageSent {
                message_id,
                from,
                to,
                origin,
                kind,
                summary,
                text,
                ..
            } => {
                if let Some(r) = self.agents.iter_mut().find(|r| r.name == *to) {
                    r.queued += 1;
                    self.queued.insert(message_id.clone(), to.clone());
                }
                let what = summary.clone().unwrap_or_else(|| head(text));
                if origin == "user" {
                    self.inflight.push((message_id.clone(), text.clone()));
                } else if origin == "system" {
                    self.note(format!("⚠ {what}"), "warn");
                } else if from == "main" {
                    self.note(format!("→ @{to} {what}"), "");
                } else if to == "main" {
                    let label = if kind == "result" { " result:" } else { ":" };
                    self.note(format!("← @{from}{label} {what}"), "");
                } else {
                    self.note(format!("@{from} → @{to}: {what}"), "");
                }
            }
            Event::MessageDelivered { message_id, .. } => {
                self.unqueue(message_id);
                self.inflight.retain(|(id, _)| id != message_id);
            }
            Event::MessageDropped { message_id, reason } => {
                self.unqueue(message_id);
                if let Some(i) = self.inflight.iter().position(|(id, _)| id == message_id) {
                    self.dropped_lines.push(self.inflight.remove(i).1);
                }
                self.note(format!("⚠ message dropped: {reason}"), "warn");
            }
            Event::AutoVerdict {
                name,
                verdict,
                reason,
                ..
            } => self.note(
                format!("auto mode: {verdict} {name}: {reason}"),
                if verdict == "allow" { "" } else { "warn" },
            ),
            Event::ModelChanged { model, .. } => {
                self.agent.model = clean_line(model);
                self.note(format!("model: {model}"), "");
            }
            // Already shown as a startup note: the event log keeps it, the pane shows it once.
            Event::Notice { level, text } => {
                let tone = match level.as_str() {
                    "error" => "error",
                    "warning" | "warn" => "warn",
                    _ => "",
                };
                if !self.take_startup(tone, text) {
                    self.note(text.clone(), tone);
                }
            }
            Event::TaskEnded {
                status,
                reason,
                usage,
                ..
            } => {
                self.finish_streams(now);
                for it in self.agent.items.iter_mut().rev() {
                    if it.kind == "text" {
                        it.is_final = *status == TaskStatus::Completed;
                        touch(it);
                        break;
                    }
                    if it.kind == "user" {
                        break;
                    }
                }
                // A task that is not `completed` is never shown as success.
                if *status == TaskStatus::Completed {
                    let ms = self.task_started.map_or(0, |t| now - t);
                    self.push(item! {
                        kind: "result".into(),
                        ok: true,
                        turns: u64::from(self.totals.turns),
                        ms,
                        cost: usage.cost_usd,
                    });
                } else {
                    let kept = self
                        .agent
                        .items
                        .iter()
                        .rev()
                        .take_while(|it| it.kind != "user")
                        .any(|it| it.kind == "text" || it.kind == "tool");
                    let (word, tone) = match status {
                        TaskStatus::Failed => ("failed", "error"),
                        TaskStatus::Cancelled => ("cancelled", "warn"),
                        TaskStatus::Blocked => ("blocked", "warn"),
                        TaskStatus::Unverified => ("unverified", "warn"),
                        _ => ("ended", "warn"),
                    };
                    if self.model_error_shown && reason.starts_with("model_error") {
                        // already said by the model error just above
                        self.push(item! { kind: "note".into(), text: format!("task {word}"), tone: tone.into() });
                    } else if *status == TaskStatus::Cancelled && reason == "cancelled" {
                        let kept = if kept { " · partial output kept" } else { "" };
                        self.note(format!("interrupted{kept}"), tone);
                    } else {
                        self.note(format!("task {word}: {reason}"), tone);
                    }
                }
                if matches!(status, TaskStatus::Unverified | TaskStatus::Blocked)
                    && self.verification.is_none()
                {
                    self.verification = Some(Verification {
                        status: if *status == TaskStatus::Blocked {
                            "blocked"
                        } else {
                            "unverified"
                        }
                        .into(),
                        summary: clean_text(reason),
                    });
                }
                self.agent.status = "idle".into();
                self.agent.interrupting = false;
                self.tools.clear();
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Harness notices are told by `origin`; an agent that is merely called "system" is a peer.
    #[test]
    fn system_notices_are_rendered_by_origin_not_by_sender_name() {
        let mut chat = Chat::default();
        let mut sent = |from: &str, origin: &str| {
            chat.apply(
                &Envelope {
                    v: 1,
                    seq: 1,
                    ts: "2026-01-01T00:00:00.000Z".into(),
                    session_id: "s".into(),
                    task_id: None,
                    turn_id: None,
                    agent_id: None,
                    event: Event::MessageSent {
                        message_id: "m".into(),
                        from: from.into(),
                        to: "main".into(),
                        origin: origin.into(),
                        kind: "message".into(),
                        summary: None,
                        text: "hi".into(),
                        bytes: 2,
                        notify_when_idle: false,
                    },
                },
                0,
            );
            chat.agent.items.last().unwrap().text.clone()
        };
        assert!(sent("x", "system").starts_with('⚠'));
        assert!(!sent("system", "agent").starts_with('⚠'));
    }

    #[test]
    fn tool_name_and_input_are_stored_without_escape_bytes() {
        let evil = "a\x1b]52;c;ZXZpbA==\x07b\x1b[6nc\u{9b}6n\u{202e}d\x08e";
        let mut chat = Chat::default();
        chat.apply(
            &Envelope {
                v: 1,
                seq: 1,
                ts: "2026-01-01T00:00:00.000Z".into(),
                session_id: "s".into(),
                task_id: None,
                turn_id: None,
                agent_id: None,
                event: Event::ToolRequested {
                    tool_call_id: "c".into(),
                    name: format!("T{evil}"),
                    input: json!({ evil: [evil, {"k": evil}], "n": 1 }),
                    input_truncated: false,
                    origin: None,
                },
            },
            0,
        );
        let it = chat.agent.items.last().unwrap();
        // JSON text escapes control chars as \uXXXX, so a surviving one shows up as that.
        let all = format!("{}{}", it.name, it.input);
        assert!(
            !it.name.chars().any(|c| c.is_control() || c == '\u{202e}'),
            "{all:?}"
        );
        for bad in ["\\u001b", "\\u009b", "\\u0007", "\\u0008", "\\u202e"] {
            assert!(!all.contains(bad), "{bad} in {all}");
        }
        assert!(it.name.starts_with('T'));
    }

    /// A stored session: thinking deltas 4.2 s apart, then an Edit with a `diff` and one without.
    #[test]
    fn replayed_session_shows_thinking_duration_and_line_numbered_diff() {
        let line = |seq: u32, ts: &str, ty: &str, payload: Value| {
            json!({"v": 1, "seq": seq, "ts": ts, "session_id": "s", "task_id": null,
                "turn_id": null, "type": ty, "payload": payload})
            .to_string()
        };
        let done = |id: &str, diff: Option<Value>| {
            let mut p = json!({"tool_call_id": id, "name": "Edit", "status": "ok", "is_error": false,
                "not_run": false, "duration_ms": 1, "preview": "p", "bytes": 1, "truncated": false});
            if let Some(d) = diff {
                p["diff"] = d;
            }
            p
        };
        let input = json!({"file_path": "src/a.rs", "old_string": "let a = 1;", "new_string": "let a = 2;"});
        let req = |id: &str| json!({"tool_call_id": id, "name": "Edit", "input": input, "input_truncated": false});
        let diff = json!({"path": "src/a.rs", "kind": "update", "added": 1, "removed": 1, "truncated": false,
            "hunks": [{"old_start": 41, "old_lines": 1, "new_start": 41, "new_lines": 1,
                "lines": [{"tag": "-", "old_no": 41, "text": "let a = 1;"}, {"tag": "+", "new_no": 41, "text": "let a = 2;\u{1b}[31m"}]}]});
        let jsonl = [
            line(
                1,
                "2026-01-01T00:00:00.000Z",
                "model.delta",
                json!({"block_index": 0, "kind": "thinking", "text": "hmm "}),
            ),
            line(
                2,
                "2026-01-01T00:00:04.200Z",
                "model.delta",
                json!({"block_index": 0, "kind": "thinking", "text": "ok"}),
            ),
            line(3, "2026-01-01T00:00:09.000Z", "tool.requested", req("a")),
            line(
                4,
                "2026-01-01T00:00:10.000Z",
                "tool.completed",
                done("a", Some(diff)),
            ),
            line(5, "2026-01-01T00:00:11.000Z", "tool.requested", req("b")),
            line(
                6,
                "2026-01-01T00:00:12.000Z",
                "tool.completed",
                done("b", None),
            ),
        ]
        .join("\n");
        let mut chat = Chat::default();
        for rec in synthesis_events::EventReader::new(jsonl.as_bytes()) {
            let synthesis_events::Record::Known(e) = rec.unwrap() else {
                panic!("known")
            };
            chat.apply(&e, parse_ts(&e.ts).unwrap());
        }
        let (lines, _) =
            synthesis_tui_render::agent::transcript_rows(&chat.agent, 80, 40, 0, 0, false, 0);
        let text: Vec<String> = lines
            .iter()
            .map(|l| {
                synthesis_tui_base::text::strip_ansi(l)
                    .trim_end()
                    .to_string()
            })
            .collect();
        let all = text.join("\n");
        assert!(all.contains("✻ Thinking · 4.2s"), "{all}");
        assert!(all.contains("│ hmm ok"), "{all}");
        assert!(all.contains("Updated src/a.rs — +1 −1 (line 41)"), "{all}");
        assert!(
            all.contains("41     − let a = 1;") || all.contains("41    − let a = 1;"),
            "{all}"
        );
        assert!(
            !all.contains('\u{1b}'),
            "escape bytes must not reach the screen"
        );
        assert!(
            all.contains("(line numbers unavailable)"),
            "older event without diff: {all}"
        );
    }

    fn env(seq: u64, event: Event) -> Envelope {
        Envelope {
            v: 1,
            seq,
            ts: "2026-01-01T00:00:00.000Z".into(),
            session_id: "s".into(),
            task_id: None,
            turn_id: None,
            agent_id: None,
            event,
        }
    }

    fn requested(id: &str, name: &str) -> Event {
        Event::ToolRequested {
            tool_call_id: id.into(),
            name: name.into(),
            input: json!({"command": "x"}),
            input_truncated: false,
            origin: None,
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn completed(id: &str, preview: &str, output: Option<&str>, cut: bool, aref: bool) -> Event {
        Event::ToolCompleted {
            tool_call_id: id.into(),
            name: "Bash".into(),
            status: ToolStatus::Ok,
            is_error: false,
            not_run: false,
            duration_ms: 1,
            preview: preview.into(),
            bytes: 9,
            truncated: false,
            result_ref: aref.then(|| synthesis_events::ArtifactRef {
                artifact_id: "a".into(),
                path: "terminal/proc_1-ab.log".into(),
                size: 9,
                sha256: "0".into(),
                mime: "text/plain".into(),
            }),
            origin: None,
            output: output.map(Into::into),
            output_truncated: cut,
            output_bytes: 9_000_000,
            diff: None,
        }
    }

    #[test]
    fn live_output_is_the_running_rows_tail_and_the_completed_row_uses_the_whole_output() {
        let mut chat = Chat::default();
        chat.apply(&env(1, requested("c", "Bash")), 0);
        for (i, t) in ["one\ntw", "o\nthree\n"].iter().enumerate() {
            chat.apply(
                &env(
                    2 + i as u64,
                    Event::ToolOutput {
                        tool_call_id: "c".into(),
                        stream: "merged".into(),
                        text: (*t).into(),
                        seq: 1 + i as u64,
                        origin: None,
                    },
                ),
                0,
            );
        }
        let it = chat.agent.items.last().unwrap();
        assert_eq!(
            (it.status.as_str(), it.output.as_str()),
            ("running", "one\ntwo\nthree\n")
        );
        let whole = "one\ntwo\nthree\nfour";
        chat.apply(
            &env(4, completed("c", "one\ntwo...", Some(whole), false, false)),
            5,
        );
        let it = chat.agent.items.last().unwrap();
        assert_eq!(
            (it.status.as_str(), it.result.as_str(), it.output.as_str()),
            ("done", whole, "")
        );
    }

    #[test]
    fn a_cut_output_is_marked_with_its_artifact_path_never_silently() {
        let mut chat = Chat::default();
        chat.apply(&env(1, requested("c", "mcp__x__y")), 0);
        chat.apply(
            &env(2, completed("c", "p", Some("head ... tail"), true, true)),
            1,
        );
        let r = &chat.agent.items.last().unwrap().result;
        assert!(
            r.starts_with("[output truncated")
                && r.contains("terminal/proc_1-ab.log")
                && r.ends_with("head ... tail"),
            "{r}"
        );
        // no artifact could be written: still said, not hidden
        chat.apply(&env(3, requested("d", "mcp__x__y")), 0);
        chat.apply(
            &env(4, completed("d", "p", Some("head ... tail"), true, false)),
            1,
        );
        assert!(
            chat.agent
                .items
                .last()
                .unwrap()
                .result
                .contains("could not be saved")
        );
        // logs from before `output` existed still show their preview
        chat.apply(&env(5, requested("e", "Read")), 0);
        chat.apply(
            &env(6, completed("e", "old preview", None, false, false)),
            1,
        );
        assert_eq!(chat.agent.items.last().unwrap().result, "old preview");
    }

    #[test]
    fn interrupted_bash_keeps_its_output_todo_calls_share_a_row_and_errors_are_said_once() {
        let mut chat = Chat::default();
        chat.apply(&env(1, requested("c", "Bash")), 0);
        chat.apply(
            &env(
                2,
                Event::ToolOutput {
                    tool_call_id: "c".into(),
                    stream: "merged".into(),
                    text: "one\ntwo\n".into(),
                    seq: 1,
                    origin: None,
                },
            ),
            0,
        );
        let mut done = completed("c", "Interrupted.", Some("Interrupted."), false, false);
        if let Event::ToolCompleted { status, .. } = &mut done {
            *status = ToolStatus::Interrupted;
        }
        chat.apply(&env(3, done), 1);
        let it = chat.agent.items.last().unwrap();
        assert_eq!(
            (it.status.as_str(), it.result.as_str()),
            ("interrupted", "one\ntwo\n")
        );
        let (rows, _) =
            synthesis_tui_render::agent::transcript_rows(&chat.agent, 80, 20, 0, 0, false, 0);
        let all = rows
            .iter()
            .map(|l| synthesis_tui_base::text::strip_ansi(l))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(all.contains("one") && all.contains("interrupted"), "{all}");

        let n = chat.agent.items.len();
        for id in ["t1", "t2", "t3"] {
            chat.apply(&env(10, requested(id, "TodoWrite")), 0);
        }
        assert_eq!(
            chat.agent.items.len(),
            n + 1,
            "one row for all TodoWrite calls"
        );

        let mut c2 = Chat::default();
        c2.apply(
            &env(
                1,
                Event::ModelError {
                    message: "Invalid API key".into(),
                    retryable: false,
                    status: None,
                    context_overflow: false,
                },
            ),
            0,
        );
        let before = c2.agent.items.len();
        c2.apply(
            &env(
                2,
                Event::TaskEnded {
                    status: TaskStatus::Blocked,
                    reason: "model_error:auth: Invalid API key".into(),
                    usage: synthesis_events::Usage {
                        input_tokens: 0,
                        output_tokens: 0,
                        cache_read_input_tokens: None,
                        cache_creation_input_tokens: None,
                        cost_usd: None,
                    },
                    verification_report_id: None,
                    changed_files: vec![],
                    artifacts_root: String::new(),
                    needed_permissions: vec![],
                },
            ),
            0,
        );
        let last = c2.agent.items.last().unwrap();
        assert_eq!(c2.agent.items.len(), before + 1);
        assert!(!last.text.contains("Invalid API key"), "{}", last.text);
    }

    #[test]
    fn a_note_to_the_model_is_not_a_user_row() {
        let mut chat = Chat::default();
        let msg = |t: &str, d: Option<&str>| {
            env(
                1,
                Event::UserMessage {
                    text: t.into(),
                    display_text: d.map(Into::into),
                    images: vec![],
                },
            )
        };
        chat.apply(&msg("[Synthesis note] analyse", Some("")), 0);
        assert!(chat.agent.items.is_empty());
        chat.apply(
            &msg("<user-bash-input>ls</user-bash-input>", Some("!ls")),
            0,
        );
        assert_eq!(chat.agent.items[0].text, "!ls");
    }
}
