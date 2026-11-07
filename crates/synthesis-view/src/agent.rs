//! What the desktop Agent view shows beyond the conversation, as one reducer over the event stream:
//! a transcript per sub-agent, the command history (Bash calls and `process.*`), verification
//! reports, browser calls (with the latest screenshot path) and the files the session changed.
//! Pure state, no GPUI. The main conversation stays in [`crate::convo`].

use serde_json::Value;
use synthesis_events::{Envelope, Event, ToolStatus};

use crate::model::{parse_ts, summarize_tool};

/// Entries kept per sub-agent, commands kept, and lines of output tail kept per command.
pub const ENTRY_LIMIT: usize = 200;
pub const COMMAND_LIMIT: usize = 200;
pub const TAIL_LINES: usize = 8;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryKind {
    /// What the sub-agent said.
    Text,
    /// A tool call it made.
    Tool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Entry {
    pub ts: Option<i64>,
    pub kind: EntryKind,
    pub text: String,
    pub failed: bool,
}

/// One sub-agent (`subagent.*` events) with the transcript of what it did.
#[derive(Debug, Clone, PartialEq)]
pub struct SubAgent {
    pub id: String,
    /// The instance name when the session gave one, else the type.
    pub name: String,
    pub kind: String,
    pub background: bool,
    /// `running`, `waiting_approval`, `idle`, `cancelled`, `evicted` (the status events) or `stopped`.
    pub status: String,
    pub model: String,
    pub started: Option<i64>,
    pub ended: Option<i64>,
    pub reason: String,
    pub tool_calls: u32,
    pub entries: Vec<Entry>,
}

impl SubAgent {
    pub fn live(&self) -> bool {
        self.ended.is_none() && !matches!(self.status.as_str(), "cancelled" | "evicted" | "stopped")
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CmdSource {
    /// A Bash call of the model.
    Tool,
    /// A command the person typed (`!cmd`).
    User,
    /// A `process.*` event (a process the terminal runtime runs).
    Process,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CmdState {
    Running,
    Ok,
    Failed,
    NotRun,
}

#[derive(Debug, Clone, PartialEq)]
pub struct CommandRun {
    pub id: String,
    pub command: String,
    pub source: CmdSource,
    pub agent: Option<String>,
    pub state: CmdState,
    pub exit_code: Option<i64>,
    pub started: Option<i64>,
    pub duration_ms: u64,
    /// The last [`TAIL_LINES`] lines of output.
    pub tail: Vec<String>,
}

/// One check of a verification report (from the stored report JSON).
#[derive(Debug, Clone, PartialEq)]
pub struct CheckRow {
    pub kind: String,
    pub command: String,
    /// `passed`, `failed`, `unverified` or `blocked`.
    pub status: String,
    pub exit_code: Option<i64>,
    pub duration_ms: u64,
    /// The check's own evidence line(s).
    pub summary: String,
    pub blocked_reason: String,
    /// `passed/failed/skipped` counts parsed from the runner output.
    pub tests: Option<(u64, u64, u64)>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct VerifyRun {
    pub report_id: String,
    pub task_id: String,
    pub status: String,
    pub attempt: i64,
    /// The one-line summary of `verification.completed`.
    pub summary: String,
    pub ts: Option<i64>,
    /// Filled by [`VerifyRun::set_detail`] once the stored report was read.
    pub checks: Vec<CheckRow>,
    pub limitations: Vec<String>,
    pub changed_files: Vec<String>,
    pub loaded: bool,
}

impl VerifyRun {
    /// Takes the checks, limitations and changed files from a stored report
    /// (`Reader::verification_reports`, the `VerificationReport` JSON).
    pub fn set_detail(&mut self, report: &Value) {
        let s = |v: &Value, k: &str| v.get(k).and_then(Value::as_str).unwrap_or("").to_string();
        let list = |k: &str| -> Vec<String> {
            report
                .get(k)
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(|x| x.as_str().map(str::to_string))
                .collect()
        };
        self.checks = report
            .get("checks")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .map(|c| CheckRow {
                kind: s(c, "kind"),
                command: s(c, "command"),
                status: s(c, "status"),
                exit_code: c.get("exit_code").and_then(Value::as_i64),
                duration_ms: c.get("duration_ms").and_then(Value::as_u64).unwrap_or(0),
                summary: s(c, "summary"),
                blocked_reason: s(c, "blocked_reason"),
                tests: c.get("tests").filter(|t| !t.is_null()).map(|t| {
                    let n = |k: &str| t.get(k).and_then(Value::as_u64).unwrap_or(0);
                    (n("passed"), n("failed"), n("skipped"))
                }),
            })
            .collect();
        self.limitations = list("limitations");
        self.changed_files = list("changed_files");
        self.loaded = true;
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct BrowserCall {
    pub id: String,
    /// `browser_navigate`, `browser_click`, ...
    pub name: String,
    pub summary: String,
    pub state: CmdState,
    pub ts: Option<i64>,
    pub duration_ms: u64,
}

/// A screenshot the browser saved (`browser.frame`): metadata only, the file is the artifact.
#[derive(Debug, Clone, PartialEq)]
pub struct Frame {
    pub path: String,
    /// Origin of the page.
    pub url: String,
    pub w: u32,
    pub h: u32,
    pub bytes: u64,
    pub ts: Option<i64>,
}

/// A file the session changed: the sum of its edits and the latest diff.
#[derive(Debug, Clone, PartialEq)]
pub struct ChangedFile {
    pub path: String,
    pub added: u64,
    pub removed: u64,
    pub edits: u32,
    /// `ToolCompleted.diff` of the latest edit (`hidden` for a secret file).
    pub diff: Option<Value>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct AgentPanes {
    pub subs: Vec<SubAgent>,
    pub commands: Vec<CommandRun>,
    pub verify: Vec<VerifyRun>,
    pub browser: Vec<BrowserCall>,
    /// The last URL a `browser_navigate` named.
    pub url: String,
    /// Path of the newest screenshot the browser tool saved.
    pub screenshot: Option<String>,
    /// Every `browser.frame`, oldest first (the newest 50).
    pub frames: Vec<Frame>,
    pub files: Vec<ChangedFile>,
    /// Tool call id -> index in `commands` / `browser` / agent id, to match the completion.
    pending: Vec<(String, Pending)>,
}

#[derive(Debug, Clone, PartialEq)]
enum Pending {
    Command(usize),
    Browser(usize),
    /// A tool call of a sub-agent: (agent id, entry index).
    Sub(String, usize),
}

const FRAME_LIMIT: usize = 50;

/// `[exit code N]` at the end of a failed Bash result.
fn exit_code_of(output: &str) -> Option<i64> {
    let t = output.trim_end();
    let rest = t.strip_suffix(']')?;
    let at = rest.rfind("[exit code ")?;
    rest[at + "[exit code ".len()..].trim().parse().ok()
}

fn tail_of(text: &str) -> Vec<String> {
    let lines: Vec<&str> = text.lines().collect();
    lines[lines.len().saturating_sub(TAIL_LINES)..]
        .iter()
        .map(|l| l.to_string())
        .collect()
}

fn state_of(status: ToolStatus, not_run: bool) -> CmdState {
    if not_run {
        CmdState::NotRun
    } else if status == ToolStatus::Ok {
        CmdState::Ok
    } else {
        CmdState::Failed
    }
}

/// `Screenshot saved as artifact <path>` in a `browser_screenshot` result. The path is whatever the
/// OS prints (`C:\...\shot.png` on Windows), so it is only checked with `Path::is_absolute`; it is not
/// rewritten. Whether it may be opened is [`viewable_screenshot`]'s decision.
fn screenshot_path(output: &str) -> Option<String> {
    let rest = output.split("saved as artifact ").nth(1)?;
    let line = rest.lines().next()?;
    // the text version continues with " (images are not supported ...)"; a path may hold " (" itself
    let path = line.split(" (images are not supported").next()?.trim();
    let is_png = std::path::Path::new(path)
        .extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("png"));
    (is_png && std::path::Path::new(path).is_absolute()).then(|| path.to_string())
}

/// Screenshots larger than this are not loaded.
pub const MAX_SHOT_BYTES: u64 = 8 << 20;

/// The screenshot to draw: a PNG file of at most [`MAX_SHOT_BYTES`] that lies inside `root` (the
/// session's artifact directory). Both sides are canonicalized, so `..`, symlinks and, on Windows,
/// the verbatim `\\?\` prefix and the letter case compare as the file system sees them.
pub fn viewable_screenshot(root: &std::path::Path, path: &str) -> Option<std::path::PathBuf> {
    let real = std::fs::canonicalize(path).ok()?;
    let root = std::fs::canonicalize(root).ok()?;
    let md = std::fs::metadata(&real).ok()?;
    let png = real
        .extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("png"));
    (png && md.is_file() && md.len() <= MAX_SHOT_BYTES && real.starts_with(&root)).then_some(real)
}

impl AgentPanes {
    pub fn sub(&self, id: &str) -> Option<&SubAgent> {
        self.subs.iter().find(|s| s.id == id)
    }

    fn sub_mut(&mut self, id: &str) -> Option<&mut SubAgent> {
        self.subs.iter_mut().find(|s| s.id == id)
    }

    /// Sub-agents that are still working.
    pub fn live_subs(&self) -> usize {
        self.subs.iter().filter(|s| s.live()).count()
    }

    /// Files changed, `(added, removed)` over all of them.
    pub fn totals(&self) -> (u64, u64) {
        self.files
            .iter()
            .fold((0, 0), |(a, r), f| (a + f.added, r + f.removed))
    }

    fn push_entry(&mut self, agent: &str, e: Entry) -> Option<usize> {
        let s = self.sub_mut(agent)?;
        s.entries.push(e);
        if s.entries.len() > ENTRY_LIMIT {
            s.entries.remove(0);
            let id = agent.to_string();
            for (_, p) in &mut self.pending {
                if let Pending::Sub(a, i) = p
                    && *a == id
                {
                    *i = i.saturating_sub(1);
                }
            }
        }
        self.sub(agent).map(|s| s.entries.len() - 1)
    }

    fn take_pending(&mut self, id: &str) -> Option<Pending> {
        let at = self.pending.iter().position(|(k, _)| k == id)?;
        Some(self.pending.remove(at).1)
    }

    fn note_file(&mut self, d: &Value) {
        if d.get("hidden").and_then(Value::as_bool) == Some(true) && d.get("path").is_none() {
            return;
        }
        let Some(path) = d.get("path").and_then(Value::as_str) else {
            return;
        };
        let n = |k: &str| d.get(k).and_then(Value::as_u64).unwrap_or(0);
        let (a, r) = (n("added"), n("removed"));
        match self.files.iter_mut().find(|f| f.path == path) {
            Some(f) => {
                f.added += a;
                f.removed += r;
                f.edits += 1;
                f.diff = Some(d.clone());
            }
            None => self.files.push(ChangedFile {
                path: path.to_string(),
                added: a,
                removed: r,
                edits: 1,
                diff: Some(d.clone()),
            }),
        }
    }

    pub fn apply(&mut self, e: &Envelope) {
        let ts = parse_ts(&e.ts);
        let agent = e.agent_id.clone();
        match &e.event {
            Event::SubagentStarted(i) => {
                if self.sub(&i.agent_id).is_none() {
                    self.subs.push(SubAgent {
                        id: i.agent_id.clone(),
                        name: i.name.clone().unwrap_or_else(|| i.agent_type.clone()),
                        kind: i.agent_type.clone(),
                        background: i.background,
                        status: "running".into(),
                        model: String::new(),
                        started: ts,
                        ended: None,
                        reason: String::new(),
                        tool_calls: 0,
                        entries: vec![],
                    });
                }
            }
            Event::SubagentStatusChanged { agent_id, to, .. } => {
                if let Some(s) = self.sub_mut(agent_id) {
                    s.status.clone_from(to);
                }
            }
            Event::SubagentStopped(i) => {
                if let Some(s) = self.sub_mut(&i.agent_id) {
                    s.ended = ts;
                    s.status = "stopped".into();
                    s.reason = i.reason.clone().unwrap_or_default();
                }
            }
            Event::TurnStarted { model, .. } => {
                if let Some(s) = agent.as_deref().and_then(|a| self.sub_mut(a)) {
                    s.model.clone_from(model);
                }
            }
            Event::AssistantMessage { content, .. } => {
                if let Some(a) = &agent {
                    let text = content
                        .iter()
                        .filter(|b| b.get("type").and_then(Value::as_str) == Some("text"))
                        .filter_map(|b| b.get("text").and_then(Value::as_str))
                        .collect::<Vec<_>>()
                        .join("\n");
                    if !text.trim().is_empty() {
                        self.push_entry(
                            a,
                            Entry {
                                ts,
                                kind: EntryKind::Text,
                                text,
                                failed: false,
                            },
                        );
                    }
                }
            }
            Event::ToolRequested {
                tool_call_id,
                name,
                input,
                origin,
                ..
            } => {
                let summary = summarize_tool(name, input);
                if let Some(a) = &agent {
                    let known = self.sub(a).is_some();
                    if known && let Some(s) = self.sub_mut(a) {
                        s.tool_calls += 1;
                    }
                    if let Some(i) = self.push_entry(
                        a,
                        Entry {
                            ts,
                            kind: EntryKind::Tool,
                            text: format!("{name} {summary}"),
                            failed: false,
                        },
                    ) {
                        self.pending
                            .push((tool_call_id.clone(), Pending::Sub(a.clone(), i)));
                    }
                }
                if name == "Bash" {
                    let command = input
                        .get("command")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .chars()
                        .take(500)
                        .collect();
                    self.commands.push(CommandRun {
                        id: tool_call_id.clone(),
                        command,
                        source: if origin.as_deref() == Some("user") {
                            CmdSource::User
                        } else {
                            CmdSource::Tool
                        },
                        agent: agent.clone(),
                        state: CmdState::Running,
                        exit_code: None,
                        started: ts,
                        duration_ms: 0,
                        tail: vec![],
                    });
                    let at = self.commands.len() - 1;
                    self.pending
                        .push((tool_call_id.clone(), Pending::Command(at)));
                    if self.commands.len() > COMMAND_LIMIT {
                        self.commands.remove(0);
                        self.pending.retain_mut(|(_, p)| match p {
                            Pending::Command(i) => i.checked_sub(1).map(|n| *i = n).is_some(),
                            _ => true,
                        });
                    }
                } else if name.starts_with("browser_") {
                    self.browser.push(BrowserCall {
                        id: tool_call_id.clone(),
                        name: name.clone(),
                        summary: browser_summary(name, input),
                        state: CmdState::Running,
                        ts,
                        duration_ms: 0,
                    });
                    if name == "browser_navigate"
                        && let Some(u) = input.get("url").and_then(Value::as_str)
                    {
                        self.url = u.to_string();
                    }
                    let at = self.browser.len() - 1;
                    self.pending
                        .push((tool_call_id.clone(), Pending::Browser(at)));
                }
            }
            Event::ToolOutput {
                tool_call_id, text, ..
            } => {
                let at = self.pending.iter().find_map(|(k, p)| match p {
                    Pending::Command(i) if k == tool_call_id => Some(*i),
                    _ => None,
                });
                if let Some(c) = at.and_then(|i| self.commands.get_mut(i)) {
                    let mut all = c.tail.join("\n");
                    all.push_str(text);
                    c.tail = tail_of(&all);
                }
            }
            Event::ToolCompleted {
                tool_call_id,
                status,
                not_run,
                duration_ms,
                output,
                preview,
                diff,
                ..
            } => {
                let state = state_of(*status, *not_run);
                let out = output.as_deref().unwrap_or(preview);
                match self.take_pending(tool_call_id) {
                    Some(Pending::Command(i)) => {
                        if let Some(c) = self.commands.get_mut(i) {
                            c.state = state;
                            c.duration_ms = *duration_ms;
                            c.exit_code = match state {
                                CmdState::Ok => Some(0),
                                // `process.exited` (sent before this) knows it; old logs do not
                                CmdState::Failed => c.exit_code.or_else(|| exit_code_of(out)),
                                _ => None,
                            };
                            if !out.is_empty() {
                                c.tail = tail_of(out);
                            }
                        }
                    }
                    Some(Pending::Browser(i)) => {
                        if let Some(b) = self.browser.get_mut(i) {
                            b.state = state;
                            b.duration_ms = *duration_ms;
                            if b.name == "browser_screenshot"
                                && let Some(p) = screenshot_path(out)
                            {
                                self.screenshot = Some(p);
                            }
                        }
                    }
                    Some(Pending::Sub(a, i)) => {
                        if let Some(en) = self.sub_mut(&a).and_then(|s| s.entries.get_mut(i)) {
                            en.failed = state == CmdState::Failed;
                        }
                    }
                    None => {}
                }
                if let Some(d) = diff {
                    self.note_file(d);
                }
            }
            // A foreground Bash call is `process_id == tool_call_id`: its row exists already.
            Event::ProcessSpawned(p) if p.tool_call_id.as_ref() != Some(&p.process_id) => {
                self.commands.push(CommandRun {
                    id: p.process_id.clone(),
                    command: p
                        .command
                        .clone()
                        .or_else(|| p.name.clone())
                        .unwrap_or_else(|| p.process_id.clone()),
                    source: CmdSource::Process,
                    agent: agent.clone(),
                    state: CmdState::Running,
                    exit_code: None,
                    started: ts,
                    duration_ms: 0,
                    tail: vec![],
                })
            }
            Event::ProcessOutput {
                process_id, text, ..
            } => {
                // Foreground rows already follow `tool.output`.
                if let Some(c) = self
                    .commands
                    .iter_mut()
                    .rev()
                    .find(|c| c.source == CmdSource::Process && c.id == *process_id)
                {
                    let mut all = c.tail.join("\n");
                    all.push_str(text);
                    c.tail = tail_of(&all);
                }
            }
            Event::ProcessExited(p) => {
                if let Some(c) = self
                    .commands
                    .iter_mut()
                    .rev()
                    .find(|c| c.id == p.process_id && c.source != CmdSource::Process)
                    .filter(|_| p.tool_call_id.as_ref() == Some(&p.process_id))
                {
                    // the real exit code, instead of `[exit code N]` in the result text
                    c.exit_code = p.exit_code;
                } else if let Some(c) = self
                    .commands
                    .iter_mut()
                    .rev()
                    .find(|c| c.source == CmdSource::Process && c.id == p.process_id)
                {
                    c.exit_code = p.exit_code;
                    c.duration_ms = p
                        .duration_ms
                        .or_else(|| ts.zip(c.started).map(|(e, s)| (e - s).max(0) as u64))
                        .unwrap_or(0);
                    c.state = if p.exit_code == Some(0) {
                        CmdState::Ok
                    } else {
                        CmdState::Failed
                    };
                }
            }
            Event::BrowserFrame {
                path,
                url,
                w,
                h,
                bytes,
            } => {
                self.screenshot = Some(path.clone());
                self.frames.push(Frame {
                    path: path.clone(),
                    url: url.clone(),
                    w: *w,
                    h: *h,
                    bytes: *bytes,
                    ts,
                });
                if self.frames.len() > FRAME_LIMIT {
                    self.frames.remove(0);
                }
            }
            Event::VerificationCompleted {
                report_id,
                status,
                attempt,
                summary,
            } => self.verify.push(VerifyRun {
                report_id: report_id.clone(),
                task_id: e.task_id.clone().unwrap_or_default(),
                status: status.clone(),
                attempt: *attempt,
                summary: summary.clone(),
                ts,
                checks: vec![],
                limitations: vec![],
                changed_files: vec![],
                loaded: false,
            }),
            _ => {}
        }
    }

    /// Reports whose checks were not read from the store yet: `(report_id, task_id, attempt)`.
    pub fn unloaded_reports(&self) -> Vec<(String, String, i64)> {
        self.verify
            .iter()
            .filter(|v| !v.loaded && !v.task_id.is_empty())
            .map(|v| (v.report_id.clone(), v.task_id.clone(), v.attempt))
            .collect()
    }

    /// Fills the report `report_id` from its stored JSON.
    pub fn set_report(&mut self, report_id: &str, report: &Value) {
        if let Some(v) = self.verify.iter_mut().find(|v| v.report_id == report_id) {
            v.set_detail(report);
        }
    }
}

/// The stored report behind a `verification.completed` event: the one with this report id, else the
/// one of this attempt (the store keys rows by its own id). Blocking: call it off the UI thread.
pub fn read_report(
    r: &synthesis_session::Reader,
    task_id: &str,
    report_id: &str,
    attempt: i64,
) -> Option<Value> {
    let all = r.verification_reports(task_id).ok()?;
    let by_id = all
        .iter()
        .position(|(_, _, v)| v.get("id").and_then(Value::as_str) == Some(report_id));
    let at = by_id.or_else(|| all.iter().rposition(|(a, _, _)| *a == attempt))?;
    Some(all.into_iter().nth(at)?.2)
}

fn browser_summary(name: &str, input: &Value) -> String {
    let s = |k: &str| input.get(k).and_then(Value::as_str);
    let t = match name {
        "browser_navigate" => s("url"),
        "browser_click" | "browser_fill" | "browser_fill_secret" => {
            s("element").or_else(|| s("ref")).or_else(|| s("selector"))
        }
        "browser_press_key" => s("key"),
        _ => None,
    };
    t.map_or_else(
        || summarize_tool(name, input),
        |t| t.chars().take(120).collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn ev(seq: u64, agent: Option<&str>, ty: &str, payload: Value) -> Envelope {
        serde_json::from_value(
            json!({"v": 1, "seq": seq, "ts": format!("2026-11-07T10:00:{seq:02}Z"),
            "session_id": "s1", "task_id": "t1", "turn_id": null, "agent_id": agent,
            "type": ty, "payload": payload}),
        )
        .unwrap()
    }

    fn run(events: Vec<Envelope>) -> AgentPanes {
        let mut p = AgentPanes::default();
        for e in &events {
            p.apply(e);
        }
        p
    }

    fn completed(id: &str, name: &str, status: &str, out: &str, diff: Option<Value>) -> Value {
        let mut v = json!({"tool_call_id": id, "name": name, "status": status,
            "is_error": status != "ok", "not_run": false, "duration_ms": 12, "preview": out,
            "bytes": 0, "truncated": false, "output": out});
        if let Some(d) = diff {
            v["diff"] = d;
        }
        v
    }

    #[test]
    fn a_sub_agent_gets_its_own_transcript_and_status() {
        let p = run(vec![
            ev(
                1,
                None,
                "subagent.started",
                json!({"agent_id": "a1", "agent_type": "explore", "name": "scout"}),
            ),
            ev(
                2,
                Some("a1"),
                "turn.started",
                json!({"model": "m-small", "provider": "p", "iteration": 1}),
            ),
            ev(
                3,
                Some("a1"),
                "tool.requested",
                json!({"tool_call_id": "c1", "name": "Read", "input": {"file_path": "a.rs"},
                    "input_truncated": false}),
            ),
            ev(
                4,
                Some("a1"),
                "tool.completed",
                completed("c1", "Read", "error", "nope", None),
            ),
            ev(
                5,
                Some("a1"),
                "assistant.message",
                json!({"stop_reason": "end_turn", "content": [{"type": "text", "text": "found it"}]}),
            ),
            ev(
                6,
                None,
                "subagent.status_changed",
                json!({"agent_id": "a1", "name": "scout", "from": "running", "to": "waiting_approval"}),
            ),
        ]);
        let s = p.sub("a1").unwrap();
        assert_eq!((s.name.as_str(), s.model.as_str()), ("scout", "m-small"));
        assert_eq!(s.status, "waiting_approval");
        assert!(s.live());
        assert_eq!(s.tool_calls, 1);
        assert_eq!(s.entries.len(), 2);
        assert_eq!(s.entries[0].text, "Read a.rs");
        assert!(s.entries[0].failed, "the completion marks the call");
        assert_eq!(s.entries[1].kind, EntryKind::Text);
        let mut p = p;
        p.apply(&ev(
            7,
            None,
            "subagent.stopped",
            json!({"agent_id": "a1", "agent_type": "explore", "reason": "done"}),
        ));
        assert!(!p.sub("a1").unwrap().live());
        assert_eq!(p.live_subs(), 0);
    }

    #[test]
    fn bash_calls_become_the_command_history_with_exit_codes_and_tails() {
        let p = run(vec![
            ev(
                1,
                None,
                "tool.requested",
                json!({"tool_call_id": "b1", "name": "Bash", "input": {"command": "cargo test"},
                    "input_truncated": false}),
            ),
            ev(
                2,
                None,
                "tool.output",
                json!({"tool_call_id": "b1", "stream": "merged", "text": "line1\nline2\n", "seq": 1}),
            ),
            ev(
                3,
                None,
                "tool.completed",
                completed("b1", "Bash", "ok", "line1\nline2\ndone", None),
            ),
            ev(
                4,
                Some("a1"),
                "tool.requested",
                json!({"tool_call_id": "b2", "name": "Bash", "input": {"command": "false"},
                    "input_truncated": false, "origin": "user"}),
            ),
            ev(
                5,
                Some("a1"),
                "tool.completed",
                completed("b2", "Bash", "error", "boom\n\n[exit code 3]", None),
            ),
        ]);
        assert_eq!(p.commands.len(), 2);
        let (a, b) = (&p.commands[0], &p.commands[1]);
        assert_eq!(
            (a.state, a.exit_code, a.duration_ms),
            (CmdState::Ok, Some(0), 12)
        );
        assert_eq!(a.tail, ["line1", "line2", "done"]);
        assert_eq!((b.state, b.exit_code), (CmdState::Failed, Some(3)));
        assert_eq!(b.source, CmdSource::User);
        assert_eq!(b.agent.as_deref(), Some("a1"));
        assert_eq!(exit_code_of("x [exit code 12]"), Some(12));
        assert_eq!(exit_code_of("no code"), None);
    }

    #[test]
    fn processes_join_the_command_history() {
        let p = run(vec![
            ev(
                1,
                None,
                "process.spawned",
                json!({"process_id": "p1", "name": "npm run dev", "pid": 7}),
            ),
            ev(
                3,
                None,
                "process.exited",
                json!({"process_id": "p1", "exit_code": 1}),
            ),
        ]);
        let c = &p.commands[0];
        assert_eq!(
            (c.source, c.state, c.exit_code),
            (CmdSource::Process, CmdState::Failed, Some(1))
        );
        assert_eq!(c.duration_ms, 2000);
    }

    #[test]
    fn process_events_give_the_exit_code_and_background_rows() {
        let p = run(vec![
            ev(
                1,
                None,
                "tool.requested",
                json!({"tool_call_id": "b1", "name": "Bash", "input": {"command": "exit 7"},
                    "input_truncated": false}),
            ),
            ev(
                2,
                None,
                "process.spawned",
                json!({"process_id": "b1", "tool_call_id": "b1", "command": "exit 7"}),
            ),
            ev(
                3,
                None,
                "process.exited",
                json!({"process_id": "b1", "tool_call_id": "b1", "exit_code": 7, "duration_ms": 5}),
            ),
            // no `[exit code N]` in the text: only the event knows
            ev(
                4,
                None,
                "tool.completed",
                completed("b1", "Bash", "error", "boom", None),
            ),
            ev(
                5,
                None,
                "process.spawned",
                json!({"process_id": "proc_2", "tool_call_id": "b2", "command": "npm run dev"}),
            ),
            ev(
                6,
                None,
                "process.output",
                json!({"process_id": "proc_2", "tool_call_id": "b2", "text": "ready\n", "seq": 1}),
            ),
            ev(
                7,
                None,
                "process.exited",
                json!({"process_id": "proc_2", "signal": "SIGKILL", "duration_ms": 900}),
            ),
        ]);
        assert_eq!(p.commands.len(), 2, "the foreground call is one row");
        assert_eq!(
            (p.commands[0].state, p.commands[0].exit_code),
            (CmdState::Failed, Some(7))
        );
        let bg = &p.commands[1];
        assert_eq!(
            (bg.source, bg.state),
            (CmdSource::Process, CmdState::Failed)
        );
        assert_eq!((bg.command.as_str(), bg.duration_ms), ("npm run dev", 900));
        assert_eq!(bg.tail, ["ready"]);
    }

    #[test]
    fn browser_frames_are_kept_and_the_latest_is_the_screenshot() {
        let frame = |n: u32| {
            json!({"path": format!("/a/screenshot-{n}.png"), "url": "https://example.com",
                "w": 1280, "h": 720, "bytes": 99})
        };
        let p = run(vec![
            ev(1, None, "browser.frame", frame(1)),
            ev(2, None, "browser.frame", frame(2)),
        ]);
        assert_eq!(p.frames.len(), 2);
        assert_eq!(p.screenshot.as_deref(), Some("/a/screenshot-2.png"));
        assert_eq!(
            (p.frames[0].w, p.frames[0].h, p.frames[0].bytes),
            (1280, 720, 99)
        );
    }

    #[test]
    fn edits_sum_per_file_and_keep_the_latest_diff() {
        let d = |a, r| json!({"path": "src/a.rs", "kind": "edit", "added": a, "removed": r, "hunks": []});
        let p = run(vec![
            ev(
                1,
                None,
                "tool.completed",
                completed("e1", "Edit", "ok", "", Some(d(3, 1))),
            ),
            ev(
                2,
                Some("a1"),
                "tool.completed",
                completed("e2", "Edit", "ok", "", Some(d(2, 0))),
            ),
            ev(
                3,
                None,
                "tool.completed",
                completed(
                    "e3",
                    "Write",
                    "ok",
                    "",
                    Some(json!({"path": "b.rs", "added": 5, "removed": 0, "hunks": []})),
                ),
            ),
        ]);
        assert_eq!(p.files.len(), 2);
        assert_eq!(
            (p.files[0].added, p.files[0].removed, p.files[0].edits),
            (5, 1, 2)
        );
        assert_eq!(p.totals(), (10, 1));
    }

    #[test]
    fn browser_calls_keep_the_url_and_the_screenshot_path() {
        let shot = std::env::temp_dir().join("x").join("shot-1.png");
        let p = run(vec![
            ev(
                1,
                None,
                "tool.requested",
                json!({"tool_call_id": "w1", "name": "browser_navigate",
                    "input": {"url": "https://example.com"}, "input_truncated": false}),
            ),
            ev(
                2,
                None,
                "tool.completed",
                completed("w1", "browser_navigate", "ok", "ok", None),
            ),
            ev(
                3,
                None,
                "tool.requested",
                json!({"tool_call_id": "w2", "name": "browser_screenshot", "input": {}, "input_truncated": false}),
            ),
            ev(
                4,
                None,
                "tool.completed",
                completed(
                    "w2",
                    "browser_screenshot",
                    "ok",
                    &format!("Screenshot saved as artifact {}", shot.display()),
                    None,
                ),
            ),
        ]);
        assert_eq!(p.url, "https://example.com");
        assert_eq!(p.browser.len(), 2);
        assert_eq!(p.browser[0].summary, "https://example.com");
        assert_eq!(p.screenshot.as_deref(), shot.to_str());
        let spaced = std::env::temp_dir().join("a (b)").join("c.PNG");
        let text = format!(
            "Screenshot saved as artifact {} (images are not supported by this model; page summary follows).\nx",
            spaced.display()
        );
        assert_eq!(screenshot_path(&text).as_deref(), spaced.to_str());
        assert_eq!(screenshot_path("saved as artifact rel.png"), None);
    }

    #[test]
    fn only_a_small_png_inside_the_artifact_dir_is_viewable() {
        let t = tempfile::tempdir().unwrap();
        let root = t.path().join("session");
        std::fs::create_dir_all(root.join("browser")).unwrap();
        let inside = root.join("browser").join("s.png");
        std::fs::write(&inside, b"png").unwrap();
        let outside = t.path().join("other.png");
        std::fs::write(&outside, b"png").unwrap();
        let text = root.join("browser").join("t.txt");
        std::fs::write(&text, b"x").unwrap();
        let s = |p: &std::path::Path| p.to_str().unwrap().to_string();
        assert!(viewable_screenshot(&root, &s(&inside)).is_some());
        assert!(
            viewable_screenshot(&root, &s(&outside)).is_none(),
            "outside the root"
        );
        assert!(viewable_screenshot(&root, &s(&text)).is_none(), "not a png");
        let escape = root.join("browser").join("..").join("..").join("other.png");
        assert!(
            viewable_screenshot(&root, &s(&escape)).is_none(),
            "dot-dot escape"
        );
        assert!(viewable_screenshot(&root, &s(&root.join("browser").join("gone.png"))).is_none());
        let big = root.join("big.png");
        std::fs::File::create(&big)
            .unwrap()
            .set_len(MAX_SHOT_BYTES + 1)
            .unwrap();
        assert!(viewable_screenshot(&root, &s(&big)).is_none(), "size cap");
    }

    #[test]
    fn a_verification_report_is_listed_then_filled_from_the_store_json() {
        let mut p = run(vec![ev(
            1,
            None,
            "verification.completed",
            json!({"report_id": "r1", "status": "failed", "attempt": 1,
                "summary": "failed: 0 passed, 1 failed, 0 blocked, 1 unverified check(s); 1 limitation(s)"}),
        )]);
        assert_eq!(
            p.unloaded_reports(),
            [("r1".to_string(), "t1".to_string(), 1)]
        );
        p.set_report(
            "r1",
            &json!({"checks": [
                {"kind": "unit", "command": "cargo test", "status": "failed", "exit_code": 101,
                 "duration_ms": 900, "summary": "1 failed", "blocked_reason": null,
                 "tests": {"passed": 4, "failed": 1, "skipped": 0}},
                {"kind": "manual", "command": null, "status": "unverified", "exit_code": null,
                 "duration_ms": 0, "summary": "", "blocked_reason": "needs a person"}],
              "limitations": ["no browser"], "changed_files": ["a.rs"]}),
        );
        let v = &p.verify[0];
        assert!(v.loaded && p.unloaded_reports().is_empty());
        assert_eq!(v.checks.len(), 2);
        assert_eq!(v.checks[0].tests, Some((4, 1, 0)));
        assert_eq!(v.checks[0].exit_code, Some(101));
        assert_eq!(v.checks[1].command, "");
        assert_eq!(v.checks[1].blocked_reason, "needs a person");
        assert_eq!((v.limitations.len(), v.changed_files.len()), (1, 1));
    }

    #[test]
    fn the_command_list_is_bounded_and_completions_still_match() {
        let mut p = AgentPanes::default();
        for i in 0..COMMAND_LIMIT + 5 {
            p.apply(&ev(
                1,
                None,
                "tool.requested",
                json!({"tool_call_id": format!("b{i}"), "name": "Bash", "input": {"command": format!("c{i}")},
                    "input_truncated": false}),
            ));
        }
        assert_eq!(p.commands.len(), COMMAND_LIMIT);
        let last = format!("b{}", COMMAND_LIMIT + 4);
        p.apply(&ev(
            2,
            None,
            "tool.completed",
            completed(&last, "Bash", "ok", "x", None),
        ));
        assert_eq!(p.commands.last().unwrap().state, CmdState::Ok);
        assert_eq!(p.commands[0].command, "c5");
    }
}
