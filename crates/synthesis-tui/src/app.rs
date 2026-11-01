//! The UI state machine. No I/O: keys, events and time go in, `Cmd`s and frames come out, so the
//! whole app is testable without a terminal. The app never owns a process or a model request;
//! it asks the runtime to start a task and cancels it through the task's token.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use serde_json::{Value, json};
use synthesis_events::{Budget, Envelope, Event};
use synthesis_harness::{Approval, DENY_MESSAGE, TaskAgent, TaskOutcome, UserReply};
use synthesis_permissions::{Mode, Suggestion};
use synthesis_tools::NeedsUser;
use synthesis_tui_base::complete::{self, Command as CmdDef, StdFs};
use synthesis_tui_base::editor::LineEditor;
use synthesis_tui_base::keys::{Key, MouseAction, Named, WheelDir, clean_paste};
use synthesis_tui_base::screen::adoptable_size;
use synthesis_tui_base::selection::{
    self as sel, Act, Event as SelEvent, Mode as SelMode, Point, Selection, selection_transition,
};
use synthesis_tui_base::text::strip_ansi;
use synthesis_tui_render::agent::{AgentOpts, EditorView};
use synthesis_tui_render::agent::{
    INPUT_VIEW_ROWS, Opt, Pending, QOption, QState, Question as RQuestion, Search, Sug,
    Suggestions as RSug, body_sel_rows,
};
use synthesis_tui_render::dash::{
    DashState, DashView, Focus, Frame, Live, MIN_COLS, MIN_ROWS, SessionRow, Usage, Workspace,
    render_frame, workspace_key,
};
use synthesis_tui_render::dialogs::HELP;
use synthesis_tui_render::model::{
    self, Session, apply_event, parse_ts, session_from_events, session_phase,
};
use synthesis_tui_render::picker::{Picker, PickerItem};
use synthesis_tui_render::theme::{self, T_ERROR, T_SUCCESS};
use synthesis_tui_render::util::basename;
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

use crate::chat::Chat;
use crate::link::{ApprovalAsk, InputAsk};
use crate::links::{
    Link, OpenTarget, existing_path, join_wrapped, link_at, mouse_debug, open_gesture,
};
use crate::route::{self, Known, Route};
use crate::sanitize::{clean_input, clean_input_lines, clean_line, clean_text};
use crate::status::{SandboxStatus, StatusExtra, status_line_with};
use crate::{ExitReason, SavedSession};

const QUIT_ARM_MS: i64 = 1500;
const SCROLL_STEP: usize = 10;
const WHEEL_LINES: usize = 3;
/// Enter and digits cannot answer a dialog for this long after it comes to the front.
const ARM_MS: i64 = 400;
/// Dialogs also stay unarmed this long after any paste activity: the tail of a paste that forged
/// its end marker (`\r`, digits) arrives later than the parser's grace window.
const PASTE_QUIET_MS: i64 = 300;
const TOAST_MS: i64 = 1800;
/// A paste of more lines than this becomes a `[Pasted text #1 +300 lines]` chip in the prompt.
const PASTE_CHIP_LINES: usize = 10;
const PASTE_CHIP_MAX_CHARS: usize = 200_000;
/// Two Esc presses within this long edit the last prompt.
const ESC_ESC_MS: i64 = 1500;

/// Shift+Tab, the only way to switch modes. Reaching `bypassPermissions` opens its confirmation;
/// from it one press goes back to the first.
const CYCLE: [Mode; 5] = [
    Mode::Default,
    Mode::AcceptEdits,
    Mode::Plan,
    Mode::Auto,
    Mode::BypassPermissions,
];

pub(crate) struct AppConfig {
    pub cwd: String,
    pub session_id: String,
    pub model: String,
    pub version: String,
    pub budget: Budget,
    pub sandbox: SandboxStatus,
    pub mode: Mode,
    pub models: Vec<String>,
    /// `effort_tiers` the current model declares (empty: no effort control).
    pub effort_tiers: Vec<String>,
    /// Skills / custom commands offered next to the local ones (sent to the model as text).
    pub extra_commands: Vec<CmdDef>,
    pub saved: Vec<SavedSession>,
    pub home: Option<PathBuf>,
    /// `SYNTHESIS_MOUSE_DEBUG=1`: every mouse report is shown in the footer.
    pub mouse_debug: bool,
    pub tz_offset_min: i32,
    /// Agent definitions `@agent:<name>` can start.
    pub agent_defs: Vec<String>,
}

/// Effects for the runtime.
#[derive(Debug)]
pub(crate) enum Cmd {
    StartTask {
        text: String,
        /// What the pane shows instead of `text` (`Some("")`: nothing, a note to the model).
        display_text: Option<String>,
        cancel: CancellationToken,
        /// A sub-agent answers instead of the main agent.
        agent: Option<TaskAgent>,
    },
    /// A line for a running agent (`main` or an instance): the harness delivers it at the
    /// agent's next safe point, and wakes an idle sub-agent.
    SendTo {
        to: String,
        text: String,
    },
    /// Esc on a background sub-agent: cancel that agent only.
    CancelAgent(String),
    /// `!cmd` / `!!cmd`: run a shell command the user typed (explicit intent) and add it with
    /// its output to the conversation; `record_only` (`!!`) gives the model no turn for it.
    UserCommand {
        command: String,
        record_only: bool,
        cancel: CancellationToken,
    },
    ClearConversation,
    Copy(String),
    Open(OpenTarget),
    SwitchModel(String),
    /// `/effort` / alt+t: the host sets the tier for the following requests (`None`: auto).
    SetEffort(Option<String>),
    /// The person switched the permission mode: the host calls `Harness::set_mode` (which
    /// refuses `bypassPermissions` without `confirmed_bypass`).
    SetMode {
        mode: Mode,
        confirmed_bypass: bool,
    },
    /// Leave and re-enter the alternate screen (ctrl+l).
    ResetScreen,
    /// Right click in the prompt: the host reads the system clipboard and hands the text back
    /// through [`App::paste_clipboard`] (or reports the failure with [`App::toast`]).
    PasteClipboard,
    /// `/mcp`, `/agents`, `/permissions`: the host answers with text for the pane.
    Info(Info),
    /// The person confirmed saving a `#` note to the user (`true`) or project instruction file.
    Remember {
        user: bool,
        text: String,
    },
    /// `/compact [instructions]`: the host asks the harness to compact the conversation.
    Compact {
        instructions: Option<String>,
    },
}

#[derive(Debug)]
pub(crate) enum Info {
    Mcp,
    Memory,
    Agents,
    Permissions(PermAction),
}

#[derive(Debug)]
pub(crate) enum PermAction {
    List,
    Allow(String),
    Remove(String),
}

enum DialogKind {
    Approval {
        id: String,
        suggestions: Vec<Suggestion>,
        reply: Option<oneshot::Sender<Approval>>,
    },
    Question {
        id: String,
        questions: Vec<synthesis_tools::Question>,
        reply: Option<oneshot::Sender<Option<UserReply>>>,
    },
    Plan {
        id: String,
        reply: Option<oneshot::Sender<Option<UserReply>>>,
    },
}

struct Dialog {
    kind: DialogKind,
    pending: Pending,
    deadline_ms: Option<i64>,
}

impl Dialog {
    fn id(&self) -> &str {
        match &self.kind {
            DialogKind::Approval { id, .. }
            | DialogKind::Question { id, .. }
            | DialogKind::Plan { id, .. } => id,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum PickerKind {
    Model,
    Command,
    Folder,
    Resume,
    /// "Bypass permissions?" yes/no.
    BypassConfirm,
    /// Where a `#` note goes: project file, user file, or nowhere.
    Remember,
}

struct PickerState {
    kind: PickerKind,
    picker: Picker,
    /// One value per item of `picker.items`, same order.
    values: Vec<String>,
    dir: PathBuf,
}

struct HistSearch {
    query: String,
    matched: String,
    at: i64,
    from: i64,
}

#[derive(Default)]
struct SelState {
    mode: Option<SelMode>,
    selection: Option<Selection>,
    press: Option<Point>,
    extending: bool,
    pointer: Option<Point>,
}

/// Two presses on the same cell within this long are a double click, three a triple click.
const MULTI_CLICK_MS: i64 = 500;

impl SelState {
    fn mode(&self) -> SelMode {
        self.mode.unwrap_or(SelMode::Idle)
    }
}

struct Toast {
    text: String,
    until: i64,
    bad: bool,
}

struct TaskState {
    cancel: CancellationToken,
    cancelling: bool,
    /// The sub-agent this task runs as (`None`: the main agent).
    agent: Option<String>,
}

/// Host rendering of a `#` note as it will be saved.
pub(crate) type NotePreview = std::rc::Rc<dyn Fn(&str) -> String>;

pub(crate) struct App {
    pub cfg: AppConfig,
    pub quit: Option<ExitReason>,
    pub mode: Mode,
    pub model: String,
    chat: Chat,
    activity: Session,
    editor: LineEditor,
    scroll: usize,
    expanded: bool,
    dialogs: Vec<Dialog>,
    /// Front dialog id and the time from which Enter / digits may answer it.
    armed: Option<(String, i64)>,
    paste_quiet_until: i64,
    picker: Option<PickerState>,
    /// Set only by the bypass confirmation; cleared when the mode leaves bypass.
    bypass_confirmed: bool,
    /// Display paths of the (user, project) instruction files `#` notes can go to; `None` = the
    /// host cannot save notes.
    memory_files: Option<(String, String)>,
    /// The `#` note waiting for the person's choice of file.
    pending_note: Option<String>,
    /// The host's rendering of a note as it will be saved (secrets masked, `@` defanged).
    note_preview: Option<NotePreview>,
    /// Mode changes made while handling one key, sent to the host as `Cmd::SetMode`.
    mode_changes: Vec<(Mode, bool)>,
    search: Option<HistSearch>,
    sug: Option<complete::Suggestions>,
    sug_view: Vec<Sug>,
    sug_selected: usize,
    sug_dismissed: Option<String>,
    task: Option<TaskState>,
    /// Where lines go when they carry no address: `None` = main.
    target: Option<String>,
    /// Messages entered while a task runs, sent one per finished task (oldest first).
    queue: Vec<String>,
    /// Big pastes live in the prompt as a chip and are expanded when the prompt is sent.
    pastes: Vec<(String, String)>,
    /// Time of an Esc on an idle empty prompt: a second one soon after edits the last prompt.
    esc_at: Option<i64>,
    /// A `/compact` between tasks is running: a new task would race with its history swap.
    compacting: bool,
    ran_task: bool,
    quit_armed_at: Option<i64>,
    toast: Option<Toast>,
    sel: SelState,
    /// Last plain press (time, screen cell, run length) for double/triple click.
    last_click: Option<(i64, Point, u8)>,
    /// Last drawn frame: hit testing and copy read it.
    frame: Option<Frame>,
    caret: Option<(u32, u32)>,
    effort: Option<String>,
    lines: Vec<String>,
    size: (u32, u32),
    commands: Vec<CmdDef>,
}

fn local_commands() -> Vec<CmdDef> {
    let c = |name: &str, d: &str, hint: &str| CmdDef {
        name: name.into(),
        aliases: vec![],
        description: d.into(),
        argument_hint: hint.into(),
        local: true,
    };
    vec![
        c("help", "commands and keys", ""),
        c("clear", "start a new conversation", ""),
        c(
            "compact",
            "summarise the conversation to free context",
            "[instructions]",
        ),
        c("new", "open a session in another folder", ""),
        c("resume", "continue a saved session", "[session id]"),
        c("model", "pick a model, or change it directly", "[model]"),
        c(
            "effort",
            "reasoning effort (alt+t cycles)",
            "[auto|low|medium|high]",
        ),
        c("mcp", "MCP servers: state, tools, errors", ""),
        c(
            "memory",
            "loaded instruction files, and where # notes are saved",
            "",
        ),
        c(
            "agents",
            "agents of this session and the types you can start",
            "",
        ),
        c(
            "permissions",
            "mode and rules; allow / remove a rule",
            "[allow|remove RULE]",
        ),
        c(
            "status",
            "session, model, sandbox, budgets, verification",
            "",
        ),
        c(
            "focus",
            "toggle focus view (prompt, summary and response only)",
            "[on|off]",
        ),
        c("exit", "quit", ""),
    ]
}

/// Canonical, case-folded where the file system folds case (macOS, Windows): the key of a folder.
fn fold_dir(p: &str) -> String {
    let c = std::fs::canonicalize(p)
        .map_or_else(|_| p.to_string(), |p| p.to_string_lossy().into_owned());
    if cfg!(any(windows, target_os = "macos")) {
        c.to_lowercase()
    } else {
        c
    }
}

/// Same folder, through symlinks (macOS /var -> /private/var) and, where the file system folds
/// case (macOS, Windows), ignoring case.
fn same_dir(a: &str, b: &str) -> bool {
    fold_dir(a) == fold_dir(b)
}

/// `5m ago`, `3h ago`, `2d ago`.
fn relative_age(ms: i64) -> String {
    let m = ms.max(0) / 60_000;
    match m {
        0 => "just now".into(),
        1..=59 => format!("{m}m ago"),
        60..=2879 => format!("{}h ago", m / 60),
        _ => format!("{}d ago", m / 1440),
    }
}

/// `!cmd` / `!!cmd` -> (record only, trimmed command).
fn bang_command(text: &str) -> (bool, String) {
    let rest = text.strip_prefix('!').unwrap_or(text);
    match rest.strip_prefix('!') {
        Some(c) => (true, c.trim().to_string()),
        None => (false, rest.trim().to_string()),
    }
}

fn activity_for(cfg: &AppConfig, replay: &[Envelope]) -> Session {
    let mut activity = session_from_events(&cfg.session_id, replay.iter());
    activity.cwd = clean_line(&cfg.cwd);
    activity.project = clean_line(&basename(&cfg.cwd));
    if activity.model.is_empty() {
        activity.model = clean_line(&cfg.model);
    }
    activity
}

fn mode_str(m: Mode) -> &'static str {
    m.as_str()
}

fn is_key(k: &Key, n: Named) -> bool {
    matches!(k, Key::Named(x) if *x == n)
}

#[cfg(test)]
mod tests;

impl App {
    pub fn new(cfg: AppConfig, replay: &[Envelope], now: i64) -> Self {
        let mut commands = local_commands();
        commands.extend(cfg.extra_commands.iter().cloned());
        let mut chat = Chat::default();
        chat.agent.id = cfg.session_id.clone();
        chat.agent.kind = "main".into();
        chat.agent.cwd = clean_line(&cfg.cwd);
        chat.agent.model = clean_line(&cfg.model);
        chat.agent.mode = mode_str(cfg.mode).into();
        chat.agent.version = cfg.version.clone();
        chat.agent.status = "idle".into();
        let activity = activity_for(&cfg, replay);
        for e in replay {
            chat.apply(e, parse_ts(&e.ts).unwrap_or(now));
        }
        chat.agent.status = "idle".into();
        // agents of an earlier run are gone with its process
        chat.agents.clear();
        chat.inflight.clear();
        let ran_task = replay
            .iter()
            .any(|e| matches!(e.event, Event::TaskEnded { .. }));
        Self {
            mode: cfg.mode,
            model: cfg.model.clone(),
            cfg,
            quit: None,
            chat,
            activity,
            editor: LineEditor::new(),
            scroll: 0,
            expanded: false,
            dialogs: vec![],
            bypass_confirmed: false,
            memory_files: None,
            pending_note: None,
            note_preview: None,
            mode_changes: vec![],
            armed: None,
            paste_quiet_until: 0,
            picker: None,
            search: None,
            sug: None,
            sug_view: vec![],
            sug_selected: 0,
            sug_dismissed: None,
            task: None,
            target: None,
            queue: vec![],
            pastes: vec![],
            esc_at: None,
            compacting: false,
            ran_task,
            quit_armed_at: None,
            toast: None,
            sel: SelState::default(),
            last_click: None,
            frame: None,
            caret: None,
            effort: None,
            lines: vec![],
            size: (80, 24),
            commands,
        }
    }

    // ------------------------------------------------------------------ inputs from the harness

    pub fn on_event(&mut self, env: &Envelope, now: i64) {
        apply_event(&mut self.activity, env);
        self.chat.apply(env, now);
        if self
            .target
            .as_ref()
            .is_some_and(|t| self.chat.agents.iter().all(|r| r.name != *t))
        {
            self.target = None;
        }
        if matches!(
            env.event,
            Event::UserMessage { .. } | Event::TaskEnded { .. }
        ) {
            self.scroll = 0;
        }
    }

    pub fn compact_started(&mut self, now: i64) {
        self.compacting = true;
        self.chat.agent.status = "running".into();
        self.chat.agent.activity = Some("Compacting".into());
        self.chat.agent.turn_start = now;
    }

    pub fn compact_done(&mut self) {
        self.compacting = false;
        self.chat.agent.activity = None;
        if self.task.is_none() {
            self.chat.agent.status = "idle".into();
        }
    }

    pub fn note(&mut self, text: impl Into<String>, tone: &str) {
        self.chat.note(text, tone);
    }

    pub fn startup_notes(&mut self, notes: &[String]) {
        for n in notes {
            self.chat.startup_note(n);
        }
    }

    pub fn toast(&mut self, text: impl Into<String>, now: i64, bad: bool) {
        self.toast = Some(Toast {
            text: clean_line(&text.into()),
            until: now + TOAST_MS,
            bad,
        });
    }

    pub fn on_approval(&mut self, ask: ApprovalAsk, now: i64) {
        let ApprovalAsk {
            req,
            reply,
            deadline_ms,
        } = ask;
        let suggestions: Vec<Value> = req.suggestions.iter().map(suggestion_view).collect();
        let mut pending = Pending {
            kind: "permission".into(),
            dialog: "permission".into(),
            tool: clean_line(&req.tool),
            // file bodies keep their lines (the dialog draws a diff); commands stay one visible line
            input: if matches!(req.tool.as_str(), "Edit" | "MultiEdit" | "Write") {
                clean_input_lines(&req.input)
            } else {
                clean_input(&req.input)
            },
            diff: crate::preview::change_diff(Path::new(&self.cfg.cwd), &req.tool, &req.input)
                .map_or(Value::Null, |d| crate::chat::clean_diff(&d)),
            suggestions,
            status: "pending".into(),
            prompt: clean_line(&req.reason),
            display_name: req.agent_id.clone().unwrap_or_default(),
            ..Pending::default()
        };
        pending.hydrate();
        self.dialogs.push(Dialog {
            kind: DialogKind::Approval {
                id: req.approval_id,
                suggestions: req.suggestions,
                reply: Some(reply),
            },
            pending,
            deadline_ms,
        });
        self.sync_arming(now);
    }

    /// Whenever a different dialog comes to the front (opened, promoted after an answer or a
    /// withdrawal) Enter and digits are ignored for [`ARM_MS`]: a keystroke typed before the
    /// dialog appeared must not answer it. Esc / ctrl-c still cancel.
    fn sync_arming(&mut self, now: i64) {
        let front = self.dialogs.first().map(Dialog::id);
        if front != self.armed.as_ref().map(|a| a.0.as_str()) {
            self.armed = front.map(|id| (id.to_string(), now + ARM_MS));
        }
    }

    fn armed_in(&self, now: i64) -> Option<i64> {
        let open = self.armed.as_ref().map_or(0, |a| a.1);
        Some(open.max(self.paste_quiet_until) - now).filter(|ms| *ms > 0)
    }

    /// Paste start / bytes / end / forged marker / held sequence seen by the key parser.
    pub fn paste_activity(&mut self, now: i64) {
        self.paste_quiet_until = now + PASTE_QUIET_MS;
    }

    pub fn on_input(&mut self, ask: InputAsk, now: i64) {
        let InputAsk { id, req, reply } = ask;
        match req {
            NeedsUser::AskUserQuestion { questions } => {
                let rq: Vec<RQuestion> = questions
                    .iter()
                    .map(|q| RQuestion {
                        question: clean_text(&q.question),
                        header: q.header.as_deref().map(clean_line).unwrap_or_default(),
                        multi_select: q.multi_select,
                        options: q
                            .options
                            .iter()
                            .map(|o| QOption {
                                label: clean_line(&o.label),
                                description: o
                                    .description
                                    .as_deref()
                                    .map(clean_line)
                                    .unwrap_or_default(),
                            })
                            .collect(),
                    })
                    .collect();
                let mut pending = Pending {
                    kind: "permission".into(),
                    dialog: "question".into(),
                    tool: "AskUserQuestion".into(),
                    status: "pending".into(),
                    questions: rq,
                    ..Pending::default()
                };
                pending.hydrate();
                self.dialogs.push(Dialog {
                    kind: DialogKind::Question {
                        id,
                        questions,
                        reply: Some(reply),
                    },
                    pending,
                    deadline_ms: None,
                });
            }
            NeedsUser::ExitPlanMode { plan } => {
                let opt = |key: &str, label: &str, mode: Option<&str>| Opt {
                    key: key.into(),
                    label: label.into(),
                    mode: mode.map(String::from),
                };
                let pending = Pending {
                    kind: "permission".into(),
                    dialog: "plan".into(),
                    tool: "ExitPlanMode".into(),
                    input: json!({ "plan": clean_text(&plan) }),
                    status: "pending".into(),
                    options: Some(vec![
                        opt("manual", "Yes, manually approve edits", Some("default")),
                        opt("accept", "Yes, and auto-accept edits", Some("acceptEdits")),
                        opt("keep", "No, keep planning (esc)", None),
                    ]),
                    ..Pending::default()
                };
                self.dialogs.push(Dialog {
                    kind: DialogKind::Plan {
                        id,
                        reply: Some(reply),
                    },
                    pending,
                    deadline_ms: None,
                });
            }
        }
        self.sync_arming(now);
    }

    /// The harness side gave up on a dialog (answered elsewhere, cancelled, expired).
    pub fn on_withdraw(&mut self, id: &str, now: i64) {
        let n = self.dialogs.len();
        self.dialogs.retain(|d| d.id() != id);
        if self.dialogs.len() != n {
            self.chat
                .note("the request was withdrawn (cancelled or expired)", "warn");
        }
        self.sync_arming(now);
    }

    pub fn task_running(&self) -> bool {
        self.task.is_some()
    }

    pub fn cancel_task(&mut self) {
        self.interrupt();
    }

    /// A reply is merely streaming: nothing else needs fast frames.
    pub fn calm_streaming(&self) -> bool {
        self.chat.agent.status != "idle"
            && self.dialogs.is_empty()
            && self.toast.is_none()
            && self.sel.mode() != SelMode::Dragging
    }

    pub fn task_done(&mut self, result: &Result<TaskOutcome, String>, now: i64) {
        self.task = None;
        self.dialogs.clear();
        self.armed = None;
        self.ran_task = true;
        self.chat.agent.status = "idle".into();
        self.chat.agent.interrupting = false;
        if let Err(e) = result {
            self.chat.note(format!("task failed to run: {e}"), "error");
        }
        // lines the agent never read come back to the prompt
        let lines = std::mem::take(&mut self.chat.dropped_lines);
        self.chat.inflight.clear();
        if !lines.is_empty() {
            if self.editor.chars.is_empty() {
                self.editor.set(&lines.join("\n"));
            }
            self.chat.note(
                "a line was not delivered before the task ended · it is back in the prompt",
                "warn",
            );
        }
        let _ = now;
    }

    // ------------------------------------------------------------------ time

    pub fn animating(&self) -> bool {
        self.chat.agent.status != "idle"
            || !self.dialogs.is_empty()
            || self.toast.is_some()
            || self.sel.mode() == SelMode::Dragging
    }

    /// Returns true when something on screen changes without input.
    pub fn tick(&mut self, now: i64) -> bool {
        let mut dirty = self.animating();
        if self.toast.as_ref().is_some_and(|t| now >= t.until) {
            self.toast = None;
            dirty = true;
        }
        // approvals expire on the harness side; drop the dialog at the same time
        for d in &self.dialogs {
            if d.deadline_ms.is_some_and(|dl| now >= dl) {
                dirty = true;
            }
        }
        if self.sel.mode() == SelMode::Dragging
            && let (Some(g), Some(p)) = (self.geom(), self.sel.pointer)
        {
            let step = sel::edge_scroll_step(p.y, g.top, g.top + g.h - 1);
            let before = self.scroll;
            if step != 0 {
                let max = self.frame.as_ref().map_or(0, |f| f.rects.max_scroll);
                self.scroll = (before as i64 - step).clamp(0, max as i64) as usize;
                if self.scroll != before
                    && let Some(g) = self.geom()
                    && let Some(s) = self.sel.selection.as_mut()
                {
                    s.b = sel::point_to_content(&g, p);
                }
            }
        }
        dirty
    }

    pub fn set_size(&mut self, cols: u32, rows: u32) {
        if self.size != (cols, rows) {
            self.size = (cols, rows);
            self.clear_selection();
        }
    }

    // ------------------------------------------------------------------ keys

    pub fn on_key(&mut self, k: Key, now: i64) -> Vec<Cmd> {
        let mut out = vec![];
        if matches!(k, Key::Paste(_)) {
            self.paste_activity(now);
        }
        match &k {
            Key::CursorPosition { row, col } => {
                if let Some((c, r)) = adoptable_size(*col, *row, self.size) {
                    self.set_size(c, r);
                }
                return out;
            }
            Key::Mouse { .. } | Key::MouseDrag { .. } | Key::Wheel { .. } => {
                if self.cfg.mouse_debug
                    && let Some(d) = mouse_debug(&k)
                {
                    self.toast(d, now, false);
                }
                self.mouse(&k, now, &mut out);
                return out;
            }
            _ => {}
        }
        if is_key(&k, Named::CtrlL) {
            out.push(Cmd::ResetScreen);
            return out;
        }
        if is_key(&k, Named::AltT) && self.dialogs.is_empty() && self.picker.is_none() {
            self.set_effort("", &mut out);
            return out;
        }
        if is_key(&k, Named::CtrlO) {
            self.expanded = !self.expanded; // global: also over dialogs, pickers and a running task
            return out;
        }
        // ctrl+c with a highlighted selection copies it; with none it keeps its interrupt meaning
        if is_key(&k, Named::CtrlC)
            && matches!(self.sel.mode(), SelMode::Selected | SelMode::Pending)
            && self.dialogs.is_empty()
            && self.picker.is_none()
            && self.search.is_none()
        {
            self.copy_selection(now, &mut out);
            self.clear_selection();
            return out;
        }
        let paging = is_key(&k, Named::PageUp) || is_key(&k, Named::PageDown);
        if selection_transition(self.sel.mode(), SelEvent::Key { page: paging }).1 == Act::Clear {
            self.clear_selection();
        }
        if !self.dialogs.is_empty() {
            self.dialog_key(&k, now);
            self.sync_arming(now);
        } else if self.picker.is_some() {
            self.picker_key(&k, now, &mut out);
        } else if self.search.is_some() {
            self.search_key(&k);
        } else if !self.suggestion_key(&k, now, &mut out) {
            self.agent_key(&k, now, &mut out);
        }
        self.refresh_suggestions(now);
        out.extend(
            self.mode_changes
                .drain(..)
                .map(|(mode, confirmed_bypass)| Cmd::SetMode {
                    mode,
                    confirmed_bypass,
                }),
        );
        out
    }

    /// The host refused a mode change (or could not apply it): show the mode it still has.
    pub fn mode_rejected(&mut self, now_mode: Mode, why: &str) {
        self.mode = now_mode;
        self.bypass_confirmed = false;
        self.mode_changed_ui();
        self.out_item(format!("mode not changed: {why}"), true);
    }

    fn running(&self) -> bool {
        self.task.is_some()
    }

    /// Shell prompt: the text starts with a typed `!` (so history recall restores it). The
    /// leading `!` is not drawn; the prompt shows `! ` instead.
    fn bash(&self) -> bool {
        self.editor.chars.first() == Some(&'!')
    }

    /// Memory prompt: the text starts with a typed `#`; the note is saved only after a confirmation.
    fn memory(&self) -> bool {
        self.editor.chars.first() == Some(&'#')
    }

    pub fn set_note_preview(&mut self, f: NotePreview) {
        self.note_preview = Some(f);
    }

    pub fn set_memory_files(&mut self, user: String, project: String) {
        self.memory_files = Some((user, project));
    }

    /// A user command finished. `err` = the harness could not run it at all.
    pub fn command_done(&mut self, err: Option<&str>) {
        self.task = None;
        self.chat.agent.status = "idle".into();
        self.chat.agent.interrupting = false;
        if let Some(e) = err {
            self.chat
                .note(format!("command failed to run: {e}"), "error");
        }
    }

    /// The model turn after a user command: same task plumbing as a typed prompt, but the
    /// request (a note from the harness) is not shown.
    pub fn begin_followup(&mut self, now: i64) -> CancellationToken {
        let cancel = CancellationToken::new();
        self.task = Some(TaskState {
            cancel: cancel.clone(),
            cancelling: false,
            agent: None,
        });
        self.chat.agent.status = "running".into();
        self.chat.agent.turn_start = now;
        self.chat.agent.turn_mode = Some(mode_str(self.mode).into());
        self.scroll = 0;
        cancel
    }

    /// Esc: cancel the running task. Returns whether there was one.
    fn interrupt(&mut self) -> bool {
        match &mut self.task {
            Some(t) => {
                if !t.cancelling {
                    t.cancelling = true;
                    t.cancel.cancel();
                    self.chat.agent.interrupting = true;
                }
                true
            }
            None => false,
        }
    }

    fn agent_key(&mut self, k: &Key, now: i64, out: &mut Vec<Cmd>) {
        match k {
            Key::Text(t) => {
                self.scroll = 0; // typing brings the view back to the prompt
                if t == "?" && self.editor.chars.is_empty() {
                    self.run_slash("help", "", now, out);
                } else if t.starts_with(['!', '#', '@'])
                    && !matches!(t.as_str(), "!" | "#" | "@")
                    && self.editor.cur == 0
                {
                    // only a typed `!` / `#` / `@` starts a shell command / note / agent message,
                    // never a burst of characters
                    self.editor.insert(&format!(" {t}"));
                } else {
                    self.editor.insert(t);
                }
            }
            Key::Paste(t) => {
                // pasted text never becomes a shell command (or an agent message) by itself: type
                // the `!` / `@` yourself
                let t = if self.editor.cur == 0 && t.starts_with(['!', '#', '@']) {
                    format!(" {t}")
                } else {
                    t.clone()
                };
                self.scroll = 0;
                let lines = t.matches('\n').count() + 1;
                if lines > PASTE_CHIP_LINES && t.chars().count() <= PASTE_CHIP_MAX_CHARS {
                    let chip = format!("[Pasted text #{} +{lines} lines]", self.pastes.len() + 1);
                    if self.editor.insert(&chip) {
                        self.pastes.push((chip, t));
                    }
                } else if !self.editor.insert(&t) {
                    self.chat
                        .note("paste is too large · use @file to attach it", "warn");
                }
            }
            Key::Named(n) => self.named(*n, now, out),
            _ => {}
        }
    }

    fn named(&mut self, n: Named, now: i64, out: &mut Vec<Cmd>) {
        use Named::*;
        let recall_queue = self.editor.chars.is_empty() && !self.queue.is_empty();
        let tab_cycles = self.editor.chars.is_empty() && self.dialogs.is_empty();
        let e = &mut self.editor;
        match n {
            Enter => {
                if e.chars.last() == Some(&'\\') {
                    e.backspace();
                    e.insert("\n");
                } else {
                    self.submit(now, out);
                }
            }
            AltEnter | ShiftEnter | CtrlJ => {
                e.insert("\n");
            }
            Backspace => e.backspace(),
            Delete => e.del(),
            Left => e.left(),
            Right => e.right(),
            CtrlLeft => e.word_left(),
            CtrlRight => e.word_right(),
            Home | CtrlA => e.home(),
            End | CtrlE => e.end(),
            CtrlU => e.kill_start(),
            CtrlK => e.kill_end(),
            CtrlW => e.kill_word(),
            AltBackspace => e.kill_word_left(),
            AltD | AltDelete => e.kill_word_right(),
            Up if recall_queue => {
                // recall the newest queued line for editing (it leaves the queue)
                let t = self.queue.pop().unwrap_or_default();
                self.editor.set(&t);
            }
            Up => self.vertical(true),
            Down => self.vertical(false),
            PageUp => {
                let max = self.frame.as_ref().map_or(0, |f| f.rects.max_scroll);
                self.scroll = (self.scroll + SCROLL_STEP).min(max);
            }
            PageDown => self.scroll = self.scroll.saturating_sub(SCROLL_STEP),
            ShiftTab => self.cycle_mode(),
            Escape => {
                // queued lines first, then a running task / command; else leave the shell prompt
                // or (twice in a row on an empty prompt) edit the last prompt
                if !self.queue.is_empty() {
                    self.cancel_queue(now);
                } else if let Some(t) = self.esc_agent() {
                    // the current target only; ctrl+c cancels the whole task
                    out.push(Cmd::CancelAgent(t));
                } else if self.interrupt() {
                    self.esc_at = None;
                } else if self.bash() || self.memory() {
                    self.editor.clear();
                } else if !self.editor.chars.is_empty() {
                    self.esc_at = None;
                } else if self.esc_at.is_some_and(|t| now - t < ESC_ESC_MS) {
                    self.esc_at = None;
                    if let Some(last) = self.editor.history.last().cloned() {
                        self.editor.set(&last);
                    }
                } else {
                    self.esc_at = Some(now);
                }
            }
            Tab if tab_cycles => self.cycle_target(),
            CtrlR => {
                let from = e.history.len() as i64 - 1;
                let (matched, at) = self.search_history("", from);
                self.search = Some(HistSearch {
                    query: String::new(),
                    matched,
                    at,
                    from,
                });
            }
            CtrlP => self.open_command_picker(),
            CtrlN => self.open_folder_picker(PathBuf::from(&self.cfg.cwd)),
            CtrlC => {
                if !self.editor.chars.is_empty() {
                    self.editor.clear();
                } else if let Some(t) = &self.task {
                    // first press cancels, a second one while it winds down quits
                    self.queue.clear();
                    if t.cancelling {
                        self.quit = Some(ExitReason::Quit);
                    } else {
                        self.interrupt();
                    }
                    self.quit_armed_at = None;
                } else if self.quit_armed_at.is_some_and(|t| now - t < QUIT_ARM_MS) {
                    self.quit = Some(ExitReason::Quit);
                } else {
                    self.quit_armed_at = Some(now);
                    self.chat.note("press ctrl+c again to quit", "");
                }
            }
            CtrlD => {
                if self.editor.chars.is_empty() {
                    self.quit = Some(ExitReason::Quit);
                } else {
                    self.editor.del();
                }
            }
            _ => {}
        }
    }

    /// Up/Down: inside a multi-line prompt move by line, else walk the history.
    fn vertical(&mut self, up: bool) {
        let e = &mut self.editor;
        let start = |e: &LineEditor| {
            e.chars[..e.cur]
                .iter()
                .rposition(|c| *c == '\n')
                .map_or(0, |i| i + 1)
        };
        let end = |e: &LineEditor| {
            e.chars[e.cur..]
                .iter()
                .position(|c| *c == '\n')
                .map_or(e.chars.len(), |i| e.cur + i)
        };
        if up && e.multiline() && start(e) > 0 {
            let col = e.cur - start(e);
            e.cur = start(e) - 1;
            e.cur = (start(e) + col).min(end(e));
        } else if !up && e.multiline() && end(e) < e.chars.len() {
            let col = e.cur - start(e);
            e.cur = end(e) + 1;
            e.cur = (start(e) + col).min(end(e));
        } else if up {
            e.up();
        } else {
            e.down();
        }
    }

    fn cycle_mode(&mut self) {
        let i = CYCLE
            .iter()
            .position(|m| *m == self.mode)
            .map_or(0, |i| i + 1);
        match CYCLE[i % CYCLE.len()] {
            Mode::BypassPermissions => self.open_bypass_confirm(),
            m => {
                self.mode = m;
                self.mode_changed();
            }
        }
    }

    fn mode_changed(&mut self) {
        if self.mode != Mode::BypassPermissions {
            self.bypass_confirmed = false;
        }
        self.mode_changes.push((self.mode, self.bypass_confirmed));
        self.mode_changed_ui();
    }

    fn mode_changed_ui(&mut self) {
        self.chat.agent.mode = mode_str(self.mode).into();
        self.chat.agent.effort = self.effort.clone().unwrap_or_default();
    }

    fn submit(&mut self, now: i64, out: &mut Vec<Cmd>) {
        let text = self.editor.text();
        if text.trim().is_empty() {
            return;
        }
        if self.memory() {
            let note = text.trim_start_matches('#').trim().to_string();
            if !note.is_empty() {
                self.editor.submit();
                self.ask_where_to_remember(note);
            }
            return;
        }
        if self.bash() {
            if bang_command(&text).1.is_empty() {
                return;
            }
        } else if let Some(rest) = text.trim_start().strip_prefix('/') {
            let (name, arg) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
            if self.cfg.extra_commands.iter().all(|c| c.name != name) {
                self.editor.submit();
                self.run_slash(name, arg.trim(), now, out);
                return;
            }
        }
        if self.compacting {
            self.toast(
                "compacting the conversation · wait for it to finish",
                now,
                true,
            );
            return;
        }
        self.editor.submit();
        let mut text = text;
        for (chip, full) in self.pastes.drain(..) {
            text = text.replace(&chip, &full);
        }
        self.route_submit(text, now, out);
    }

    // ------------------------------------------------------------------ agents

    fn row(&self, name: &str) -> Option<&crate::chat::AgentRow> {
        self.chat.agents.iter().find(|r| r.name == name)
    }

    fn set_target(&mut self, name: &str) {
        self.target = (name != "main").then(|| name.to_string());
        self.chat.note(format!("now talking to @{name}"), "");
    }

    /// Tab on an empty prompt: main, then the agents of this session, then main again.
    fn cycle_target(&mut self) {
        let mut order = vec!["main".to_string()];
        order.extend(self.chat.agents.iter().map(|r| r.name.clone()));
        let cur = self.target.clone().unwrap_or_else(|| "main".into());
        let i = order.iter().position(|n| *n == cur).map_or(0, |i| i + 1);
        let next = order[i % order.len()].clone();
        self.set_target(&next);
    }

    /// Esc while the target is a background agent that is running next to the task.
    fn esc_agent(&self) -> Option<String> {
        let t = self.target.as_ref()?;
        let driving = self.task.as_ref()?.agent.as_deref() == Some(t.as_str());
        let live = self
            .row(t)
            .is_some_and(|r| r.status == "running" || r.status == "waiting_approval");
        (!driving && live).then(|| t.clone())
    }

    /// Reads the first word of a submitted line (messaging.md A5) and sends the rest where it says.
    fn route_submit(&mut self, text: String, now: i64, out: &mut Vec<Cmd>) {
        let names: Vec<String> = self.chat.agents.iter().map(|r| r.name.clone()).collect();
        let cwd = PathBuf::from(&self.cfg.cwd);
        let known = Known {
            names: &names,
            defs: &self.cfg.agent_defs,
        };
        match route::parse(&text, &known, |n| cwd.join(n).exists()) {
            Route::None => match self.target.clone() {
                Some(t) if !text.starts_with('!') => self.send_to(&t, text, now, out),
                _ => self.send_to("main", text, now, out),
            },
            Route::To { name, rest, hint } => {
                if let Some(h) = hint {
                    self.chat.note(h, "");
                }
                if rest.is_empty() {
                    self.set_target(&name);
                } else {
                    self.send_to(&name, rest, now, out);
                }
            }
            Route::Start { def, rest } if rest.is_empty() => self.chat.note(
                format!("write a message after @agent:{def} to start it"),
                "warn",
            ),
            // one task at a time: the start waits like any queued line and is read again then
            Route::Start { .. } if self.running() => self.queue.push(text),
            Route::Start { def, rest } => self.dispatch_agent(TaskAgent::New(def), rest, now, out),
            Route::Unknown(n) => self.chat.note(
                format!("@agent:{n}: no such agent or agent type · /agents lists them"),
                "error",
            ),
            Route::Later(w) => self.chat.note(
                format!("@{w}: addressing another session is not available yet"),
                "warn",
            ),
        }
    }

    fn send_to(&mut self, name: &str, text: String, now: i64, out: &mut Vec<Cmd>) {
        if self.running() {
            let own_task = self.task.as_ref().and_then(|t| t.agent.as_deref());
            let main_busy_elsewhere = name == "main" && own_task.is_some();
            // `!cmd` and a main line during a sub-agent's task wait for the task to end
            if text.starts_with('!') || main_busy_elsewhere {
                self.queue.push(text);
            } else {
                out.push(Cmd::SendTo {
                    to: name.to_string(),
                    text,
                });
            }
        } else if name == "main" {
            self.dispatch(text, now, out);
        } else {
            match self.row(name).map(|r| r.status.as_str()) {
                Some("idle" | "cancelled") => {
                    self.dispatch_agent(TaskAgent::Existing(name.to_string()), text, now, out);
                }
                Some(s) => self.chat.note(format!("@{name} is {s}"), "warn"),
                None => self.chat.note(format!("@{name}: no such agent"), "error"),
            }
        }
    }

    fn dispatch_agent(&mut self, agent: TaskAgent, text: String, now: i64, out: &mut Vec<Cmd>) {
        let cancel = self.begin_followup(now);
        self.clear_selection();
        if let TaskAgent::Existing(n) = &agent {
            if let Some(r) = self.chat.agents.iter_mut().find(|r| r.name == *n) {
                r.show = true;
            }
            if let Some(t) = self.task.as_mut() {
                t.agent = Some(n.clone());
            }
        }
        out.push(Cmd::StartTask {
            text,
            display_text: None,
            cancel,
            agent: Some(agent),
        });
    }

    fn agent_rows(&self) -> Vec<String> {
        let mut rows: Vec<&crate::chat::AgentRow> = self.chat.agents.iter().collect();
        rows.sort_by_key(|r| r.status == "idle");
        rows.into_iter()
            .map(|r| {
                format!(
                    "{} ({}) {} · {} tools{}",
                    r.name,
                    r.kind,
                    r.status,
                    r.tools,
                    if r.queued > 0 {
                        format!(" · {} queued", r.queued)
                    } else {
                        String::new()
                    }
                )
            })
            .collect()
    }

    /// The host could not hand a line to its agent: it goes back to the prompt.
    pub fn send_failed(&mut self, text: &str, why: &str) {
        self.chat.note(format!("not sent: {why}"), "error");
        if self.editor.chars.is_empty() {
            self.editor.set(text);
        }
    }

    /// Starts `text` (a prompt, or a `!cmd` / `!!cmd` shell line) now; the editor is not touched.
    fn dispatch(&mut self, text: String, now: i64, out: &mut Vec<Cmd>) {
        if text.starts_with('!') {
            let (record_only, command) = bang_command(&text);
            let cancel = self.begin_followup(now);
            self.clear_selection();
            out.push(Cmd::UserCommand {
                command,
                record_only,
                cancel,
            });
            return;
        }
        let cancel = self.begin_followup(now);
        self.clear_selection();
        out.push(Cmd::StartTask {
            text,
            display_text: None,
            cancel,
            agent: None,
        });
    }

    /// Called by the host after every message: the next queued line starts once nothing runs.
    pub fn next_queued(&mut self, now: i64) -> Vec<Cmd> {
        let mut out = vec![];
        if self.task.is_none() && !self.compacting && self.quit.is_none() && !self.queue.is_empty()
        {
            let t = self.queue.remove(0);
            self.route_submit(t, now, &mut out);
        }
        out
    }

    /// Esc: give the queued lines back to the prompt (or drop them when it has a draft).
    fn cancel_queue(&mut self, now: i64) {
        let all = std::mem::take(&mut self.queue).join("\n");
        if self.editor.chars.is_empty() {
            self.editor.set(&all);
        }
        self.toast("queued messages cancelled", now, false);
    }

    // ------------------------------------------------------------------ slash commands

    pub fn output(&mut self, text: String, error: bool) {
        self.out_item(text, error);
    }

    fn out_item(&mut self, text: impl Into<String>, error: bool) {
        self.chat.push(item! {
            kind: "command-output".into(),
            text: clean_text(&text.into()),
            error,
        });
    }

    fn run_slash(&mut self, name: &str, arg: &str, now: i64, out: &mut Vec<Cmd>) {
        match name {
            "help" => {
                self.chat.push(item! {
                    kind: "help".into(),
                    lines: HELP.iter().map(|s| (*s).to_string()).collect(),
                });
            }
            "compact" if self.compacting => self.out_item("/compact: already compacting", true),
            "compact" => out.push(Cmd::Compact {
                instructions: (!arg.is_empty()).then(|| arg.to_string()),
            }),
            "exit" | "quit" => self.quit = Some(ExitReason::Quit),
            "clear" => {
                if self.running() {
                    self.out_item("/clear: a task is running · esc cancels it first", true);
                } else {
                    self.chat.agent.items.clear();
                    // the meters belong to the conversation that is gone
                    self.chat.ctx = Default::default();
                    self.chat.verification = None;
                    self.chat.totals = Default::default();
                    self.ran_task = false;
                    self.activity = activity_for(&self.cfg, &[]);
                    self.activity.model = clean_line(&self.model);
                    self.chat.note("new conversation", "");
                    out.push(Cmd::ClearConversation);
                }
            }
            "new" => self.open_folder_picker(PathBuf::from(&self.cfg.cwd)),
            "resume" => {
                if arg.is_empty() {
                    self.open_resume_picker(false, now);
                } else {
                    self.quit = Some(ExitReason::Resume(arg.to_string()));
                }
            }
            "model" => {
                if arg.is_empty() {
                    self.open_model_picker();
                } else {
                    self.switch_model(arg, out);
                }
            }
            "effort" => self.set_effort(arg, out),
            "mcp" => out.push(Cmd::Info(Info::Mcp)),
            "memory" => out.push(Cmd::Info(Info::Memory)),
            "agents" => out.push(Cmd::Info(Info::Agents)),
            "permissions" | "perms" => {
                let (sub, rule) = arg.split_once(char::is_whitespace).unwrap_or((arg, ""));
                let rule = rule.trim();
                match (sub, rule.is_empty()) {
                    ("", _) | ("list", _) => out.push(Cmd::Info(Info::Permissions(PermAction::List))),
                    ("allow", false) => out.push(Cmd::Info(Info::Permissions(PermAction::Allow(rule.into())))),
                    ("remove" | "rm", false) => out.push(Cmd::Info(Info::Permissions(PermAction::Remove(rule.into())))),
                    _ => self.out_item("usage: /permissions [allow RULE | remove RULE]   e.g. /permissions allow Bash(git status:*)", true),
                }
            }
            "status" => self.status_item(),
            "focus" => {
                let on = match arg {
                    "on" => true,
                    "off" => false,
                    _ => !self.chat.agent.focus_view,
                };
                self.chat.agent.focus_view = on;
            }
            other => self.out_item(format!("unknown command: /{other} (try /help)"), true),
        }
        let _ = now;
    }

    /// `# note`: nothing is written until the person picks a file here (Esc saves nothing). The
    /// host redacts secrets from the note before it is written.
    fn ask_where_to_remember(&mut self, note: String) {
        let Some((user, project)) = self.memory_files.clone() else {
            self.out_item("# notes are not available in this session", true);
            return;
        };
        let item = |label: &str, detail: String, current| PickerItem {
            label: label.into(),
            detail,
            current,
        };
        let items = vec![
            (
                item("No, do not save", "esc also cancels".into(), true),
                "no".to_string(),
            ),
            (
                item("Save to the project file", project, false),
                "project".into(),
            ),
            (item("Save to the user file", user, false), "user".into()),
        ];
        let shown = self
            .note_preview
            .as_ref()
            .map_or_else(|| note.clone(), |f| f(&note));
        self.pending_note = Some(note);
        self.open_picker(
            PickerKind::Remember,
            "Remember this note?",
            &format!(
                "will be saved as: - {} (part of the instructions of later sessions)",
                crate::sanitize::clean_line(&shown)
            ),
            items,
            PathBuf::new(),
        );
    }

    /// Esc, "No" (the first item) or anything but "Yes" keeps the current mode.
    fn open_bypass_confirm(&mut self) {
        let items = vec![
            (
                PickerItem {
                    label: format!(
                        "No, keep {}",
                        synthesis_tui_render::dialogs::mode_name(Some(self.mode.as_str()))
                    ),
                    detail: "esc also cancels".into(),
                    current: true,
                },
                "no".to_string(),
            ),
            (
                PickerItem {
                    label: "Yes, bypass permissions".into(),
                    detail: "this session only; shift+tab leaves it".into(),
                    current: false,
                },
                "yes".to_string(),
            ),
        ];
        self.out_item(
            "bypassPermissions turns OFF approval prompts: every tool call runs, including edits and shell commands anywhere the sandbox allows. Still stops: deny rules, ask rules, critical removals (rm -rf ~ or /), destructive MCP calls (they still ask). It is never saved and never restored on resume.",
            false,
        );
        self.open_picker(
            PickerKind::BypassConfirm,
            "Bypass permissions?",
            "enter confirms the highlighted choice",
            items,
            PathBuf::new(),
        );
    }

    /// `/effort [tier|auto]`; no argument (and alt+t) steps through auto, then the model's tiers.
    fn set_effort(&mut self, arg: &str, out: &mut Vec<Cmd>) {
        let tiers = &self.cfg.effort_tiers;
        if tiers.is_empty() {
            self.out_item("/effort: this model declares no effort tiers", true);
            return;
        }
        let next = match arg {
            "" => match self
                .effort
                .as_ref()
                .and_then(|e| tiers.iter().position(|t| t == e))
            {
                None => tiers.first().cloned(),
                Some(i) => tiers.get(i + 1).cloned(),
            },
            "auto" | "default" => None,
            t if tiers.iter().any(|x| x == t) => Some(t.to_string()),
            _ => {
                let all = tiers.join("|");
                self.out_item(format!("usage: /effort [auto|{all}]"), true);
                return;
            }
        };
        self.chat
            .note(format!("effort: {}", next.as_deref().unwrap_or("auto")), "");
        self.effort.clone_from(&next);
        out.push(Cmd::SetEffort(next));
    }

    fn switch_model(&mut self, name: &str, out: &mut Vec<Cmd>) {
        if self.running() {
            self.out_item("/model: a task is running · esc cancels it first", true);
        } else {
            out.push(Cmd::SwitchModel(name.to_string()));
        }
    }

    pub fn model_switched(&mut self, label: &str) {
        self.model = label.to_string();
        self.chat.agent.model = clean_line(label);
        self.activity.model = clean_line(label);
        self.chat.note(format!("model: {label}"), "");
    }

    fn status_item(&mut self) {
        let t = &self.chat.totals;
        let v = self
            .chat
            .verification
            .as_ref()
            .map_or("none".to_string(), |v| {
                format!("{}: {}", v.status, v.summary)
            });
        let rows = [
            ("session", self.cfg.session_id.clone()),
            ("model", clean_line(&self.model)),
            ("folder", clean_line(&self.cfg.cwd)),
            (
                "mode",
                synthesis_tui_render::dialogs::mode_name(Some(mode_str(self.mode))).to_string(),
            ),
            (
                "context",
                self.chat
                    .ctx
                    .text(i64::MIN)
                    .map_or("no usage yet".to_string(), |(t, _)| t),
            ),
            (
                "sandbox",
                self.cfg.sandbox.text().0.replace("sandbox: ", ""),
            ),
            (
                "budget",
                format!(
                    "turns {}/{} · tool calls {}/{}",
                    t.turns,
                    self.cfg.budget.max_turns,
                    t.tool_calls,
                    self.cfg.budget.max_tool_calls
                ),
            ),
            ("verification", v),
        ];
        self.chat.push(item! {
            kind: "info".into(),
            title: "Status".into(),
            rows: rows
                .iter()
                .map(|(k, v)| vec![(*k).to_string(), clean_text(v)])
                .collect(),
        });
    }

    // ------------------------------------------------------------------ pickers

    fn open_picker(
        &mut self,
        kind: PickerKind,
        title: &str,
        sub: &str,
        items: Vec<(PickerItem, String)>,
        dir: PathBuf,
    ) {
        let (pi, values): (Vec<_>, Vec<_>) = items.into_iter().unzip();
        let mut picker = Picker::new(title, title, sub, pi);
        picker.no_filter = matches!(kind, PickerKind::BypassConfirm | PickerKind::Remember);
        self.picker = Some(PickerState {
            kind,
            picker,
            values,
            dir,
        });
    }

    fn open_model_picker(&mut self) {
        let items = self
            .cfg
            .models
            .iter()
            .map(|m| {
                (
                    PickerItem {
                        label: m.clone(),
                        detail: String::new(),
                        current: *m == self.model,
                    },
                    m.clone(),
                )
            })
            .collect::<Vec<_>>();
        if items.is_empty() {
            self.out_item("/model: no models were provided by the host", true);
        } else {
            self.open_picker(
                PickerKind::Model,
                "Model",
                "enter selects",
                items,
                PathBuf::new(),
            );
        }
    }

    fn open_command_picker(&mut self) {
        let items = self
            .commands
            .iter()
            .map(|c| {
                (
                    PickerItem {
                        label: format!("/{}", c.name),
                        detail: c.description.clone(),
                        current: false,
                    },
                    c.name.clone(),
                )
            })
            .collect();
        self.open_picker(
            PickerKind::Command,
            "Commands",
            "enter inserts",
            items,
            PathBuf::new(),
        );
    }

    /// Sessions of this folder first; the first row toggles "all folders".
    fn open_resume_picker(&mut self, all: bool, now: i64) {
        let here = |c: &str| same_dir(c, &self.cfg.cwd);
        let mut items = vec![];
        let mut n = 0;
        // the open session cannot be resumed, and one without a prompt has nothing to continue
        for s in self.cfg.saved.iter().filter(|s| {
            (all || here(&s.cwd)) && s.id != self.cfg.session_id && !s.title.trim().is_empty()
        }) {
            n += 1;
            let title = clean_line(&s.title);
            let title = if title.is_empty() {
                "(no prompt)".into()
            } else {
                title
            };
            let age = relative_age(now - s.mtime);
            let place = if all {
                format!(" · {}", clean_line(&s.cwd))
            } else {
                String::new()
            };
            items.push((
                PickerItem {
                    label: title.chars().take(70).collect(),
                    detail: format!(
                        "{age}{place} · {}",
                        s.id.chars().take(8).collect::<String>()
                    ),
                    current: s.id == self.cfg.session_id,
                },
                s.id.clone(),
            ));
        }
        items.push((
            PickerItem {
                label: if all {
                    "◂ only this folder".into()
                } else {
                    "▸ show sessions from all folders".into()
                },
                detail: String::new(),
                current: false,
            },
            "__toggle__".to_string(),
        ));

        if n == 0 {
            if all {
                self.out_item("/resume: no saved sessions", true);
            } else {
                // nothing here: the full list, if it has anything
                self.open_resume_picker(true, now);
            }
            return;
        }
        let title = if all {
            "Resume a session · all folders"
        } else {
            "Resume a session · this folder"
        };
        self.open_picker(
            PickerKind::Resume,
            title,
            "enter continues it",
            items,
            PathBuf::new(),
        );
    }

    fn open_folder_picker(&mut self, dir: PathBuf) {
        let mut items = vec![
            (
                PickerItem {
                    label: "✓ use this folder".into(),
                    detail: dir.display().to_string(),
                    current: false,
                },
                "use".to_string(),
            ),
            (
                PickerItem {
                    label: "..".into(),
                    detail: "parent folder".into(),
                    current: false,
                },
                "up".to_string(),
            ),
        ];
        for name in list_dirs(&dir) {
            items.push((
                PickerItem {
                    label: format!("{}/", clean_line(&name)),
                    detail: String::new(),
                    current: false,
                },
                format!("cd:{name}"),
            ));
        }
        let mut picker = Picker::new(
            "folder",
            "New session in folder",
            &dir.display().to_string(),
            vec![],
        );
        let (pi, values): (Vec<_>, Vec<_>) = items.into_iter().unzip();
        picker.items = pi;
        picker.choice = 0;
        self.picker = Some(PickerState {
            kind: PickerKind::Folder,
            picker,
            values,
            dir,
        });
    }

    fn picker_key(&mut self, k: &Key, now: i64, out: &mut Vec<Cmd>) {
        let Some(ps) = self.picker.as_mut() else {
            return;
        };
        let p = &mut ps.picker;
        match k {
            Key::Named(Named::Escape | Named::CtrlC) => {
                self.picker = None;
                self.pending_note = None;
            }
            // the ring goes on past bypass: one press = one step, never swallowed by the dialog
            Key::Named(Named::ShiftTab) if ps.kind == PickerKind::BypassConfirm => {
                self.picker = None;
                self.mode = CYCLE[0];
                self.mode_changed();
            }
            Key::Named(Named::Up) => p.move_by(-1),
            Key::Named(Named::Down) => p.move_by(1),
            Key::Named(Named::PageUp) => p.move_by(-(p.window_size.max(1) as i64)),
            Key::Named(Named::PageDown) => p.move_by(p.window_size.max(1) as i64),
            Key::Named(Named::Home) => p.home(),
            Key::Named(Named::End) => p.end(),
            Key::Named(Named::Backspace) => p.backspace(),
            Key::Text(t) | Key::Paste(t) if !p.no_filter => p.type_text(&t.replace('\n', " ")),
            Key::Named(Named::Enter) => {
                let Some(item) = p.filtered().get(p.choice).copied() else {
                    return;
                };
                let Some(idx) = p.items.iter().position(|i| std::ptr::eq(i, item)) else {
                    return;
                };
                let value = ps.values[idx].clone();
                let (kind, dir) = (ps.kind, ps.dir.clone());
                self.pick(kind, &value, dir, now, out);
            }
            _ => {}
        }
    }

    fn pick(&mut self, kind: PickerKind, value: &str, dir: PathBuf, now: i64, out: &mut Vec<Cmd>) {
        match kind {
            PickerKind::Folder => match value {
                "use" => {
                    self.picker = None;
                    self.quit = Some(ExitReason::OpenFolder(dir));
                }
                "up" => {
                    let parent = dir.parent().map_or(dir.clone(), Path::to_path_buf);
                    self.open_folder_picker(parent);
                }
                v => {
                    let name = v.strip_prefix("cd:").unwrap_or(v);
                    self.open_folder_picker(dir.join(name));
                }
            },
            PickerKind::Resume if value == "__toggle__" => {
                let all = self
                    .picker
                    .as_ref()
                    .is_some_and(|p| p.picker.title.contains("this folder"));
                self.open_resume_picker(all, now);
            }
            PickerKind::Resume => {
                self.picker = None;
                self.quit = Some(ExitReason::Resume(value.to_string()));
            }
            PickerKind::Model => {
                self.picker = None;
                self.switch_model(value, out);
            }
            PickerKind::BypassConfirm => {
                self.picker = None;
                if value == "yes" {
                    self.mode = Mode::BypassPermissions;
                    self.bypass_confirmed = true;
                    self.mode_changed();
                }
            }
            PickerKind::Remember => {
                self.picker = None;
                let note = self.pending_note.take();
                if let (Some(text), "user" | "project") = (note, value) {
                    out.push(Cmd::Remember {
                        user: value == "user",
                        text,
                    });
                }
            }
            PickerKind::Command => {
                self.picker = None;
                let c = self.commands.iter().find(|c| c.name == value).cloned();
                if let Some(c) = c {
                    if c.argument_hint.is_empty() && c.local {
                        self.run_slash(&c.name, "", now, out);
                    } else {
                        self.editor.set(&format!("/{} ", c.name));
                    }
                }
            }
        }
    }

    // ------------------------------------------------------------------ history search

    fn search_history(&self, query: &str, from: i64) -> (String, i64) {
        let h = &self.editor.history;
        let q = query.to_lowercase();
        let mut i = from.min(h.len() as i64 - 1);
        while i >= 0 {
            let entry = &h[i as usize];
            if q.is_empty() || entry.to_lowercase().contains(&q) {
                return (entry.clone(), i);
            }
            i -= 1;
        }
        (String::new(), -1)
    }

    fn search_key(&mut self, k: &Key) {
        let Some(s) = self.search.as_mut() else {
            return;
        };
        let mut again = true;
        match k {
            Key::Text(t) | Key::Paste(t) => s.query.push_str(t),
            Key::Named(Named::Backspace) => {
                s.query.pop();
            }
            Key::Named(Named::CtrlR) => {
                s.from = s.at - 1;
                again = false;
            }
            Key::Named(Named::Enter | Named::Right | Named::Tab) => {
                let m = s.matched.clone();
                if !m.is_empty() {
                    self.editor.set(&m);
                }
                self.search = None;
                return;
            }
            Key::Named(Named::Escape | Named::CtrlC) => {
                self.search = None;
                return;
            }
            _ => return,
        }
        let (query, mut from) = (s.query.clone(), s.from);
        if again {
            from = self.editor.history.len() as i64 - 1;
        }
        let (matched, at) = self.search_history(&query, from);
        if let Some(s) = self.search.as_mut() {
            s.from = from;
            s.matched = matched;
            s.at = at;
        }
    }

    // ------------------------------------------------------------------ suggestions

    fn refresh_suggestions(&mut self, now: i64) {
        if !self.dialogs.is_empty()
            || self.picker.is_some()
            || self.search.is_some()
            || self.bash()
            || self.memory()
        {
            self.sug = None;
            self.sug_view.clear();
            return;
        }
        let key = format!("{}|{}", self.editor.text(), self.editor.cur);
        if self.sug_dismissed.as_deref() == Some(&key) {
            self.sug = None;
            self.sug_view.clear();
            return;
        }
        let cwd = PathBuf::from(&self.cfg.cwd);
        let mut opts = complete::Options::new(&cwd);
        opts.commands = &self.commands;
        opts.limit = 50;
        // agents are offered at the start of the message only (the picker checks the position)
        let mut refs = vec![complete::AgentRef {
            id: "main".into(),
            kind: Some("main".into()),
            status: Some("this session".into()),
            ..complete::AgentRef::default()
        }];
        refs.extend(self.chat.agents.iter().map(|r| complete::AgentRef {
            id: r.name.clone(),
            kind: Some(r.kind.clone()),
            status: Some(r.status.clone()),
            queued: r.queued as usize,
            start_new: false,
        }));
        refs.extend(self.cfg.agent_defs.iter().map(|d| complete::AgentRef {
            id: d.clone(),
            start_new: true,
            ..complete::AgentRef::default()
        }));
        opts.agents = &refs;
        let next = complete::suggest(&self.editor, &opts, &StdFs);
        let same = match (&next, &self.sug) {
            (Some(a), Some(b)) => a.token == b.token,
            _ => false,
        };
        if !same {
            self.sug_selected = 0;
        }
        self.sug_view = next
            .as_ref()
            .map(|s| {
                s.items
                    .iter()
                    .map(|i| Sug {
                        label: i.label.clone(),
                        detail: i.detail(&StdFs, now),
                    })
                    .collect()
            })
            .unwrap_or_default();
        self.sug = next.filter(|s| !s.items.is_empty());
        if self.sug.is_none() {
            self.sug_view.clear();
        }
    }

    /// Returns true when the key was consumed.
    fn suggestion_key(&mut self, k: &Key, now: i64, out: &mut Vec<Cmd>) -> bool {
        let Some(s) = self.sug.as_ref() else {
            return false;
        };
        let n = s.items.len();
        match k {
            Key::Named(Named::Up) => self.sug_selected = (self.sug_selected + n - 1) % n,
            Key::Named(Named::Down) => self.sug_selected = (self.sug_selected + 1) % n,
            Key::Named(Named::PageUp) => self.sug_selected = self.sug_selected.saturating_sub(8),
            Key::Named(Named::PageDown) => self.sug_selected = (self.sug_selected + 8).min(n - 1),
            Key::Named(n @ (Named::Tab | Named::Enter)) => {
                let item = &s.items[self.sug_selected.min(n_idx(s))];
                let exact = self.editor.chars[s.token.start..s.token.end]
                    .iter()
                    .collect::<String>()
                    == item.insert.trim_end();
                let detail = item.detail(&StdFs, now);
                let is_cmd = s.token.kind == complete::TokenKind::Command;
                let (token, item) = (s.token.clone(), item.clone());
                complete::apply(&mut self.editor, &token, &item);
                // enter on a complete command runs it right away
                if *n == Named::Enter && is_cmd && (exact || !detail.contains('<')) {
                    self.sug = None;
                    self.sug_view.clear();
                    self.submit(now, out);
                }
            }
            Key::Named(Named::Escape) => {
                self.sug_dismissed = Some(format!("{}|{}", self.editor.text(), self.editor.cur));
            }
            _ => return false,
        }
        true
    }

    // ------------------------------------------------------------------ dialogs

    fn dialog_key(&mut self, k: &Key, now: i64) {
        let Some(d) = self.dialogs.first_mut() else {
            return;
        };
        // any key keeps an expiring approval waiting: only the clock or an answer ends it
        match &d.kind {
            DialogKind::Question { .. } => self.question_key(k),
            _ => self.choice_key(k, now),
        }
    }

    fn choice_key(&mut self, k: &Key, now: i64) {
        let wait = self.armed_in(now).is_some();
        let d = &mut self.dialogs[0];
        let n = d.pending.options.as_ref().map_or(0, Vec::len);
        if n == 0 {
            return;
        }
        let pick = match k {
            Key::Named(Named::Up) => {
                d.pending.choice = (d.pending.choice + n - 1) % n;
                None
            }
            Key::Named(Named::Down) => {
                d.pending.choice = (d.pending.choice + 1) % n;
                None
            }
            // `x`: the whole (sanitised) input, scrollable; harmless, so allowed while arming
            Key::Text(t) if t == "x" => {
                d.pending.view_all = !d.pending.view_all;
                d.pending.input_scroll = 0;
                None
            }
            Key::Named(Named::PageUp | Named::PageDown) if d.pending.view_all => {
                let max = d
                    .pending
                    .input_rows
                    .load(std::sync::atomic::Ordering::Relaxed)
                    .saturating_sub(INPUT_VIEW_ROWS);
                let step = INPUT_VIEW_ROWS / 2;
                d.pending.input_scroll = if matches!(k, Key::Named(Named::PageUp)) {
                    d.pending.input_scroll.saturating_sub(step)
                } else {
                    (d.pending.input_scroll + step).min(max)
                };
                None
            }
            Key::Text(_) | Key::Named(Named::Enter) if wait => None,
            Key::Text(t) if t.len() == 1 && t.as_bytes()[0].is_ascii_digit() && t != "0" => {
                let i = usize::from(t.as_bytes()[0] - b'1');
                (i < n).then_some(i)
            }
            Key::Named(Named::Enter) => Some(d.pending.choice),
            Key::Named(Named::Escape | Named::CtrlC) => Some(usize::MAX), // the last option is "no"
            _ => None,
        };
        let Some(i) = pick else { return };
        let key = if i == usize::MAX {
            if matches!(d.kind, DialogKind::Plan { .. }) {
                "keep"
            } else {
                "no"
            }
            .to_string()
        } else {
            d.pending
                .options
                .as_ref()
                .map(|o| o[i].key.clone())
                .unwrap_or_default()
        };
        self.answer(&key);
    }

    fn answer(&mut self, key: &str) {
        let mut d = self.dialogs.remove(0);
        match &mut d.kind {
            DialogKind::Approval {
                suggestions, reply, ..
            } => {
                let a = match key {
                    "yes" => Approval::allow(),
                    "always" => Approval {
                        accepted: suggestions.clone(),
                        ..Approval::allow()
                    },
                    _ => Approval::deny(DENY_MESSAGE),
                };
                if let Some(r) = reply.take() {
                    let _ = r.send(a);
                }
            }
            DialogKind::Plan { reply, .. } => {
                let (approved, mode) = match key {
                    "manual" => (true, Some(Mode::Default)),
                    "accept" => (true, Some(Mode::AcceptEdits)),
                    _ => (false, None),
                };
                if let Some(m) = mode {
                    self.mode = m;
                    self.mode_changed();
                }
                let message = (!approved).then(|| {
                    "The user doesn't want to proceed with this plan yet. Stay in plan mode, keep planning, and ask what to change.".to_string()
                });
                if let Some(r) = reply.take() {
                    let _ = r.send(Some(UserReply::Plan {
                        approved,
                        message,
                        mode,
                    }));
                }
            }
            DialogKind::Question {
                questions, reply, ..
            } => {
                let mut answers = serde_json::Map::new();
                for (i, q) in questions.iter().enumerate() {
                    let text = if key == "no" {
                        "The user declined to answer.".to_string()
                    } else {
                        let st = &d.pending.qstate[i];
                        st.picked
                            .iter()
                            .filter_map(|&k| {
                                if k == q.options.len() {
                                    Some(st.other.trim().to_string())
                                } else {
                                    q.options.get(k).map(|o| o.label.clone())
                                }
                            })
                            .filter(|s| !s.is_empty())
                            .collect::<Vec<_>>()
                            .join(", ")
                    };
                    answers.insert(q.question.clone(), Value::String(text));
                }
                if let Some(r) = reply.take() {
                    let _ = r.send(Some(UserReply::Answers(Value::Object(answers))));
                }
            }
        }
    }

    /// Port of Node `questionKey`.
    fn question_key(&mut self, k: &Key) {
        let d = &mut self.dialogs[0];
        let p = &mut d.pending;
        let many = p.questions.len() > 1;
        let last = if many {
            p.questions.len()
        } else {
            p.questions.len() - 1
        };
        if matches!(k, Key::Named(Named::Escape | Named::CtrlC)) {
            self.answer("no");
            return;
        }
        if many && is_key(k, Named::Tab) {
            p.qi = last.min(p.qi + 1);
            return;
        }
        if many && is_key(k, Named::ShiftTab) {
            p.qi = p.qi.saturating_sub(1);
            return;
        }
        if many && p.qi >= p.questions.len() {
            match k {
                Key::Named(Named::Enter) => self.answer("yes"),
                Key::Named(Named::Left) => p.qi -= 1,
                _ => {}
            }
            return;
        }
        let q = p.questions[p.qi].clone();
        let st: &mut QState = &mut p.qstate[p.qi];
        let n = q.options.len() + 1; // + Other
        let is_other = st.choice == q.options.len();
        let mut finished = false;
        let advance = |qi: &mut usize, finished: &mut bool| {
            if many {
                *qi = last.min(*qi + 1);
            } else {
                *finished = true;
            }
        };
        if is_other && st.typing {
            match k {
                Key::Text(t) | Key::Paste(t) => st.other.push_str(&t.replace('\n', " ")),
                Key::Named(Named::Backspace) => {
                    st.other.pop();
                }
                Key::Named(Named::Enter) => {
                    st.typing = false;
                    if !st.other.trim().is_empty() {
                        if !q.multi_select {
                            st.picked = BTreeSet::from([st.choice]);
                        } else {
                            st.picked.insert(st.choice);
                        }
                        if !q.multi_select {
                            advance(&mut p.qi, &mut finished);
                        }
                    }
                }
                Key::Named(Named::Up | Named::Down) => st.typing = false,
                _ => {}
            }
            if st.typing || is_key(k, Named::Enter) {
                if finished {
                    self.answer("yes");
                }
                return;
            }
        }
        let toggle = |st: &mut QState| {
            if !st.picked.remove(&st.choice) {
                st.picked.insert(st.choice);
            }
        };
        match k {
            Key::Named(Named::Up) => st.choice = (st.choice + n - 1) % n,
            Key::Named(Named::Down) => st.choice = (st.choice + 1) % n,
            Key::Named(Named::Left) if many => p.qi = p.qi.saturating_sub(1),
            Key::Named(Named::Right) if many => p.qi = last.min(p.qi + 1),
            Key::Text(t)
                if t.len() == 1
                    && (b'1'..=b'9').contains(&t.as_bytes()[0])
                    && usize::from(t.as_bytes()[0] - b'0') <= n =>
            {
                st.choice = usize::from(t.as_bytes()[0] - b'1');
                if st.choice == q.options.len() {
                    st.typing = true;
                } else if q.multi_select {
                    toggle(st);
                } else {
                    st.picked = BTreeSet::from([st.choice]);
                    advance(&mut p.qi, &mut finished);
                }
            }
            Key::Text(t) if t == " " && q.multi_select => {
                if is_other {
                    st.typing = true;
                } else {
                    toggle(st);
                }
            }
            Key::Named(Named::Enter) => {
                if is_other && (st.other.is_empty() || !st.picked.contains(&st.choice)) {
                    st.typing = true;
                } else if q.multi_select {
                    advance(&mut p.qi, &mut finished);
                } else {
                    st.picked = BTreeSet::from([st.choice]);
                    advance(&mut p.qi, &mut finished);
                }
            }
            Key::Text(t) if is_other => {
                st.typing = true;
                st.other.push_str(t);
            }
            _ => {}
        }
        if finished {
            self.answer("yes");
        }
    }

    // ------------------------------------------------------------------ mouse + selection

    fn geom(&self) -> Option<sel::Geom> {
        let f = self.frame.as_ref()?;
        let r = f.rects.chat_rect?;
        let b = f.chat_body.as_ref()?;
        sel::body_geom(
            Some(sel::Rect {
                x: r.x,
                y: r.y,
                w: r.w,
            }),
            Some(sel::Body {
                total: b.total as i64,
                h: b.h as i64,
            }),
            self.scroll as i64,
        )
    }

    fn in_chat(&self, p: Point) -> bool {
        self.frame
            .as_ref()
            .and_then(|f| f.rects.chat_rect)
            .is_some_and(|r| p.x >= r.x && p.x < r.x + r.w && p.y >= r.y && p.y < r.y + r.h)
    }

    /// Below the message rows, inside the chat pane: working line, prompt box, suggestions.
    fn in_input(&self, p: Point) -> bool {
        self.in_chat(p)
            && self.frame.as_ref().is_some_and(|f| {
                let top = f.rects.chat_rect.map_or(0, |r| r.y)
                    + f.chat_body.as_ref().map_or(0, |b| b.h as i64);
                p.y >= top
            })
    }

    /// Clipboard text from [`Cmd::PasteClipboard`]: sanitised like a bracketed paste and
    /// inserted at the cursor (never submitted).
    pub fn paste_clipboard(&mut self, text: &str, now: i64) {
        if text.is_empty() {
            self.toast("clipboard is empty", now, true);
            return;
        }
        let mut out = vec![];
        self.paste_activity(now);
        self.agent_key(&Key::Paste(clean_paste(text)), now, &mut out);
    }

    pub fn clear_selection(&mut self) {
        self.sel = SelState::default();
    }

    fn mouse(&mut self, k: &Key, now: i64, out: &mut Vec<Cmd>) {
        match *k {
            Key::Wheel { dir, x, y, mods } => {
                let ev = SelEvent::Wheel {
                    shift: mods.shift,
                    alt: mods.alt,
                };
                let (mode, act) = selection_transition(self.sel.mode(), ev);
                if act == Act::Clear {
                    self.clear_selection();
                } else {
                    self.sel.mode = Some(mode);
                }
                let p = Point {
                    x: i64::from(x) - 1,
                    y: i64::from(y) - 1,
                };
                if self.in_chat(p) && matches!(dir, WheelDir::Up | WheelDir::Down) {
                    let max = self.frame.as_ref().map_or(0, |f| f.rects.max_scroll);
                    self.scroll = if dir == WheelDir::Up {
                        (self.scroll + WHEEL_LINES).min(max)
                    } else {
                        self.scroll.saturating_sub(WHEEL_LINES)
                    };
                    self.follow_pointer();
                }
            }
            // motion with no button (terminals that report it): hint when over a link
            Key::MouseDrag {
                x, y, button: 3, ..
            } if self.sel.mode() != SelMode::Dragging => {
                let p = Point {
                    x: i64::from(x) - 1,
                    y: i64::from(y) - 1,
                };
                if self.dialogs.is_empty() && self.in_chat(p) && self.link_under(p).is_some() {
                    self.toast("ctrl+click to open", now, false);
                }
            }
            Key::MouseDrag { x, y, .. } => {
                if self.sel.mode() != SelMode::Dragging || self.sel.selection.is_none() {
                    return;
                }
                let p = Point {
                    x: i64::from(x) - 1,
                    y: i64::from(y) - 1,
                };
                self.sel.pointer = Some(p);
                if let Some(g) = self.geom() {
                    let c = sel::point_to_content(&g, p);
                    if let Some(pr) = self.sel.press
                        && pr != c
                    {
                        if let Some(s) = self.sel.selection.as_mut() {
                            s.a = pr;
                        }
                        self.sel.press = None;
                        self.sel.extending = false;
                    }
                    if let Some(s) = self.sel.selection.as_mut() {
                        s.b = c;
                    }
                }
            }
            Key::Mouse {
                x,
                y,
                button: 0,
                action: MouseAction::Up,
                mods,
            } if self.sel.mode() == SelMode::Dragging => {
                let _ = (x, y);
                let extending = std::mem::take(&mut self.sel.extending);
                self.sel.press = None;
                let Some(s) = self.sel.selection else {
                    self.sel.mode = Some(SelMode::Idle);
                    return;
                };
                let moved = s.a != s.b;
                let (mode, act) = selection_transition(
                    self.sel.mode(),
                    SelEvent::Up {
                        moved,
                        shift: mods.shift,
                        alt: mods.alt,
                        extending,
                    },
                );
                self.sel.mode = Some(mode);
                match act {
                    Act::Copy => self.copy_selection(now, out),
                    Act::Pend => self.toast(
                        "selection started · hold Shift and scroll, then click where it should end · esc cancels",
                        now,
                        false,
                    ),
                    _ => self.clear_selection(),
                }
            }
            // ctrl / alt / secondary click on a link or path: open it (see `open_gesture`)
            Key::Mouse {
                x,
                y,
                button,
                action: MouseAction::Down,
                mods,
            } if self.dialogs.is_empty()
                && open_gesture(button, mods)
                && self.try_open(x, y, button, mods, now, out) => {}
            // a dialog owns the mouse: a click picks the option under it
            Key::Mouse {
                y,
                button: 0,
                action: MouseAction::Down,
                ..
            } if !self.dialogs.is_empty() => self.dialog_click(i64::from(y) - 1, now),
            Key::Mouse {
                x,
                y,
                button: 0,
                action: MouseAction::Down,
                mods,
            } => self.mouse_down(x, y, mods, now, out),
            // right click in the prompt box = paste; never while a dialog, picker or search
            // owns the keyboard (a click must not answer or feed them)
            Key::Mouse {
                x,
                y,
                button: 2,
                action: MouseAction::Down,
                ..
            } if self.dialogs.is_empty() && self.picker.is_none() && self.search.is_none() => {
                let p = Point {
                    x: i64::from(x) - 1,
                    y: i64::from(y) - 1,
                };
                if self.in_input(p) {
                    out.push(Cmd::PasteClipboard);
                }
            }
            _ => {}
        }
    }

    /// The link under screen cell `p`. Chat rows are looked up with their soft-wrapped neighbours
    /// joined (a long url or path may span rows); anything else from the drawn row.
    fn link_under(&self, p: Point) -> Option<Link> {
        if let (Some(g), Some(body)) = (
            self.geom(),
            self.frame.as_ref().and_then(|f| f.chat_body.as_ref()),
        ) {
            let b = sel::body_bounds(&g);
            let c = sel::point_to_content(&g, p);
            if p.y >= b.y0 && p.y <= b.y1 && c.y >= 0 {
                let rows = |y: i64| body_sel_rows(body, y.max(0) as usize, y.max(0) as usize).pop();
                let (text, col) = join_wrapped(&rows, c.y, p.x - g.x0)?;
                return link_at(&text, col);
            }
        }
        let row = self.lines.get(p.y.max(0) as usize)?;
        link_at(row, p.x.max(0) as usize)
    }

    /// Opens the link under the pointer. `false` leaves the click to the selection logic: nothing
    /// there, the prompt box's right click (paste), or an alt+click on text that is no real path.
    fn try_open(
        &mut self,
        x: u32,
        y: u32,
        button: u32,
        mods: synthesis_tui_base::keys::Mods,
        now: i64,
        out: &mut Vec<Cmd>,
    ) -> bool {
        let p = Point {
            x: i64::from(x) - 1,
            y: i64::from(y) - 1,
        };
        // right click on the prompt line stays paste (the streaming tail above it is chat too)
        let on_prompt = self.caret.is_some_and(|(_, cy)| i64::from(cy) == p.y);
        if !self.in_chat(p) || (button == 2 && on_prompt) {
            return false;
        }
        let Some(link) = self.link_under(p) else {
            return false;
        };
        match link {
            Link::Url(u) => out.push(Cmd::Open(OpenTarget::Url(u))),
            Link::Path(raw) => {
                let found = existing_path(
                    &raw,
                    self.cfg.home.as_deref(),
                    Some(Path::new(&self.cfg.cwd)),
                );
                match found {
                    Some(t) => out.push(Cmd::Open(OpenTarget::from_path(&t))),
                    // alt+click is also "extend selection": only a real path takes it over
                    None if mods.ctrl || button == 2 => {
                        self.toast(format!("not found {}", clean_line(&raw)), now, true);
                    }
                    None => return false,
                }
            }
        }
        self.clear_selection();
        true
    }

    /// Click on screen row `y` while a permission / plan dialog is open: the option drawn there is
    /// chosen and confirmed (as Enter would, so the arming delay applies). Questions are keyboard
    /// only.
    fn dialog_click(&mut self, y: i64, now: i64) {
        let Some(d) = self.dialogs.first() else {
            return;
        };
        let n = d.pending.options.as_ref().map_or(0, Vec::len);
        if matches!(d.kind, DialogKind::Question { .. }) || n == 0 {
            return;
        }
        let rows: Vec<Option<usize>> = self
            .lines
            .iter()
            .map(|l| option_no(&strip_ansi(l)))
            .collect();
        // the last "1." starts the options; later rows (wrapped labels) belong to the option above
        let Some(first) = rows.iter().rposition(|r| *r == Some(1)) else {
            return;
        };
        let y = y.max(0) as usize;
        if y < first {
            return;
        }
        let hit = rows[first..=y.min(rows.len() - 1)]
            .iter()
            .rev()
            .find_map(|r| *r);
        if let Some(no) = hit.filter(|no| *no <= n) {
            self.dialogs[0].pending.choice = no - 1;
            self.choice_key(&Key::Named(Named::Enter), now);
            self.sync_arming(now);
        }
    }

    fn follow_pointer(&mut self) {
        if self.sel.mode() == SelMode::Dragging
            && let (Some(g), Some(p)) = (self.geom(), self.sel.pointer)
            && let Some(s) = self.sel.selection.as_mut()
        {
            s.b = sel::point_to_content(&g, p);
        }
    }

    fn mouse_down(
        &mut self,
        x: u32,
        y: u32,
        mods: synthesis_tui_base::keys::Mods,
        now: i64,
        out: &mut Vec<Cmd>,
    ) {
        let p = Point {
            x: i64::from(x) - 1,
            y: i64::from(y) - 1,
        };
        let g = if self.in_chat(p) { self.geom() } else { None };
        let b = g.as_ref().map(sel::body_bounds);
        let in_body = b.is_some_and(|b| p.y >= b.y0 && p.y <= b.y1);
        let plain = !(mods.shift || mods.alt);
        let run = match self.last_click {
            Some((t, q, n)) if plain && q == p && now - t < MULTI_CLICK_MS => (n + 1).min(3),
            _ => 1,
        };
        self.last_click = plain.then_some((now, p, run));
        if run >= 2
            && let Some(g) = g
            && in_body
            && self.select_unit(&g, p, run, now, out)
        {
            return;
        }
        // a plain press starts afresh; only shift/alt extend a kept selection
        if plain {
            self.clear_selection();
        }
        // a drag may start on any cell of the body, blank ones included
        let on_text = in_body
            && b.is_some_and(|b| {
                let g = g.as_ref().unwrap();
                !(p.x < b.x0 || p.x >= b.x1 || p.y - g.top + g.row_at0 < 0)
            });
        let (mode, act) = selection_transition(
            self.sel.mode(),
            SelEvent::Down {
                text: on_text,
                body: in_body,
                shift: mods.shift,
                alt: mods.alt,
            },
        );
        if act == Act::Extend
            && self.sel.selection.is_some()
            && let Some(g) = g
        {
            self.sel.pointer = Some(p);
            self.sel.press = (self.sel.mode() == SelMode::Selected && !(mods.shift || mods.alt))
                .then(|| sel::point_to_content(&g, p));
            self.sel.extending = true;
            if let Some(s) = self.sel.selection.as_mut() {
                s.b = sel::point_to_content(&g, p);
            }
            self.sel.mode = Some(mode);
            return;
        }
        self.clear_selection();
        if let (Some(g), true) = (g, on_text) {
            self.sel.pointer = Some(p);
            let c = sel::point_to_content(&g, p);
            self.sel.selection = Some(Selection { a: c, b: c });
            self.sel.mode = Some(SelMode::Dragging);
        }
        let _ = now;
    }

    /// Double click selects the word under the pointer, triple click the line; both copy at once.
    fn select_unit(
        &mut self,
        g: &sel::Geom,
        p: Point,
        run: u8,
        now: i64,
        out: &mut Vec<Cmd>,
    ) -> bool {
        let c = sel::point_to_content(g, p);
        let Some(body) = self.frame.as_ref().and_then(|f| f.chat_body.as_ref()) else {
            return false;
        };
        let row_at = |y: i64| body_sel_rows(body, y.max(0) as usize, y.max(0) as usize).pop();
        let Some(row) = row_at(c.y) else {
            return false;
        };
        if row.text.trim().is_empty() {
            return false;
        }
        // a word stays on its row; a line is the whole wrapped paragraph
        let (mut y0, mut y1) = (c.y, c.y);
        let (from, to) = if run == 2 {
            match sel::word_span(&row.text, c.x - g.x0) {
                Some(w) => w,
                None => return false,
            }
        } else {
            while y0 > 0 && row_at(y0 - 1).is_some_and(|r| r.soft) {
                y0 -= 1;
            }
            while y1 + 1 < g.total && row_at(y1).is_some_and(|r| r.soft) {
                y1 += 1;
            }
            (0, g.x1 - g.x0)
        };
        self.sel.selection = Some(Selection {
            a: Point {
                x: g.x0 + from,
                y: y0,
            },
            b: Point {
                x: g.x0 + to - 1,
                y: y1,
            },
        });
        self.sel.mode = Some(SelMode::Selected);
        self.copy_selection(now, out);
        true
    }

    fn copy_selection(&mut self, now: i64, out: &mut Vec<Cmd>) {
        let (Some(s), Some(g)) = (self.sel.selection, self.geom()) else {
            self.clear_selection();
            return;
        };
        let text = self
            .frame
            .as_ref()
            .and_then(|f| f.chat_body.as_ref())
            .map_or(String::new(), |body| {
                sel::text_of_rows(&s, g.x0, g.x1, |from, to| {
                    body_sel_rows(body, from.max(0) as usize, to.max(0) as usize)
                })
            });
        if text.is_empty() {
            self.clear_selection();
        } else {
            out.push(Cmd::Copy(text));
        }
        let _ = now;
    }

    // ------------------------------------------------------------------ rendering

    fn session_rows(&self, now: i64) -> Vec<SessionRow> {
        let phase = session_phase(&self.activity, now);
        let busy = self.chat.agent.status != "idle";
        let phase_s = if busy {
            match phase.phase {
                model::Phase::Idle => "thinking",
                model::Phase::Tool => "tool",
                model::Phase::Agents => "agents",
                model::Phase::Thinking => "thinking",
            }
        } else {
            "idle"
        };
        let mut rows = vec![SessionRow {
            id: self.cfg.session_id.clone(),
            project: clean_line(&basename(&self.cfg.cwd)),
            cwd: clean_line(&self.cfg.cwd),
            mtime: now,
            live: Some(Live {
                name: "synthesis".into(),
                cwd: clean_line(&self.cfg.cwd),
                status: if !self.dialogs.is_empty() {
                    "waiting"
                } else if busy {
                    "running"
                } else {
                    "idle"
                }
                .into(),
            }),
            phase: phase_s.into(),
            tokens: self.activity.tokens.total() as f64,
            ctx_pct: None,
            waiting: !self.dialogs.is_empty(),
            mode: Some(mode_str(self.mode).into()),
        }];
        // One row per other folder (the newest): the same repo listed once per stored session is noise.
        let mut seen: Vec<(String, &SavedSession)> = vec![];
        let mut canon: std::collections::HashMap<&str, String> = Default::default();
        let me = fold_dir(&self.cfg.cwd);
        for s in self.cfg.saved.iter() {
            let k = canon
                .entry(s.cwd.as_str())
                .or_insert_with(|| fold_dir(&s.cwd))
                .clone();
            if k == me {
                continue;
            }
            match seen.iter_mut().find(|(n, _)| *n == k) {
                Some((_, n)) if n.mtime < s.mtime => *n = s,
                Some(_) => {}
                None => seen.push((k, s)),
            }
        }
        let newest: Vec<&SavedSession> = seen.into_iter().map(|(_, s)| s).collect();
        for s in newest {
            rows.push(SessionRow {
                id: s.id.clone(),
                project: clean_line(&basename(&s.cwd)),
                cwd: clean_line(&s.cwd),
                mtime: s.mtime,
                live: None,
                phase: "idle".into(),
                tokens: 0.0,
                ctx_pct: None,
                waiting: false,
                mode: None,
            });
        }
        rows
    }

    /// Where the real cursor goes after the last `render` (the prompt caret), if the prompt is shown.
    pub fn caret(&self) -> Option<(u32, u32)> {
        self.caret
    }

    /// One frame of `cols` x `rows` painted lines (the last one is the status line).
    pub fn render(&mut self, cols: u32, rows: u32, now: i64) -> Vec<String> {
        self.size = (cols, rows);
        let (w, h) = (cols as usize, rows as usize);
        if (cols as i64) < MIN_COLS || (rows as i64) < MIN_ROWS + 1 {
            self.caret = None;
            return too_small(w, h);
        }
        let frame_no = (now.max(0) / 80) as usize;
        // dialogs -> pending list, with the live countdown
        self.chat.agent.pending = self
            .dialogs
            .iter()
            .map(|d| {
                let mut p = d.pending.clone();
                if let Some(dl) = d.deadline_ms {
                    let left = ((dl - now) as f64 / 1000.0).ceil().max(0.0) as i64;
                    p.prompt = format!(
                        "{} · expires unanswered in {}:{:02}",
                        p.prompt,
                        left / 60,
                        left % 60
                    )
                    .trim_start_matches(" · ")
                    .to_string();
                }
                p
            })
            .collect();
        if self.armed_in(now).is_some()
            && let Some(p) = self.chat.agent.pending.first_mut()
        {
            p.prompt = format!("{} · wait…", p.prompt)
                .trim_start_matches(" · ")
                .to_string();
        }
        self.chat.agent.mode = mode_str(self.mode).into();
        self.chat.agent.model = clean_line(&self.model);
        // one source for the header, the Status panel and the footer: the harness's context meter
        let (used, window) = self.chat.ctx.numbers();
        self.activity.context_used = used;
        self.activity.context_estimated = true;
        self.activity.context_display_reset = false;
        self.chat.agent.context_window = (window > 0).then_some(window as f64);
        let rows_s = self.session_rows(now);
        let key = workspace_key(&self.cfg.cwd);
        let ws = [Workspace {
            key: key.clone(),
            name: clean_line(&basename(&self.cfg.cwd)),
            cwd: clean_line(&self.cfg.cwd),
            count: (1 + self
                .cfg
                .saved
                .iter()
                .filter(|s| s.id != self.cfg.session_id)
                .count()) as u32,
            waiting: !self.dialogs.is_empty(),
            busy: self.chat.agent.status != "idle",
        }];
        let lead = self.bash() || self.memory();
        let mut opts = AgentOpts {
            input_mode: if self.bash() {
                Some("bash".to_string())
            } else {
                self.memory().then(|| "memory".to_string())
            },
            scroll: self.scroll,
            expanded: self.expanded,
            selected: self.sug_selected,
            queued: self
                .queue
                .iter()
                .cloned()
                .chain(self.chat.inflight.iter().map(|(_, t)| t.clone()))
                .collect(),
            agent_rows: self.agent_rows(),
            route: self.target.as_ref().map(|t| {
                let (st, q) = self
                    .row(t)
                    .map_or(("gone", 0), |r| (r.status.as_str(), r.queued));
                format!(
                    "@{t} · {st}{}",
                    if q > 0 {
                        format!(" · {q} queued")
                    } else {
                        String::new()
                    }
                )
            }),
            suggestions: (!self.sug_view.is_empty()).then(|| RSug {
                items: self.sug_view.clone(),
            }),
            ..AgentOpts::default()
        };
        if let Some(ps) = &self.picker {
            opts.picker = Some(ps.picker.clone());
        }
        if let Some(s) = &self.search {
            opts.search = Some(Search {
                query: s.query.clone(),
                matched: (!s.matched.is_empty()).then(|| s.matched.clone()),
            });
        }
        let view = DashView {
            opts,
            ..DashView::default()
        };
        let usage = Usage::default();
        let sid = self.cfg.session_id.clone();
        let frame = {
            let state = DashState {
                session: &self.activity,
                sessions: &rows_s,
                index: 0,
                usage: &usage,
                follow: false,
                history: &[],
                usage_metric: "tokens",
                mcp_servers: &[],
                mcp_live: None,
                workspaces: Some(&ws),
                active_workspace: &key,
                agent: Some(&self.chat.agent),
                chat_agent: None,
                editor: Some(EditorView {
                    chars: &self.editor.chars[usize::from(lead)..],
                    cur: self.editor.cur.saturating_sub(usize::from(lead)),
                }),
                focus: Focus::Agent,
                view: &view,
                agent_session_id: Some(&sid),
                tz_offset_min: self.cfg.tz_offset_min,
            };
            render_frame(&state, w, h - 1, frame_no, now)
        };
        let mut lines = frame.lines.clone();
        lines.push(status_line_with(
            &self.cfg.sandbox,
            &self.cfg.budget,
            &self.chat.totals,
            self.chat.verification.as_ref(),
            self.ran_task,
            self.chat
                .task_started
                .map_or(0, |t| (now - t).max(0) as u64),
            w,
            &StatusExtra {
                context: Some(&self.chat.ctx),
                now,
                expanded: self.expanded,
            },
        ));
        self.caret = frame.rects.caret.map(|(x, y)| (x as u32, y as u32));
        self.frame = Some(frame);
        self.lines.clone_from(&lines);
        // selection highlight is painted on a copy: hit tests keep reading the plain lines
        if let (Some(s), Some(g)) = (self.sel.selection, self.geom()) {
            let b = sel::body_bounds(&g);
            lines =
                sel::apply_selection(&lines, Some(&sel::to_screen(&s, &g)), Some(&b)).into_owned();
        }
        if let Some(t) = &self.toast {
            let color = if t.bad { T_ERROR } else { T_SUCCESS };
            let last = lines.len() - 1;
            lines[last] = format!(
                "{}\x1b[7m{}{}",
                theme::fg(color),
                theme::fit(
                    &format!(" {} {} ", if t.bad { "✗" } else { "✓" }, t.text),
                    w
                ),
                theme::RESET
            );
        }
        lines
    }
}

/// `1` of a dialog option row such as `│ ❯ 1. Yes` or `│   2. No`.
fn option_no(row: &str) -> Option<usize> {
    let cs: Vec<char> = row.chars().collect();
    (2..cs.len().saturating_sub(2)).find_map(|i| {
        let d = cs[i].to_digit(10).filter(|d| *d > 0)?;
        (cs[i + 1] == '.' && cs[i + 2] == ' ' && cs[i - 1] == ' ' && matches!(cs[i - 2], '❯' | ' '))
            .then_some(d as usize)
    })
}

fn n_idx(s: &complete::Suggestions) -> usize {
    s.items.len() - 1
}

/// Suggestion in the shape the pane's `always_label` reads.
fn suggestion_view(s: &Suggestion) -> Value {
    match s {
        Suggestion::AddRules { rules, behavior } => json!({
            "type": "addRules",
            "behavior": format!("{behavior:?}").to_lowercase(),
            "rules": rules.iter().map(|r| json!({
                "toolName": clean_line(&r.tool_name),
                "ruleContent": r.rule_content.as_deref().map(clean_line).unwrap_or_default(),
            })).collect::<Vec<_>>(),
        }),
        Suggestion::SetMode(m) => json!({"type": "setMode", "mode": m.as_str()}),
    }
}

fn list_dirs(dir: &Path) -> Vec<String> {
    let mut v: Vec<String> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .filter_map(|e| e.file_name().into_string().ok())
        .filter(|n| !n.starts_with('.'))
        .collect();
    v.sort_by_key(|n| n.to_lowercase());
    v.truncate(500);
    v
}

pub(crate) fn too_small(w: usize, h: usize) -> Vec<String> {
    // The longest wording that fits whole (no cut mid-word at 30 columns, QA B-27).
    let need = format!("{}x{}", MIN_COLS, MIN_ROWS + 1);
    let text = [
        format!("terminal too small · need {need}"),
        format!("too small · need {need}"),
        format!("need {need}"),
    ]
    .into_iter()
    .find(|t| t.chars().count() <= w)
    .unwrap_or_else(|| need.clone());
    let mut lines = vec![" ".repeat(w); h];
    if h > 0 {
        let pad = w.saturating_sub(text.chars().count()) / 2;
        lines[h / 2] = theme::fit(&format!("{}{text}", " ".repeat(pad)), w);
    }
    lines
}
