//! Live state of the desktop TUI view: the `ViewState` the engine's events feed, the dialogs waiting
//! for the person, the prompt queue and history, the sidebar selection. Plain Rust, no gpui; the window
//! (`tui_view`) reads it and calls the methods below, the pump applies `UiMsg`s with [`Live::on_msg`].
//! The rules follow the classic TUI (`docs/TUI.md`; `crates/synthesis-tui/src/app.rs`).

use std::collections::{HashSet, VecDeque};
use std::sync::Arc;

use serde_json::Value;
use synthesis_harness::{UserReply, sanitize_for_display};
use synthesis_permissions::{Mode, Suggestion};
use synthesis_tools::{NeedsUser, Question};
use synthesis_tui_base::complete::Command;
use synthesis_tui_render::dialogs::permission_options;
use synthesis_view::convo::Item;
use synthesis_view::state::ViewState;

use crate::engine::{AskApproval, Choice, Engine, McpInfo, UiMsg};

/// Enter and digits are ignored this long after a dialog comes to the front: a key typed before it
/// appeared must not answer it (the TUI's `ARM_MS`). Esc still rejects.
pub const ARM_MS: i64 = 400;
/// The second Ctrl+C that quits has to come within this long (the TUI's `QUIT_ARM_MS`).
pub const QUIT_ARM_MS: i64 = 1500;
/// A session switch with a task running needs a second click within this long.
pub const SWITCH_ARM_MS: i64 = 5000;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Focus {
    Chat,
    Workspace,
    Running,
}

impl Focus {
    /// Tab: chat -> Workspace -> Running -> chat.
    pub fn next(self) -> Self {
        match self {
            Focus::Chat => Focus::Workspace,
            Focus::Workspace => Focus::Running,
            Focus::Running => Focus::Chat,
        }
    }
}

#[derive(Clone, Debug)]
pub enum DialogKind {
    Approval(AskApproval),
    Plan {
        id: String,
        plan: String,
    },
    Question {
        id: String,
        questions: Vec<Question>,
        /// Index of the question shown.
        cur: usize,
        /// Chosen option indexes per question.
        picked: Vec<Vec<usize>>,
    },
}

#[derive(Clone, Debug)]
pub struct Dialog {
    pub kind: DialogKind,
    /// (key, label) the person can pick; the last one rejects.
    pub options: Vec<(String, String)>,
    pub sel: usize,
    /// Enter and digits do nothing before this (ms).
    pub armed_until: i64,
}

impl Dialog {
    pub fn id(&self) -> &str {
        match &self.kind {
            DialogKind::Approval(a) => &a.id,
            DialogKind::Plan { id, .. } | DialogKind::Question { id, .. } => id,
        }
    }

    /// The headline.
    pub fn title(&self) -> String {
        match &self.kind {
            DialogKind::Approval(a) if a.dialog == "outside_read" => {
                format!("{} wants to read outside the working directories", a.tool)
            }
            DialogKind::Approval(a) => format!("{} needs your permission", a.tool),
            DialogKind::Plan { .. } => "Ready to code? Here is the plan".into(),
            DialogKind::Question { .. } => "Synthesis has a question".into(),
        }
    }
}

/// The suggestion in the shape `permission_options` reads (the TUI's `suggestion_view`).
fn suggestion_view(s: &Suggestion) -> Value {
    use serde_json::json;
    match s {
        Suggestion::AddRules { rules, behavior } => json!({
            "type": "addRules",
            "behavior": format!("{behavior:?}").to_lowercase(),
            "rules": rules.iter().map(|r| json!({
                "toolName": sanitize_for_display(&r.tool_name),
                "ruleContent": r.rule_content.as_deref().map(sanitize_for_display).unwrap_or_default(),
            })).collect::<Vec<_>>(),
        }),
        Suggestion::SetMode(m) => json!({"type": "setMode", "mode": m.as_str()}),
    }
}

/// What the person typed became.
#[derive(Debug, PartialEq, Eq)]
pub enum Submitted {
    /// Nothing to do (empty line, or the text went to the queue).
    Nothing,
    /// Hand it to the engine.
    Send(String),
    Quit,
}

pub const NOT_YET: &str = "not available in the desktop app yet";

/// The chat's own state beyond the events.
pub struct Live {
    pub view: ViewState,
    pub engine: Option<Arc<Engine>>,
    /// The engine is being built (first start or a relaunch).
    pub starting: bool,
    /// A task is running (from `send` to the engine's `TaskDone`).
    pub busy: bool,
    pub dialogs: Vec<Dialog>,
    pub queue: VecDeque<String>,
    pub show_thinking: bool,
    /// Internal rows (`docs/TUI.md` Events) show up in the chat.
    pub verbose: bool,
    /// Ctrl+O: every tool output whole.
    pub expand_all: bool,
    pub expanded: HashSet<String>,
    pub focus: Focus,
    /// Selected workspace (its `key`); Running lists its sessions.
    pub ws_key: String,
    pub run_sel: usize,
    pub quit_armed_until: Option<i64>,
    /// A second click/Enter on this session id within [`SWITCH_ARM_MS`] switches while a task runs.
    pub switch_armed: Option<(String, i64)>,
    pub history: Vec<String>,
    pub mcp: Vec<McpInfo>,
    pub commands: Vec<Command>,
    pub sid: String,
    pub cwd: String,
    pub trusted: bool,
}

impl Live {
    pub fn new() -> Self {
        Self {
            view: ViewState::default(),
            engine: None,
            starting: true,
            busy: false,
            dialogs: vec![],
            queue: VecDeque::new(),
            show_thinking: false,
            verbose: false,
            expand_all: false,
            expanded: HashSet::new(),
            focus: Focus::Chat,
            ws_key: String::new(),
            run_sel: 0,
            quit_armed_until: None,
            switch_armed: None,
            history: vec![],
            mcp: vec![],
            commands: vec![],
            sid: String::new(),
            cwd: String::new(),
            trusted: false,
        }
    }

    /// A line in the chat that is not from the model.
    pub fn note(&mut self, text: impl Into<String>, warning: bool) {
        self.view.convo.items.push(Item::Notice {
            warning,
            text: text.into(),
        });
    }

    /// Applies what the engine sent. Returns true when a task just ended (the caller refreshes the
    /// store panels and sends the next queued line).
    pub fn on_msg(&mut self, m: UiMsg, now: i64) -> bool {
        match m {
            UiMsg::Event(e) => self.view.apply(&e),
            UiMsg::BadEvent(t) => self.note(t, true),
            UiMsg::Approval(a) => self.on_approval(*a, now),
            UiMsg::Input(i) => self.on_input(i.id, i.req, now),
            UiMsg::Withdraw(id) => {
                let n = self.dialogs.len();
                self.dialogs.retain(|d| d.id() != id);
                if self.dialogs.len() != n {
                    self.note("the request was withdrawn (cancelled or expired)", true);
                    self.arm(now);
                }
            }
            UiMsg::TaskDone(r) => {
                self.busy = false;
                self.dialogs.clear();
                if let Err(e) = r {
                    self.note(format!("task failed to run: {e}"), true);
                }
                return true;
            }
        }
        false
    }

    fn arm(&mut self, now: i64) {
        if let Some(d) = self.dialogs.first_mut() {
            d.armed_until = d.armed_until.max(now + ARM_MS);
        }
    }

    fn push_dialog(&mut self, d: Dialog, now: i64) {
        self.dialogs.push(d);
        if self.dialogs.len() == 1 {
            self.arm(now);
        }
    }

    fn on_approval(&mut self, a: AskApproval, now: i64) {
        let views: Vec<Value> = a.suggestions.iter().map(suggestion_view).collect();
        // work done for another session (peers) says so in the dialog
        let mut a = a;
        a.reason = match &a.origin {
            Some(o) => format!(
                "for {}: {}",
                sanitize_for_display(o),
                sanitize_for_display(&a.reason)
            ),
            None => sanitize_for_display(&a.reason),
        };
        self.push_dialog(
            Dialog {
                options: permission_options(&views, false),
                kind: DialogKind::Approval(a),
                sel: 0,
                armed_until: 0,
            },
            now,
        );
    }

    fn on_input(&mut self, id: String, req: NeedsUser, now: i64) {
        let d = match req {
            NeedsUser::ExitPlanMode { plan } => Dialog {
                options: vec![
                    ("manual".into(), "Yes, manually approve edits".into()),
                    ("accept".into(), "Yes, and auto-accept edits".into()),
                    ("keep".into(), "No, keep planning (esc)".into()),
                ],
                kind: DialogKind::Plan {
                    id,
                    plan: sanitize_for_display(&plan),
                },
                sel: 0,
                armed_until: 0,
            },
            NeedsUser::AskUserQuestion { questions } => {
                let picked = vec![vec![]; questions.len()];
                let mut d = Dialog {
                    options: vec![],
                    kind: DialogKind::Question {
                        id,
                        questions,
                        cur: 0,
                        picked,
                    },
                    sel: 0,
                    armed_until: 0,
                };
                d.options = question_options(&d);
                d
            }
        };
        self.push_dialog(d, now);
    }

    /// Up/Down in the front dialog.
    pub fn move_sel(&mut self, down: bool) {
        if let Some(d) = self.dialogs.first_mut() {
            let n = d.options.len().max(1);
            d.sel = if down {
                (d.sel + 1) % n
            } else {
                (d.sel + n - 1) % n
            };
        }
    }

    /// Picks option `i` (digit key or click) of the front dialog; Enter picks the highlighted one.
    /// `None` while the dialog is still arming (Esc and clicks on "no" are never held back).
    pub fn pick(&mut self, i: usize, now: i64) -> Option<String> {
        let d = self.dialogs.first()?;
        if now < d.armed_until || i >= d.options.len() {
            return None;
        }
        self.activate(i)
    }

    /// Esc / Ctrl+C in a dialog: the last option ("no" / "keep planning").
    pub fn reject(&mut self) -> Option<String> {
        let n = self.dialogs.first()?.options.len();
        self.activate(n.checked_sub(1)?)
    }

    /// Acts on option `i`. A multi-question dialog advances or toggles instead of answering.
    fn activate(&mut self, i: usize) -> Option<String> {
        let d = self.dialogs.first_mut()?;
        let key = d.options[i].0.clone();
        if let DialogKind::Question {
            questions,
            cur,
            picked,
            ..
        } = &mut d.kind
        {
            match key.as_str() {
                "no" => {}
                "next" => {
                    *cur += 1;
                    d.sel = 0;
                    d.options = question_options_of(&d.kind);
                    return None;
                }
                "done" => {}
                k => {
                    // an option of the current question: toggle (multi) or choose (single)
                    let idx: usize = k.strip_prefix("opt")?.parse().ok()?;
                    let multi = questions[*cur].multi_select;
                    let p = &mut picked[*cur];
                    if let Some(pos) = p.iter().position(|x| *x == idx) {
                        p.remove(pos);
                    } else if multi {
                        p.push(idx);
                    } else {
                        *p = vec![idx];
                    }
                    d.options = question_options_of(&d.kind);
                    d.sel = i;
                    return None;
                }
            }
        }
        Some(key)
    }

    /// Sends the answer of the front dialog to the engine and drops it. `key` comes from
    /// [`Live::pick`] / [`Live::reject`].
    pub fn answer(&mut self, key: &str, now: i64) {
        if self.dialogs.is_empty() {
            return;
        }
        let d = self.dialogs.remove(0);
        if let Some(e) = &self.engine {
            match d.kind {
                DialogKind::Approval(a) => {
                    let choice = match key {
                        "yes" => Choice::Once,
                        "always" => Choice::Always,
                        _ => Choice::Deny,
                    };
                    e.answer(&a.id, choice);
                }
                DialogKind::Plan { id, .. } => {
                    let (approved, mode) = match key {
                        "manual" => (true, Some(Mode::Default)),
                        "accept" => (true, Some(Mode::AcceptEdits)),
                        _ => (false, None),
                    };
                    if let Some(m) = mode {
                        e.set_mode(m);
                    }
                    let message = (!approved).then(|| {
                        "The user doesn't want to proceed with this plan yet. Stay in plan mode, keep planning, and ask what to change.".to_string()
                    });
                    e.answer_input(
                        &id,
                        Some(UserReply::Plan {
                            approved,
                            message,
                            mode,
                        }),
                    );
                }
                DialogKind::Question {
                    id,
                    questions,
                    picked,
                    ..
                } => {
                    let mut answers = serde_json::Map::new();
                    for (q, p) in questions.iter().zip(&picked) {
                        let text = if key == "no" {
                            "The user declined to answer.".to_string()
                        } else {
                            p.iter()
                                .filter_map(|k| q.options.get(*k).map(|o| o.label.clone()))
                                .collect::<Vec<_>>()
                                .join(", ")
                        };
                        answers.insert(q.question.clone(), Value::String(text));
                    }
                    e.answer_input(&id, Some(UserReply::Answers(Value::Object(answers))));
                }
            }
        }
        self.arm(now);
    }

    /// Esc on the prompt: reject a dialog, else interrupt the task and drop what was queued
    /// behind it. Returns true when it did something.
    pub fn interrupt(&mut self) -> bool {
        if !self.dialogs.is_empty() {
            // the engine side learns of it through the answer
            if let Some(k) = self.reject() {
                self.answer(&k, 0);
            }
            return true;
        }
        if !self.busy {
            return false;
        }
        self.queue.clear();
        if let Some(e) = &self.engine {
            e.cancel();
        }
        true
    }

    /// Ctrl+C: the second press within [`QUIT_ARM_MS`] quits. Returns true to quit.
    pub fn ctrl_c(&mut self, now: i64) -> bool {
        if !self.dialogs.is_empty() {
            self.interrupt();
            return false;
        }
        if self.quit_armed_until.is_some_and(|t| now < t) {
            return true;
        }
        self.quit_armed_until = Some(now + QUIT_ARM_MS);
        false
    }

    /// Classifies what the person typed (same rules as the TUI's prompt) and adds it to the
    /// history; the host sends `Submitted::Send`.
    pub fn submit(&mut self, text: &str) -> Submitted {
        let text = text.trim_end();
        if text.trim().is_empty() {
            return Submitted::Nothing;
        }
        if self.history.last().map(String::as_str) != Some(text) {
            self.history.push(text.to_string());
        }
        if let Some(rest) = text.strip_prefix('/') {
            let name = rest.split_whitespace().next().unwrap_or("");
            match self.commands.iter().find(|c| c.name == name) {
                Some(c) if c.local && name == "exit" => return Submitted::Quit,
                Some(c) if c.local && name == "help" => {
                    self.help();
                    return Submitted::Nothing;
                }
                // skills and MCP prompts are for the engine
                Some(c) if !c.local => {}
                Some(_) => {
                    self.note(format!("/{name}: {NOT_YET}"), false);
                    return Submitted::Nothing;
                }
                None => {
                    self.note(format!("unknown command /{name}"), true);
                    return Submitted::Nothing;
                }
            }
        } else if text.starts_with('!') || text.starts_with('#') {
            let what = if text.starts_with('!') {
                "!command"
            } else {
                "#note"
            };
            self.note(format!("{what}: {NOT_YET}"), false);
            return Submitted::Nothing;
        }
        if self.busy || self.engine.is_none() || !self.dialogs.is_empty() {
            // the TUI queues a line typed while a task runs; it starts when that one ends
            self.queue.push_back(text.to_string());
            return Submitted::Nothing;
        }
        self.busy = true;
        Submitted::Send(text.to_string())
    }

    /// The next queued line once the task ended and nothing waits for an answer.
    pub fn next_queued(&mut self) -> Option<String> {
        if self.busy || self.engine.is_none() || !self.dialogs.is_empty() {
            return None;
        }
        let t = self.queue.pop_front()?;
        self.busy = true;
        Some(t)
    }

    fn help(&mut self) {
        let mut lines = vec![
            "Enter sends; Shift+Enter adds a line. Esc interrupts a running task (partial output is kept).".to_string(),
            "Up/Down walk the prompt history, Ctrl+R searches it; Tab walks chat, Workspace, Running; Shift+Tab cycles the permission mode.".into(),
            "Ctrl+O shows every tool output whole; Ctrl+C twice quits.".into(),
            "@ mentions a file, / completes a command. Commands:".into(),
        ];
        for c in &self.commands {
            let tag = if c.local && !matches!(c.name.as_str(), "help" | "exit") {
                format!("  ({NOT_YET})")
            } else {
                String::new()
            };
            lines.push(format!(
                "  /{} {}: {}{tag}",
                c.name, c.argument_hint, c.description
            ));
        }
        self.note(lines.join("\n"), false);
    }

    /// The prompt history entry `back` steps from the newest (`None` past either end).
    pub fn history_at(&self, back: usize) -> Option<&str> {
        self.history
            .len()
            .checked_sub(back + 1)
            .map(|i| self.history[i].as_str())
    }

    /// Ctrl+R: the newest entry containing `needle` older than `before` steps back.
    pub fn history_search(&self, needle: &str, before: usize) -> Option<usize> {
        let n = needle.to_lowercase();
        (before..self.history.len()).find(|b| {
            self.history_at(*b)
                .is_some_and(|h| h.to_lowercase().contains(&n))
        })
    }

    /// Shift+Tab: default -> acceptEdits -> plan -> default. Bypass and auto take their own
    /// confirmation and are not on this cycle.
    pub fn cycle_mode(&mut self) {
        let Some(e) = &self.engine else { return };
        let next = match e.mode() {
            Mode::Default => Mode::AcceptEdits,
            Mode::AcceptEdits => Mode::Plan,
            _ => Mode::Default,
        };
        e.set_mode(next);
    }
}

impl Default for Live {
    fn default() -> Self {
        Self::new()
    }
}

fn question_options(d: &Dialog) -> Vec<(String, String)> {
    question_options_of(&d.kind)
}

/// The options of the question shown: its choices (with a check mark when picked), then
/// next/done and "decline".
fn question_options_of(k: &DialogKind) -> Vec<(String, String)> {
    let DialogKind::Question {
        questions,
        cur,
        picked,
        ..
    } = k
    else {
        return vec![];
    };
    let q = &questions[*cur];
    let mut v: Vec<(String, String)> = q
        .options
        .iter()
        .enumerate()
        .map(|(i, o)| {
            let on = picked[*cur].contains(&i);
            let mark = match (q.multi_select, on) {
                (true, true) => "[x] ",
                (true, false) => "[ ] ",
                (false, true) => "(o) ",
                (false, false) => "( ) ",
            };
            let desc = o
                .description
                .as_deref()
                .filter(|d| !d.is_empty())
                .map_or(String::new(), |d| {
                    format!(" \u{2013} {}", sanitize_for_display(d))
                });
            (
                format!("opt{i}"),
                format!("{mark}{}{desc}", sanitize_for_display(&o.label)),
            )
        })
        .collect();
    let last = *cur + 1 >= questions.len();
    v.push((
        if last { "done" } else { "next" }.into(),
        if last {
            "Send answers"
        } else {
            "Next question"
        }
        .into(),
    ));
    v.push(("no".into(), "Decline to answer (esc)".into()));
    v
}

/// The input of a tool call as the dialog shows it: every line visible (nothing hidden behind an
/// escape or a blank run), long bodies cut with a count of what was left out.
pub fn input_lines(tool: &str, input: &Value, max_lines: usize) -> Vec<String> {
    let s = |k: &str| input.get(k).and_then(Value::as_str);
    let mut out: Vec<String> = vec![];
    // line by line: `sanitize_for_display` turns a newline itself into an escape
    let mut push = |label: &str, body: &str| {
        let mut lines = body
            .replace("\r\n", "\n")
            .split('\n')
            .map(sanitize_for_display)
            .collect::<Vec<_>>()
            .into_iter();
        if let Some(first) = lines.next() {
            out.push(if label.is_empty() {
                first
            } else {
                format!("{label}: {first}")
            });
        }
        out.extend(lines);
    };
    match tool {
        "Bash" => push("", s("command").unwrap_or("")),
        "Edit" | "MultiEdit" | "Write" => {
            push("file", s("file_path").unwrap_or(""));
            for k in ["old_string", "new_string", "content"] {
                if let Some(v) = s(k) {
                    push(k, v);
                }
            }
        }
        _ => push("", &serde_json::to_string_pretty(input).unwrap_or_default()),
    }
    if out.len() > max_lines {
        let more = out.len() - max_lines;
        out.truncate(max_lines);
        out.push(format!(
            "\u{2026} {more} more line{}",
            if more == 1 { "" } else { "s" }
        ));
    }
    out
}

pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as i64)
}

/// Minutes east of UTC for the usage days and the clock; 0 when the zone cannot be read. No std
/// or dependency gives the local zone safely, so unix asks `date`; Windows uses UTC.
pub fn local_offset_min() -> i32 {
    if !cfg!(unix) {
        return 0;
    }
    std::process::Command::new("/bin/date")
        .arg("+%z")
        .output()
        .map_or(0, |o| zone_minutes(&String::from_utf8_lossy(&o.stdout)))
}

/// `+0530` / `-0800` (what `date +%z` prints) as minutes east of UTC.
pub fn zone_minutes(t: &str) -> i32 {
    let t = t.trim();
    let (sign, d) = t.split_at(t.len().min(1));
    match (sign, d.len() == 4, d.parse::<i32>()) {
        ("+", true, Ok(n)) => n / 100 * 60 + n % 100,
        ("-", true, Ok(n)) => -(n / 100 * 60 + n % 100),
        _ => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn live_with_commands() -> Live {
        let mut l = Live::new();
        l.commands = synthesis_tui::local_commands();
        l.commands.push(Command {
            name: "tdd".into(),
            description: "skill: tdd".into(),
            ..Command::default()
        });
        l.starting = false;
        l
    }

    #[test]
    fn slash_commands_are_never_dropped_silently() {
        let mut l = live_with_commands();
        assert_eq!(l.submit("/exit"), Submitted::Quit);
        assert_eq!(l.submit("/resume"), Submitted::Nothing);
        assert!(
            matches!(l.view.convo.items.last(), Some(Item::Notice { text, .. }) if text == &format!("/resume: {NOT_YET}"))
        );
        assert_eq!(l.submit("/nope"), Submitted::Nothing);
        assert!(
            matches!(l.view.convo.items.last(), Some(Item::Notice { text, .. }) if text == "unknown command /nope")
        );
        assert_eq!(l.submit("!ls"), Submitted::Nothing);
        assert!(
            matches!(l.view.convo.items.last(), Some(Item::Notice { text, .. }) if text.starts_with("!command"))
        );
        assert_eq!(l.submit("/help"), Submitted::Nothing);
        assert!(
            matches!(l.view.convo.items.last(), Some(Item::Notice { text, .. }) if text.contains("/model"))
        );
        // a skill goes to the engine, once there is one
        assert_eq!(
            l.submit("/tdd now"),
            Submitted::Nothing,
            "no engine yet: queued"
        );
        assert_eq!(l.queue.len(), 1);
        assert!(l.history.contains(&"/tdd now".to_string()));
    }

    #[test]
    fn ctrl_c_needs_two_presses_in_time() {
        let mut l = Live::new();
        assert!(!l.ctrl_c(1000));
        assert!(l.ctrl_c(1000 + QUIT_ARM_MS - 1));
        let mut l = Live::new();
        assert!(!l.ctrl_c(1000));
        assert!(!l.ctrl_c(1000 + QUIT_ARM_MS + 1), "too late: arms again");
    }

    fn ask(id: &str, suggestions: Vec<Suggestion>) -> AskApproval {
        AskApproval {
            id: id.into(),
            agent_id: None,
            tool: "Bash".into(),
            input: json!({"command": "ls"}),
            dialog: "permission",
            prompt: String::new(),
            reason: "needs a look".into(),
            origin: None,
            suggestions,
        }
    }

    #[test]
    fn a_dialog_ignores_enter_until_armed_and_esc_rejects() {
        let mut l = Live::new();
        l.on_msg(UiMsg::Approval(Box::new(ask("a", vec![]))), 10_000);
        assert_eq!(
            l.dialogs[0].options.len(),
            2,
            "yes / no without suggestions"
        );
        assert_eq!(l.pick(0, 10_000 + ARM_MS - 1), None, "still arming");
        assert_eq!(l.pick(0, 10_000 + ARM_MS).as_deref(), Some("yes"));
        assert_eq!(l.reject().as_deref(), Some("no"));
        l.on_msg(UiMsg::Withdraw("a".into()), 10_500);
        assert!(l.dialogs.is_empty());
        assert!(
            matches!(l.view.convo.items.last(), Some(Item::Notice { text, .. }) if text.contains("withdrawn"))
        );
    }

    #[test]
    fn peer_work_says_where_it_came_from_and_offers_no_always() {
        let mut l = Live::new();
        let mut a = ask("a", vec![]);
        a.origin = Some("peer x".into());
        l.on_msg(UiMsg::Approval(Box::new(a)), 0);
        let DialogKind::Approval(a) = &l.dialogs[0].kind else {
            panic!()
        };
        assert_eq!(a.reason, "for peer x: needs a look");
    }

    #[test]
    fn questions_toggle_and_advance_before_answering() {
        let mut l = Live::new();
        let q = |text: &str, multi: bool| Question {
            question: text.into(),
            header: None,
            multi_select: multi,
            options: vec![
                synthesis_tools::QuestionOption {
                    label: "a".into(),
                    description: None,
                },
                synthesis_tools::QuestionOption {
                    label: "b".into(),
                    description: None,
                },
            ],
        };
        l.on_msg(
            UiMsg::Input(Box::new(crate::engine::AskInput {
                id: "ask-1".into(),
                req: NeedsUser::AskUserQuestion {
                    questions: vec![q("one", true), q("two", false)],
                },
            })),
            0,
        );
        assert_eq!(l.pick(0, 1000), None);
        assert_eq!(l.pick(1, 1000), None);
        assert!(
            l.dialogs[0].options[0].1.starts_with("[x]")
                && l.dialogs[0].options[1].1.starts_with("[x]")
        );
        assert_eq!(l.pick(2, 1000), None, "next question");
        let DialogKind::Question { cur, .. } = &l.dialogs[0].kind else {
            panic!()
        };
        assert_eq!(*cur, 1);
        assert_eq!(l.pick(0, 1000), None);
        assert_eq!(l.pick(1, 1000), None, "single choice replaces");
        assert!(
            l.dialogs[0].options[0].1.starts_with("( )")
                && l.dialogs[0].options[1].1.starts_with("(o)")
        );
        assert_eq!(l.pick(2, 1000).as_deref(), Some("done"));
    }

    #[test]
    fn tool_input_keeps_every_line_visible_and_counts_what_it_cuts() {
        let v = json!({"command": "echo a\necho b\u{1b}[2J\necho c"});
        let l = input_lines("Bash", &v, 10);
        assert_eq!(l.len(), 3);
        assert!(l[1].contains("\\u{1b}"), "{:?}", l[1]);
        let l = input_lines("Bash", &v, 2);
        assert_eq!(l.last().unwrap(), "\u{2026} 1 more line");
    }

    #[test]
    fn history_walks_back_and_searches() {
        let mut l = Live::new();
        l.history = vec!["one".into(), "two cargo".into(), "three".into()];
        assert_eq!(l.history_at(0), Some("three"));
        assert_eq!(l.history_at(2), Some("one"));
        assert_eq!(l.history_at(3), None);
        assert_eq!(l.history_search("CARGO", 0), Some(1));
        assert_eq!(l.history_search("cargo", 2), None);
    }

    #[test]
    fn zone_offsets_parse() {
        assert_eq!(zone_minutes("+0700\n"), 420);
        assert_eq!(zone_minutes("-0530"), -330);
        assert_eq!(zone_minutes("garbage"), 0);
    }
}
