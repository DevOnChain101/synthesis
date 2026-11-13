//! Live state of the desktop TUI view: the `ViewState` the engine's events feed, the dialogs waiting
//! for the person, the prompt queue and history, the sidebar selection. Plain Rust, no gpui; the window
//! (`tui_view`) reads it and calls the methods below, the pump applies `UiMsg`s with [`Live::on_msg`].
//! The rules follow the CLI (`docs/CLI-APP.md`; `crates/synthesis-cli-ui/src/app.rs`).

use std::collections::{HashSet, VecDeque};
use std::sync::Arc;

use serde_json::Value;
use synthesis_cli_ui_base::complete::Command;
use synthesis_cli_ui_render::dialogs::permission_options;
use synthesis_harness::{UserReply, sanitize_for_display};
use synthesis_permissions::{Mode, Suggestion};
use synthesis_tools::{NeedsUser, Question};
use synthesis_view::convo::Item;
use synthesis_view::state::ViewState;

use crate::engine::{AskApproval, Choice, Engine, McpInfo, UiMsg, is_command};
use crate::models::ModelRow;
use crate::send_pick::{self, Seen, SendPick, SendTo};

/// Enter and digits are ignored this long after a dialog comes to the front: a key typed before it
/// appeared must not answer it (the CLI's `ARM_MS`). Esc still rejects.
pub const ARM_MS: i64 = 400;
/// The second Ctrl+C that quits has to come within this long (the CLI's `QUIT_ARM_MS`).
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
        /// "Other": the free-text answer per question.
        other: Vec<Option<String>>,
        /// The text being typed for "Other" (Enter keeps it, Esc drops it).
        typing: Option<String>,
    },
    /// `/model`, `/resume` and `/send`: a list to choose from. `headers`: (index of the first option under
    /// it, text); an option's key is the model or session id.
    Picker {
        kind: PickerKind,
        title: String,
        headers: Vec<(usize, String)>,
    },
    /// `/rewind`, step two: the exact files, and the confirmation. Cancel is the selected default.
    Rewind(Box<crate::rewind::RewindDialog>),
    /// `/clean`, step two: what goes, what stays, the backup toggle and (for "all") the word to type.
    /// Cancel is the first row and the selected one; Esc closes it.
    Clean(Box<crate::clean::CleanDialog>),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PickerKind {
    Model,
    Resume,
    /// `#note`: which instruction file gets it (or none).
    Remember,
    /// `/memory review`: the pending records to choose from.
    MemReview,
    /// A change to memory (add, edit, forget, accept): says what it will do; the person must engage it.
    MemConfirm,
    /// No model configured: the CLI's setup card (check again, Settings, quit).
    Setup,
    /// Shift+Tab reached bypassPermissions: the confirmation (Cancel is the first option and the default).
    Bypass,
    /// `/rewind`: the checkpoints to choose from (a dry run follows; nothing is written by this).
    Rewind,
    /// `/clean`: the scopes with their counts (a confirmation follows; nothing is deleted by this).
    Clean,
    /// `/send` with no arguments: which live peer the next line goes to.
    Send,
    /// `/close-workspace` and the Workspace panel's close button: Cancel is the first option and the default.
    CloseWorkspace,
    /// `/trust revoke`: confirm forgetting a remembered root (Keep is first and the default).
    Revoke,
    /// `/trust ask` in an untrusted folder: trust it for this session? ("Not now" is first and the
    /// default.)
    Trust,
}

/// What the Trust dialog granted: the project root the session restarts trusted for, and, when the person chose
/// to remember it, the root to record (the project root, or its parent for "all its subfolders") with its scope.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TrustGrant {
    pub root: std::path::PathBuf,
    pub remember: Option<(std::path::PathBuf, &'static str)>,
}

/// What the window does after a command or a picker (it restarts the engine, talks to it).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Action {
    /// Restart on the same session with this `provider/model`.
    Model(String),
    /// The model picker's Refresh row: `synt models --refresh` off the UI thread; the result comes
    /// back through [`Live::models_refreshed`].
    RefreshModels,
    /// `/open <path>`: a typed folder path as a new workspace.
    Open(String),
    /// `/context <tokens|unset>`: the window of the model in use, written to the user synt.json.
    SetContext(String),
    Resume(String),
    /// `/trust revoke` ended the trust of this root (and what is under it): a session trust kept for it goes too.
    TrustRevoked(String),
    /// `/trust ask`: raises the Trust dialog for the session's folder.
    AskTrust,
    /// A fresh session in the selected workspace.
    New,
    /// The confirmation of `/close-workspace` was answered yes: close every session of this
    /// workspace key (see `close_ws`).
    CloseWorkspace(String),
    /// Hide this session, open a fresh one in its folder.
    Close,
    Clear,
    Compact(Option<String>),
    /// `/config`: the shell shows the Settings view.
    OpenSettings,
    /// `/panels [arg]`: the view lists, hides or shows panels and saves the choice.
    Panels(String),
    /// `/files`: the centre panel shows its file explorer tab (the project tree and the changes).
    ShowFiles,
    /// `/reveal [path]`: the folder (or a path the agent printed) in the system file manager.
    Reveal(String),
    /// `/diff [file]`: the centre panel shows its diff tab, with `file` open when one is named.
    ShowDiff(Option<String>),
    /// The Trust dialog was accepted: the window restarts this session with the folder trusted.
    TrustFolder(TrustGrant),
    /// Setup card: build the engine again (the person set a model and a key meanwhile).
    Recheck,
    /// Setup card: quit the app.
    Quit,
    /// `/peers`: the listing is read off the UI thread; the text comes back through [`Live::peers_done`].
    Peers,
    /// `/doctor`: the checks run off the UI thread; the rows come back through [`Live::doctor_done`].
    Doctor,
    /// `/send ADDRESS TEXT`: runs off the UI thread; the outcome comes back through [`Live::send_done`].
    Send {
        to: String,
        text: String,
    },
    /// `/rewind`: read the session's checkpoints off the UI thread ([`Live::rewind_listed`]).
    Rewind,
    /// A checkpoint was picked (None: all): make the dry run ([`Live::rewind_previewed`]).
    RewindPreview(Option<String>),
    /// The person confirmed this dry run: write it ([`Live::rewind_done`]).
    RewindApply(crate::rewind::Preview),
    /// `/clean`: count what each scope would delete, off the UI thread ([`Live::clean_listed`]).
    Clean,
    /// The person confirmed: delete these sessions (of this scope, still in scope and not in `keep`).
    CleanApply {
        scope: synthesis_session::CleanScope,
        ids: Vec<String>,
        backup: bool,
        keep: Vec<String>,
    },
}

/// An image waiting for the next message (the CLI's chip).
#[derive(Clone, Debug)]
pub struct Attached {
    pub name: String,
    pub media_type: &'static str,
    pub bytes: usize,
    pub base64: String,
}

/// The CLI's limits (`docs/CLI-APP.md`): images per message and base64 bytes per message (the session
/// store refuses a message over 8 MiB).
pub const MAX_ATTACHMENTS: usize = 5;
pub const MAX_ATTACHED_BYTES: usize = 6 << 20;

#[derive(Clone, Debug)]
pub struct Dialog {
    pub kind: DialogKind,
    /// (key, label) the person can pick; the last one rejects.
    pub options: Vec<(String, String)>,
    pub sel: usize,
    /// Enter and digits do nothing before this (ms).
    pub armed_until: i64,
    /// The person has deliberately engaged the dialog (arrow keys, Tab, a click on it). Until then digits and
    /// Enter do not answer a dialog that needs the person: they were probably meant for the prompt.
    pub engaged: bool,
}

impl Dialog {
    pub fn id(&self) -> &str {
        match &self.kind {
            DialogKind::Approval(a) => &a.id,
            DialogKind::Plan { id, .. } | DialogKind::Question { id, .. } => id,
            DialogKind::Picker { .. } | DialogKind::Rewind(_) | DialogKind::Clean(_) => "",
        }
    }

    /// Index of the highlighted option among the children of a picker's list (its group headers
    /// count), for keeping it in view.
    pub fn scroll_child(&self) -> Option<usize> {
        let DialogKind::Picker { headers, .. } = &self.kind else {
            return None;
        };
        Some(self.sel + headers.iter().filter(|(i, _)| *i <= self.sel).count())
    }

    /// The person's permission or answer is awaited (the red "needs you" signal); a picker they opened is not,
    /// the bypass confirmation is.
    pub fn needs_person(&self) -> bool {
        match &self.kind {
            DialogKind::Picker { kind, .. } => {
                matches!(
                    kind,
                    PickerKind::Bypass
                        | PickerKind::MemConfirm
                        | PickerKind::Trust
                        | PickerKind::Revoke
                        | PickerKind::Clean
                        | PickerKind::CloseWorkspace
                )
            }
            _ => true,
        }
    }

    /// Digits and Enter may answer this dialog: it is a picker the person opened, or they engaged it.
    pub fn answerable(&self) -> bool {
        let send = matches!(
            self.kind,
            DialogKind::Picker {
                kind: PickerKind::Send,
                ..
            }
        );
        self.engaged || !(self.needs_person() || send)
    }

    /// Digits pick option 1-9. Not in the model and send pickers: there they type into the filter.
    pub fn digits_pick(&self) -> bool {
        !matches!(
            self.kind,
            DialogKind::Picker {
                kind: PickerKind::Model | PickerKind::Send,
                ..
            }
        )
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
            DialogKind::Picker { title, .. } => title.clone(),
            DialogKind::Rewind(_) => "Rewind files: confirm".into(),
            DialogKind::Clean(c) => format!(
                "Clean: confirm PERMANENT deletion of {}",
                crate::clean::scope_title(c.scope)
            ),
        }
    }
}

/// `s` cut from the left to `n` chars (`\u{2026}` first): the end of a path is the part that tells.
pub fn clip_left(s: &str, n: usize) -> String {
    let c: Vec<char> = s.chars().collect();
    if c.len() <= n {
        s.to_string()
    } else {
        std::iter::once('\u{2026}')
            .chain(c[c.len() - (n - 1)..].iter().copied())
            .collect()
    }
}

/// What the Trust dialog says, one paragraph each: the canonical root that would be trusted (and that it is
/// not the folder the person is in, when so), what trust enables, what it risks, what the restart does.
pub fn trust_text(root: &std::path::Path, cwd: &str) -> Vec<String> {
    let shown = |p: &str| clip_left(&sanitize_for_display(p), 72);
    let mut v = vec![format!(
        "Trust root: {}",
        shown(&root.display().to_string())
    )];
    if root != std::path::Path::new(cwd) {
        v.push(format!(
            "This is NOT the folder you are in ({}): it is the nearest parent holding .git or .synt, and everything under it is trusted.",
            shown(cwd)
        ));
    }
    v.push("Enables: the project's agents, skills, memory and instruction files (data, not execution). A REMEMBERED trust deliberately goes no further: the project's hooks, MCP servers, permission allow rules, defaultMode, additionalDirectories, env, verification checks and provider settings (URLs, keys, headers) stay as in an untrusted folder, and a notice at every start lists what was ignored. Only \"Trust for this session only\" applies them, for that run, and each MCP server still needs its own approval as NAME@HASH (--approve-mcp at launch or your user settings; `synt mcp get` shows the hash). This is narrower than VS Code's workspace trust. The scripts that hooks and MCP servers name are checked for changes on a best-effort basis; other code those scripts run in turn is NOT covered.".into());
    v.push("Risks: a repository you did not write can carry hooks that run commands (one can run at the restart that follows a session-only trust), allow rules that skip prompts, an env that redirects programs, and provider endpoints that receive your prompts and keys; a session-only trust applies all of that. A later git pull can change agents, skills, instructions or memory without any prompt; if the trusted configuration changes, remembered trust is not honoured: the folder starts untrusted and you are asked again, with the changed file names.".into());
    v.push("The session restarts now and must be idle: the saved conversation is reloaded (if it cannot be, a fresh session starts and a note says so), this session's 'always allow' grants and its permission mode are reset. \"Trust this folder (remember)\" records the root in your own session store (never in the project) for later sessions, resumed ones included, with the limits above; /trust lists and revokes it. \"Trust for this session only\" saves nothing, but it lasts until the app exits and covers every session of that folder (/trust revoke ends it). Not now changes nothing: nothing from the folder is loaded.".into());
    v
}

/// What the Trust dialog says when the engine's sandbox does not enforce: a remembered trust is then stored but not
/// honoured at later starts (a command that can run unconfined could have written the store).
pub fn enforcement_paragraph(enforcing: Option<bool>) -> Option<String> {
    (enforcing == Some(false)).then(|| {
        "The engine's sandbox is NOT enforcing: \"Trust this folder (remember)\" and the parent grant are stored, but at later starts they will NOT be honoured (a command that can run unconfined could have written the store). Only \"Trust for this session only\" applies; the engine must run with --sandbox enforce for remembered trust.".to_string()
    })
}

/// What `/trust revoke` says once the store answered.
fn revoke_message(root: &str, done: Result<bool, String>) -> String {
    match done {
        Ok(true) => format!(
            "revoked trust of {}: it takes effect at the next engine start (a new session, /model or a restart); this session keeps what it already loaded. A session-only trust of that folder ends too",
            sanitize_for_display(root)
        ),
        Ok(false) => format!(
            "{} was not remembered (a session-only trust of it, if any, ends)",
            sanitize_for_display(root)
        ),
        Err(err) => format!("could not revoke: {err}"),
    }
}

/// The suggestion in the shape `permission_options` reads (the CLI's `suggestion_view`).
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
    /// Send now: deliver the line to `to` (`main` or a sub-agent) at its next safe point.
    Inject {
        to: String,
        text: String,
    },
    /// Refused and explained in a notice; the prompt keeps its text.
    Held,
    /// A command the window carries out.
    Do(Action),
    Quit,
}

const PEERS_OFF: &str = "peers are off (synt.peers.enabled in user settings, or --peers)";

pub const NOT_YET: &str = "not available in the desktop app yet";

/// Built-in commands the desktop app carries out (the others answer [`NOT_YET`]).
pub const WIRED: [&str; 25] = [
    "help",
    "clear",
    "compact",
    "new",
    "close",
    "resume",
    "model",
    "effort",
    "mcp",
    "memory",
    "agents",
    "permissions",
    "status",
    "exit",
    "images",
    "terminal",
    "browser",
    "verbose",
    "config",
    "files",
    "diff",
    "focus",
    "peers",
    "name",
    "rename",
];

fn refresh_label(p: &ModelPick, now: i64) -> String {
    if p.refreshing {
        format!("{} Refreshing the model list...", spinner(now))
    } else {
        "Refresh the model list (asks each provider)".into()
    }
}

/// One frame of the refresh spinner (120 ms each).
fn spinner(ms: i64) -> &'static str {
    const F: [&str; 4] = ["|", "/", "-", "\\"];
    F[(ms.max(0) / 120) as usize % F.len()]
}

/// Desktop-only commands (not in the CLI's list): `/panels`, added to the engine's commands.
pub fn trust_command() -> Command {
    Command {
        name: "trust".into(),
        description: "list the folders you trusted and remembered; revoke one".into(),
        argument_hint: "[revoke <n|path>]".into(),
        local: true,
        ..Command::default()
    }
}

/// Desktop-only commands (not in the CLI's list): `/panels`, added to the engine's commands.
pub fn panels_command() -> Command {
    Command {
        name: "panels".into(),
        description: "show or hide dashboard panels (synt.tui.panels)".into(),
        argument_hint: "[hide-all|show-all|<name> on|off]".into(),
        local: true,
        ..Command::default()
    }
}

pub fn is_wired(name: &str) -> bool {
    // `open`, `reveal`, `send`, `panels`, `doctor`, `trust`, `rewind`, `context` and `clean` exist only in the desktop app
    WIRED.contains(&name)
        || matches!(
            name,
            "open"
                | "reveal"
                | "send"
                | "panels"
                | "doctor"
                | "trust"
                | "rewind"
                | "context"
                | "close-workspace"
                | "clean"
        )
}

/// `/close-workspace` is the desktop's own command (the CLI has none).
pub fn close_workspace_command() -> Command {
    Command {
        name: "close-workspace".into(),
        aliases: vec![],
        description: "close every session of this workspace (asks first; nothing is deleted)"
            .into(),
        argument_hint: String::new(),
        local: true,
    }
}

/// `/rewind` is the desktop's own command too: the engine's list has none.
pub fn rewind_command() -> Command {
    Command {
        name: "rewind".into(),
        aliases: vec![],
        description: "put files a run changed back to a checkpoint (dry run first, then confirm)"
            .into(),
        argument_hint: String::new(),
        local: true,
    }
}

/// `/clean` is the desktop's own command too.
pub fn clean_command() -> Command {
    Command {
        name: "clean".into(),
        aliases: vec![],
        description: "permanently delete stored sessions (pick a scope, then confirm)".into(),
        argument_hint: String::new(),
        local: true,
    }
}

/// `/reveal` is the desktop's own command (the CLI has none), so it is not in [`WIRED`].
pub fn reveal_command() -> Command {
    Command {
        name: "reveal".into(),
        aliases: vec![],
        description: "show the folder, or a path the agent printed, in the file manager".into(),
        argument_hint: "[path]".into(),
        local: true,
    }
}

/// `/send` is the desktop's own command (the CLI has none), so it is not in [`WIRED`].
pub fn send_command() -> Command {
    Command {
        name: "send".into(),
        aliases: vec![],
        description: "message a session in another workspace or process (no arguments: pick it)"
            .into(),
        argument_hint: "[<ws:workspace/sess:session> <message>]".into(),
        local: true,
    }
}

/// `5m ago`, `3h ago`, `2d ago` (the CLI's wording).
pub fn age(ms: i64) -> String {
    let m = ms.max(0) / 60_000;
    match m {
        0 => "just now".into(),
        1..=59 => format!("{m}m ago"),
        60..=2879 => format!("{}h ago", m / 60),
        _ => format!("{}d ago", m / 1440),
    }
}

/// A sub-agent name as shown: control characters removed, at most 40 chars.
pub fn label(name: &str) -> String {
    clip(&sanitize_for_display(name), 40)
}

/// `s` on one line, at most `n` chars.
pub fn clip(s: &str, n: usize) -> String {
    let one = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if one.chars().count() <= n {
        return one;
    }
    one.chars()
        .take(n.saturating_sub(1))
        .chain(['\u{2026}'])
        .collect()
}

/// The window note without its "Set it: ..." hint (`/context` with no argument says how in its own line).
fn window_short(source: &str) -> &str {
    source.split_once(". Set it:").map_or(source, |(a, _)| a)
}

/// How a notice reaches the chat (docs/CLI-APP.md "Events"). `Warning`: an error or something the person must
/// act on, never hidden. `Result`: the outcome of a command or action the person asked for, never hidden.
/// `Bookkeeping`: a success the window already shows elsewhere (a queue row, a status bar, a dialog the person
/// just answered); an internal row, shown only with `verbose` (`/verbose`, `synt.tui.verbose`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NoticeKind {
    Warning,
    Result,
    Bookkeeping,
}

/// A stored session (or the empty welcome of a `new ...` row) shown read-only in the chat while the engine
/// stays on its own session: selecting a row starts nothing.
#[derive(Clone)]
pub struct Preview {
    /// The session id; empty for the `new workspace` / `new session` rows.
    pub sid: String,
    pub convo: std::sync::Arc<synthesis_view::convo::Convo>,
}

/// The chat's own state beyond the events.
pub struct Live {
    pub view: ViewState,
    /// What the chat shows instead of `view.convo` (see [`Preview`]); `attach` drops it.
    pub preview: Option<Preview>,
    /// The Workspace selection is on its `new workspace` row (Running: `run_sel` past the last row).
    pub ws_new: bool,
    pub engine: Option<Arc<Engine>>,
    /// The engine is being built (first start or a relaunch).
    pub starting: bool,
    /// The engine being built runs in a WSL distro (the line says "starting distro").
    pub starting_distro: bool,
    /// A task is running (from `send` to the engine's `TaskDone`).
    pub busy: bool,
    /// A `/rewind` is open or writing: no prompt starts (files must not change under a task).
    pub rewinding: bool,
    /// The background job of the current `/rewind` step, for its timeout (`rewind_job_start`).
    pub rewind_job: Option<u64>,
    pub rewind_seq: u64,
    /// A `/clean` is open or deleting: no prompt starts and the engine is not switched.
    pub cleaning: bool,
    pub clean_job: Option<u64>,
    /// The deletion itself (not the counting) is running: a timeout never clears `cleaning`.
    pub clean_deleting: bool,
    pub clean_seq: u64,
    /// The plans the open scope picker shows (the confirmation takes its ids from here).
    pub clean_plans: Vec<(synthesis_session::CleanScope, synthesis_session::CleanPlan)>,
    /// `/compact` between tasks is running (until `UiMsg::Compacted`).
    pub compacting: bool,
    pub dialogs: Vec<Dialog>,
    pub queue: VecDeque<String>,
    /// Who a line sent now goes to while a task runs: `main` or a sub-agent instance (Ctrl+T).
    pub target: String,
    /// Instance names of the sub-agents that can be addressed (from `subagent.*` events).
    pub agents: Vec<String>,
    /// Lines this window handed to `Engine::send_now` that no `message.sent` has answered yet: (to, text).
    pub injected: Vec<(String, String)>,
    /// Dropped main lines put back at the front of the queue by the current burst of drops.
    requeued: usize,
    /// Sent-now lines the harness accepted and has not delivered yet: (message id, to, text).
    pub pending: Vec<(String, String, String)>,
    /// The next `user.message` of main is a sent-now line being delivered.
    mark_delivered: bool,
    pub show_thinking: bool,
    /// Internal rows (`docs/CLI-APP.md` Events) show up in the chat.
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
    /// Rows of the peers listing (polled).
    pub peer_rows: Vec<Value>,
    /// When each listed peer was last active, as the polls saw it.
    peer_seen: Seen,
    /// The open `/send` picker's rows and filter.
    pub send_pick: Option<SendPick>,
    /// The chip over the prompt: the next line goes to this peer, once.
    pub send_to: Option<SendTo>,
    pub commands: Vec<Command>,
    pub sid: String,
    /// The title `/rename` stored for this session id (before the lists have it).
    pub renamed: Option<(String, String)>,
    pub cwd: String,
    pub trusted: bool,
    /// The root the open Trust dialog offers (what "Trust for this session" grants).
    pub trust_root: Option<std::path::PathBuf>,
    /// The parent folder the open Trust dialog also offers (all its subfolders, remembered).
    pub trust_parent: Option<std::path::PathBuf>,
    /// The remembered root the open revoke confirmation is about.
    pub revoke_root: Option<String>,
    /// Remote mode: the engine's trust state as last seen (`/trust revoke` resolves numbers against it).
    pub trust_cache: Option<crate::remote::TrustState>,
    /// The root of the session-only trust in force (set by the window after each engine start).
    pub session_grant: Option<String>,
    /// Images for the next message.
    pub images: Vec<Attached>,
    /// A clipboard read is in flight (key repeat starts no second one).
    pub image_busy: bool,
    /// `/focus`: only the prompts, answers, results and notices show (the CLI's focus view).
    pub focus_view: bool,
    /// The Usage & Metrics period (7 or 30 days).
    pub usage_days: usize,
    /// The `#note` the open "Remember this note?" dialog is about.
    pending_note: Option<String>,
    /// The open `/close-workspace` confirmation, for the checks at confirm time.
    pub(crate) close_asked: Option<crate::close_ws::Asked>,
    /// The open model picker's rows, filter and refresh state.
    pub model_pick: Option<ModelPick>,
    /// The stored model was not available: the first prompt opens the model picker.
    pub stale_model: Option<String>,
    /// The first prompt, kept while that picker is open: it is sent only by picking a model there.
    pub held_prompt: Option<String>,
    /// A prompt handed back unsent (Esc in that picker, a failed switch): the window puts it in the input box.
    pub returned_prompt: Option<String>,
    /// Notes shown once the next engine is attached (the conversation is rebuilt on a restart).
    pub after_attach: Vec<String>,
    /// What the open memory confirmation will do if the person says yes.
    pub mem_pending: Option<(crate::memory_ui::Pending, String)>,
}

/// State behind the `/model` picker; the dialog's options are rebuilt from it.
#[derive(Clone, Debug, Default)]
pub struct ModelPick {
    pub rows: Vec<ModelRow>,
    pub filter: String,
    /// A refresh runs in the background.
    pub refreshing: bool,
    /// The last refresh's failures, one line per provider.
    pub errors: Vec<String>,
}

impl Live {
    pub fn new() -> Self {
        Self {
            view: ViewState::default(),
            preview: None,
            ws_new: false,
            engine: None,
            starting: true,
            starting_distro: false,
            busy: false,
            rewinding: false,
            rewind_job: None,
            rewind_seq: 0,
            cleaning: false,
            clean_job: None,
            clean_deleting: false,
            clean_seq: 0,
            clean_plans: vec![],
            compacting: false,
            dialogs: vec![],
            queue: VecDeque::new(),
            target: "main".into(),
            agents: vec![],
            pending: vec![],
            injected: vec![],
            requeued: 0,
            mark_delivered: false,
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
            peer_rows: vec![],
            peer_seen: Seen::default(),
            send_pick: None,
            send_to: None,
            commands: vec![],
            sid: String::new(),
            renamed: None,
            cwd: String::new(),
            trusted: false,
            trust_root: None,
            trust_parent: None,
            revoke_root: None,
            trust_cache: None,
            session_grant: None,
            images: vec![],
            image_busy: false,
            focus_view: false,
            usage_days: 7,
            pending_note: None,
            close_asked: None,
            model_pick: None,
            stale_model: None,
            held_prompt: None,
            returned_prompt: None,
            after_attach: vec![],
            mem_pending: None,
        }
    }

    /// The conversation the chat draws: the preview when one is shown, else the open session's.
    pub fn convo(&self) -> &synthesis_view::convo::Convo {
        self.preview.as_ref().map_or(&self.view.convo, |p| &p.convo)
    }

    /// Takes over a started engine: its identity, commands and notices. `replay`: the stored
    /// conversation of a resumed session.
    pub fn attach(&mut self, engine: Arc<Engine>, replay: Option<ViewState>) {
        self.preview = None;
        match replay {
            // the stored conversation and panels; the store panels load separately
            Some(v) => {
                self.view.convo = v.convo;
                self.view.session = v.session;
                self.view.status = v.status;
            }
            None => {
                self.view.convo = Default::default();
                self.view.session = synthesis_view::model::Session::new(&engine.sid);
                self.view.status = Default::default();
            }
        }
        if let Some(w) = engine.context_window {
            self.view.status.context.set_window(w);
        }
        for n in &engine.notices {
            // a restart (model, trust, /context) keeps the conversation: a start-up note it already shows is not repeated
            let shown = |i: &Item| matches!(i, Item::Notice { text, .. } if text == n);
            if !self.view.convo.items.iter().any(shown) {
                self.note(n.clone(), false);
            }
        }
        for n in std::mem::take(&mut self.after_attach) {
            self.note(n, false);
        }
        self.stale_model = engine.stale_model.clone();
        self.model_pick = None;
        self.send_pick = None;
        self.send_to = None;
        self.sid = engine.sid.clone();
        self.cwd = engine.cwd.clone();
        self.trusted = engine.trusted;
        self.commands = engine.commands.clone();
        self.commands.push(panels_command());
        self.commands.push(trust_command());
        self.commands.push(crate::doctor_ui::doctor_command());
        self.mcp = engine.mcp_status();
        self.queue.clear();
        self.dialogs.clear();
        self.rewinding = false;
        self.cleaning = self.clean_deleting;
        self.clean_plans.clear();
        // the prompt that waited for a model choice goes out on the model that was picked; if the
        // switch failed (the stored model is still unavailable) it goes back to the input box
        if let Some(t) = self.held_prompt.take() {
            if engine.stale_model.is_none() {
                self.queue.push_back(t);
            } else {
                self.returned_prompt = Some(t);
            }
        }
        self.busy = false;
        self.starting = false;
        self.starting_distro = false;
        self.engine = Some(engine);
    }

    /// `/clear`: the conversation and its meters start over in the same session (the model,
    /// mode and folder stay).
    pub fn clear_conversation(&mut self) {
        let old = std::mem::take(&mut self.view.status);
        self.view.status = synthesis_view::status::StatusModel {
            model: old.model.clone(),
            effort: old.effort,
            mode: old.mode,
            cwd: old.cwd.clone(),
            started_ms: old.started_ms,
            ..Default::default()
        };
        let sid = self
            .engine
            .as_ref()
            .map_or(self.sid.clone(), |e| e.sid.clone());
        let prev = std::mem::replace(
            &mut self.view.session,
            synthesis_view::model::Session::new(&sid),
        );
        self.view.session.cwd = prev.cwd;
        self.view.session.project = prev.project;
        self.view.session.model = prev.model;
        self.view.convo = Default::default();
        if let Some(w) = self.engine.as_ref().and_then(|e| e.context_window) {
            self.view.status.context.set_window(w);
        }
        self.images.clear();
        self.note("new conversation", false);
    }

    /// A line in the chat that is not from the model: a warning (`true`) or the result of something the
    /// person asked for. Both are always shown; see [`NoticeKind`] for what is not.
    pub fn note(&mut self, text: impl Into<String>, warning: bool) {
        let kind = if warning {
            NoticeKind::Warning
        } else {
            NoticeKind::Result
        };
        self.note_kind(kind, text);
    }

    /// The one place a chat notice becomes a row.
    pub fn note_kind(&mut self, kind: NoticeKind, text: impl Into<String>) {
        let text = text.into();
        self.view.convo.items.push(match kind {
            NoticeKind::Warning => Item::Notice {
                warning: true,
                text,
            },
            NoticeKind::Result => Item::Notice {
                warning: false,
                text,
            },
            NoticeKind::Bookkeeping => Item::Internal(text),
        });
    }

    /// Applies what the engine sent. Returns true when a task just ended (the caller refreshes the
    /// store panels and sends the next queued line).
    pub fn on_msg(&mut self, m: UiMsg, now: i64) -> bool {
        match m {
            UiMsg::Event(e) => {
                self.view.apply(&e);
                self.track_messages(&e);
                // a start-up note the harness also emits as an event shows once
                let items = &mut self.view.convo.items;
                if let Some(Item::Notice { text, .. }) = items.last()
                    && items[..items.len() - 1]
                        .iter()
                        .any(|i| matches!(i, Item::Notice { text: t, .. } if t == text))
                {
                    items.pop();
                }
            }
            UiMsg::BadEvent(t) => self.note(t, true),
            UiMsg::SendFailed { to, text, why } => {
                if let Some(i) = self.injected.iter().position(|p| p.0 == to && p.1 == text) {
                    self.injected.remove(i);
                }
                if to == "main" {
                    self.note(format!("not delivered ({why}): queued instead"), true);
                    self.requeue_main(text);
                } else {
                    self.note(
                        format!(
                            "not delivered to @{} ({why}): {}",
                            label(&to),
                            clip(&text, 80)
                        ),
                        true,
                    );
                }
            }
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
            UiMsg::CompactQueued => {
                self.compacting = false;
                self.note(
                    "compaction requested: it runs before the next model request",
                    false,
                );
            }
            UiMsg::Note(t, err) => self.note(t, err),
            UiMsg::Doctor(r) => self.doctor_done(r),
            UiMsg::Effort(Ok(e)) => {
                self.note(format!("effort: {}", e.as_deref().unwrap_or("auto")), false);
                self.view.status.effort = e;
            }
            UiMsg::Effort(Err(why)) => self.note(format!("/effort: {why}"), true),
            // an answer for a session that is no longer the open one is not applied to this one
            UiMsg::Renamed(sid, _) if sid != self.sid => {}
            UiMsg::Renamed(_, r) => match r {
                Ok(t) => {
                    self.view.workspaces.set_title(&self.sid, &t);
                    self.renamed = Some((self.sid.clone(), t.clone()));
                    self.note(
                        format!("Session renamed: {}", sanitize_for_display(&t)),
                        false,
                    );
                }
                Err(why) => self.note(why, true),
            },
            UiMsg::TrustList(r) => match r {
                Ok(st) => {
                    self.trust_cache = Some(st.clone());
                    self.trust_cmd_with("", Some(st));
                }
                Err(why) => self.note(format!("/trust: cannot ask the engine ({why})"), true),
            },
            UiMsg::Compacted(r) => {
                self.compacting = false;
                match r {
                    Ok(()) => self.note("conversation compacted", false),
                    Err(e) => self.note(format!("/compact: {e}"), true),
                }
            }
            UiMsg::TaskDone(r) => {
                self.busy = false;
                self.target = "main".into();
                self.pending.clear();
                self.injected.clear();
                self.agents.clear();
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

    pub(crate) fn push_dialog(&mut self, d: Dialog, now: i64) {
        // an approval, plan or question never waits behind a model or send picker: it goes in front
        // (the picker is armed again when it is answered)
        let front_picker = matches!(
            self.dialogs.first().map(|f| &f.kind),
            Some(DialogKind::Picker {
                kind: PickerKind::Model | PickerKind::Send,
                ..
            })
        );
        if front_picker && !matches!(d.kind, DialogKind::Picker { .. }) {
            self.dialogs.insert(0, d);
            return self.arm(now);
        }
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
                engaged: false,
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
                engaged: false,
            },
            NeedsUser::AskUserQuestion { questions } => {
                let n = questions.len();
                let picked = vec![vec![]; n];
                let mut d = Dialog {
                    options: vec![],
                    kind: DialogKind::Question {
                        id,
                        questions,
                        cur: 0,
                        picked,
                        other: vec![None; n],
                        typing: None,
                    },
                    sel: 0,
                    armed_until: 0,
                    engaged: false,
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
            d.engaged = true;
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
        // the clean confirmation: Esc is Cancel (its last row is the destructive one)
        if matches!(
            self.dialogs.first().map(|d| &d.kind),
            Some(DialogKind::Clean(_))
        ) {
            self.clean_dismissed();
            self.dialogs.remove(0);
            return None;
        }
        if let Some(Dialog {
            kind: DialogKind::Picker { .. },
            ..
        }) = self.dialogs.first()
        {
            if matches!(
                self.dialogs.first().map(|d| &d.kind),
                Some(DialogKind::Picker {
                    kind: PickerKind::Clean,
                    ..
                })
            ) {
                self.clean_dismissed();
            }
            if let Some(d) = self.dialogs.first()
                && matches!(
                    d.kind,
                    DialogKind::Picker {
                        kind: PickerKind::Rewind,
                        ..
                    }
                )
            {
                self.rewind_dismissed();
            }
            self.dialogs.remove(0);
            self.pending_note = None;
            self.mem_pending = None;
            // Esc: nothing is sent; the prompt returns to the input box
            if self.model_pick.is_some() {
                self.returned_prompt = self.held_prompt.take();
            }
            self.close_model_pick();
            self.send_pick = None;
            return None;
        }
        if let Some(Dialog {
            kind: DialogKind::Question { typing, .. },
            ..
        }) = self.dialogs.first_mut()
            && typing.take().is_some()
        {
            return None;
        }
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
            other,
            typing,
            ..
        } = &mut d.kind
        {
            match key.as_str() {
                "no" => {}
                "other" => {
                    *typing = Some(other[*cur].clone().unwrap_or_default());
                    return None;
                }
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
                    if !multi {
                        other[*cur] = None;
                    }
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
    pub fn answer(&mut self, key: &str, now: i64) -> Option<Action> {
        if self.dialogs.is_empty() {
            return None;
        }
        // "Set the context window": the picker closes and the input box gets `/context `
        if key == "context"
            && matches!(
                self.dialogs[0].kind,
                DialogKind::Picker {
                    kind: PickerKind::Model,
                    ..
                }
            )
        {
            self.dialogs.remove(0);
            self.close_model_pick();
            self.returned_prompt = Some("/context ".into());
            self.context_info();
            return None;
        }
        // the model picker's Refresh and error rows act without closing it
        if matches!(
            self.dialogs[0].kind,
            DialogKind::Picker {
                kind: PickerKind::Model,
                ..
            }
        ) && matches!(key, "refresh" | "info")
        {
            return (key == "refresh" && self.start_refresh()).then_some(Action::RefreshModels);
        }
        // the clean confirmation's backup toggle and a not-yet-enabled delete leave the dialog open
        if self.clean_stays(key) {
            return None;
        }
        let d = self.dialogs.remove(0);
        // the next dialog waits out its arming delay whichever branch below answers this one
        self.arm(now);
        if matches!(
            d.kind,
            DialogKind::Clean(_)
                | DialogKind::Picker {
                    kind: PickerKind::Clean,
                    ..
                }
        ) {
            return self.clean_answer(d, key);
        }
        if matches!(
            d.kind,
            DialogKind::Rewind(_)
                | DialogKind::Picker {
                    kind: PickerKind::Rewind,
                    ..
                }
        ) {
            return self.rewind_answer(d, key);
        }
        if let DialogKind::Picker { kind, .. } = d.kind {
            if kind == PickerKind::Bypass {
                if let (Some(e), "enable") = (&self.engine, key)
                    && let Err(why) = e.enable_bypass()
                {
                    self.note(format!("bypass permissions not enabled: {why}"), true);
                }
                return None;
            }
            if kind == PickerKind::Trust {
                let (root, parent) = (self.trust_root.take(), self.trust_parent.take());
                let root = root?;
                let remember = match key {
                    "remember" => Some((root.clone(), synthesis_session::trust::FOLDER)),
                    "parent" => Some((parent?, synthesis_session::trust::PARENT)),
                    "session" => None,
                    _ => return None,
                };
                return Some(Action::TrustFolder(TrustGrant { root, remember }));
            }
            if kind == PickerKind::Revoke {
                if let (Some(e), Some(root), "revoke") =
                    (&self.engine, self.revoke_root.take(), key)
                {
                    if e.is_remote() {
                        // in the background: the engine's store call can take a while
                        let root2 = root.clone();
                        e.spawn_remote(move |r| {
                            Some(UiMsg::Note(
                                revoke_message(&root2, r.trust_revoke(&root2)),
                                false,
                            ))
                        });
                        self.trust_cache = None;
                        if self.session_grant.as_deref() == Some(root.as_str()) {
                            self.session_grant = None;
                        }
                        return Some(Action::TrustRevoked(root));
                    }
                    let done = synthesis_session::trust::revoke(&e.synthesis_home, &root)
                        .map_err(|e| e.to_string());
                    let msg = revoke_message(&root, done);
                    self.note(msg, false);
                    if self.session_grant.as_deref() == Some(root.as_str()) {
                        self.session_grant = None;
                    }
                    return Some(Action::TrustRevoked(root));
                }
                return None;
            }
            if matches!(kind, PickerKind::MemReview | PickerKind::MemConfirm) {
                self.memory_answer(kind, key, now);
                return None;
            }
            if kind == PickerKind::Remember {
                // a `#` note is a file of the machine the instruction files live on: the engine writes it there,
                // this side never does
                if let Some(e) = self.engine.clone().filter(|e| e.is_remote()) {
                    if let (Some(text), "user" | "project") = (self.pending_note.take(), key) {
                        let user = key == "user";
                        e.spawn_remote(move |r| {
                            Some(match r.memory_note(&text, user) {
                                Ok(done) => UiMsg::Note(done, false),
                                Err(why) => UiMsg::Note(format!("# note not saved: {why}"), true),
                            })
                        });
                    }
                    return None;
                }
                if let (Some(e), Some(text), "user" | "project") =
                    (&self.engine, self.pending_note.take(), key)
                {
                    match e.notes.save(key == "user", &text) {
                        Ok(done) => self.note(done, false),
                        Err(err) => self.note(format!("# note not saved: {err}"), true),
                    }
                }
                return None;
            }
            if kind == PickerKind::Send {
                // the chip holds the id address of the row; "info" rows pick nothing
                let pick = self.send_pick.take();
                if let Some(r) = pick.and_then(|p| p.rows.into_iter().find(|r| r.address == key)) {
                    self.send_to = Some(SendTo {
                        label: r.chip(),
                        address: r.address,
                    });
                }
                return None;
            }
            if kind == PickerKind::Model {
                self.close_model_pick();
            }
            let current = self.engine.as_ref().map(|e| e.label.as_str());
            return match kind {
                PickerKind::Model if current == Some(key) => {
                    // a deliberate pick of the model in use: the held prompt goes to it
                    self.queue.extend(self.held_prompt.take());
                    self.note_kind(NoticeKind::Bookkeeping, format!("model: already {key}"));
                    None
                }
                PickerKind::Model => Some(Action::Model(key.to_string())),
                PickerKind::Resume => Some(Action::Resume(key.to_string())),
                PickerKind::CloseWorkspace => key
                    .strip_prefix("close:")
                    .and_then(|k| self.confirm_close_workspace(k, now)),
                PickerKind::Remember
                | PickerKind::Bypass
                | PickerKind::Rewind
                | PickerKind::Clean
                | PickerKind::Send
                | PickerKind::MemReview
                | PickerKind::MemConfirm
                | PickerKind::Trust
                | PickerKind::Revoke => None,
                PickerKind::Setup => Some(match key {
                    "recheck" => Action::Recheck,
                    "settings" => Action::OpenSettings,
                    _ => Action::Quit,
                }),
            };
        }
        if let Some(e) = &self.engine {
            match d.kind {
                DialogKind::Picker { .. } | DialogKind::Rewind(_) | DialogKind::Clean(_) => {}
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
                    other,
                    ..
                } => {
                    let mut answers = serde_json::Map::new();
                    for ((q, p), o) in questions.iter().zip(&picked).zip(&other) {
                        let text = if key == "no" {
                            "The user declined to answer.".to_string()
                        } else {
                            p.iter()
                                .filter_map(|k| q.options.get(*k).map(|o| o.label.clone()))
                                .chain(o.iter().cloned())
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
        None
    }

    /// The front dialog is a question whose "Other" text is being typed.
    pub fn typing_other(&self) -> bool {
        matches!(
            self.dialogs.first(),
            Some(Dialog {
                kind: DialogKind::Question {
                    typing: Some(_),
                    ..
                },
                ..
            })
        )
    }

    /// A character typed into the "Other" answer of the front question (false: not typing).
    pub fn type_other(&mut self, c: &str) -> bool {
        match self.dialogs.first_mut() {
            Some(Dialog {
                kind:
                    DialogKind::Question {
                        typing: Some(t), ..
                    },
                ..
            }) => {
                t.push_str(c);
                true
            }
            _ => false,
        }
    }

    /// Backspace in the "Other" answer.
    pub fn backspace_other(&mut self) {
        if let Some(Dialog {
            kind: DialogKind::Question {
                typing: Some(t), ..
            },
            ..
        }) = self.dialogs.first_mut()
        {
            t.pop();
        }
    }

    /// Enter in the "Other" answer: keep the text (empty drops it) and go back to the choices.
    pub fn commit_other(&mut self) {
        let Some(d) = self.dialogs.first_mut() else {
            return;
        };
        if let DialogKind::Question {
            cur,
            picked,
            other,
            typing,
            questions,
            ..
        } = &mut d.kind
            && let Some(t) = typing.take()
        {
            let t = t.trim().to_string();
            if !t.is_empty() && !questions[*cur].multi_select {
                picked[*cur].clear();
            }
            other[*cur] = (!t.is_empty()).then_some(t);
            d.options = question_options_of(&d.kind);
        }
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
        // Esc spends the chip first; a second one cancels the task
        if self.send_to.take().is_some() {
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

    /// Classifies what the person typed (same rules as the CLI's prompt) and adds it to the
    /// history; the host sends `Submitted::Send`.
    pub fn submit(&mut self, text: &str) -> Submitted {
        self.submit_as(text, false)
    }

    /// [`Live::submit`]; `now` (Cmd/Ctrl+Enter) delivers the line to the running task instead of
    /// queueing it. A sub-agent target (Ctrl+T) always delivers: the queue is for main only.
    pub fn submit_as(&mut self, text: &str, now: bool) -> Submitted {
        let text = text.trim_end();
        if text.trim().is_empty() {
            return Submitted::Nothing;
        }
        if self.history.last().map(String::as_str) != Some(text) {
            self.history.push(text.to_string());
        }
        if let Some(rest) = text.strip_prefix('/') {
            let typed = rest.split_whitespace().next().unwrap_or("");
            // the CLI's two aliases
            let name = match typed {
                "quit" => "exit",
                "perms" => "permissions",
                n => n,
            };
            match self.commands.iter().find(|c| c.name == name) {
                // skills and MCP prompts are for the engine
                Some(c) if !c.local => {}
                Some(_) => {
                    let arg = rest.trim_start()[typed.len()..].trim();
                    return self.command(name, arg);
                }
                None => {
                    self.note(format!("unknown command /{name}"), true);
                    return Submitted::Nothing;
                }
            }
        } else if let Some(note) = text.strip_prefix('#') {
            self.ask_where_to_remember(note.trim());
            return Submitted::Nothing;
        } else if text.starts_with('!') && text.trim_start_matches('!').trim().is_empty() {
            return Submitted::Nothing; // `!` with no command
        }
        if !is_command(text)
            && let Some(to) = self.send_to.take()
        {
            // one-shot: the chip is spent whether the line goes or not
            if !send_pick::listed(&self.peer_rows, &to.address) {
                self.note(format!("{} is gone, nothing sent", to.label), true);
                return Submitted::Held;
            }
            if !self.images.is_empty() {
                self.note("images are not sent to another session", true);
            }
            return Submitted::Do(Action::Send {
                to: to.address,
                text: text.into(),
            });
        }
        if self.target != "main" && !self.agents.contains(&self.target) {
            let gone = label(&self.target);
            self.target = "main".into();
            self.note(format!("@{gone} is gone, nothing sent"), true);
            return Submitted::Held;
        }
        if self.busy && self.engine.is_some() && (now || self.target != "main") {
            match self.inject(text) {
                Ok(s) => return s,
                Err(why) if self.target != "main" => {
                    self.note(why, true);
                    return Submitted::Held;
                }
                Err(why) => {
                    self.note_kind(NoticeKind::Bookkeeping, format!("{why}: queued instead"))
                }
            }
        }
        if self.busy
            || self.rewinding
            || self.cleaning
            || self.engine.is_none()
            || !self.dialogs.is_empty()
        {
            // the CLI queues a line typed while a task runs; it starts when that one ends
            self.queue.push_back(text.to_string());
            return Submitted::Nothing;
        }
        if let Some(stored) = self.stale_model.take() {
            self.held_prompt = Some(text.to_string());
            let using = self.engine.as_ref().map_or("?", |e| e.label.as_str());
            let msg = format!(
                "the stored model {} is not available. Your prompt is held, not sent: pick a model to send it (choosing {using} sends it there); Esc returns it to the input box",
                sanitize_for_display(&stored)
            );
            self.note(msg, true);
            self.open_model_picker();
            return Submitted::Nothing;
        }
        self.busy = true;
        Submitted::Send(text.to_string())
    }

    /// The line as a send-now delivery to the target, or why it cannot be one. Commands (`/`, `!`)
    /// are never delivered; an open dialog holds delivery (the harness drains only after the answer).
    fn inject(&mut self, text: &str) -> Result<Submitted, &'static str> {
        if is_command(text) {
            return Err("commands are not sent now");
        }
        if !self.dialogs.is_empty() {
            return Err("an approval is open and holds delivery");
        }
        if !self.images.is_empty() {
            return Err("images go with a started line");
        }
        self.injected.push((self.target.clone(), text.to_string()));
        Ok(Submitted::Inject {
            to: self.target.clone(),
            text: text.to_string(),
        })
    }

    /// "Send now" on queued row `i`. The row stays queued when it cannot be delivered.
    pub fn send_queued_now(&mut self, i: usize) -> Submitted {
        if !self.busy || self.engine.is_none() {
            return Submitted::Nothing;
        }
        let Some(t) = self.queue.get(i).cloned() else {
            return Submitted::Nothing;
        };
        match self.inject(&t) {
            Ok(s) => {
                self.queue.remove(i);
                s
            }
            Err(why) => {
                self.note(format!("{why}: it stays queued"), false);
                Submitted::Nothing
            }
        }
    }

    /// Ctrl+T: main, then each sub-agent in turn.
    pub fn cycle_target(&mut self) {
        if self.send_to.take().is_some() {
            return; // the chip's peer is one more stop: from there back to main
        }
        if !self.busy {
            return; // the target applies while a task runs
        }
        let mut order = vec!["main".to_string()];
        order.extend(self.agents.iter().cloned());
        let i = order
            .iter()
            .position(|n| *n == self.target)
            .map_or(0, |i| i + 1);
        self.target = order[i % order.len()].clone();
    }

    /// A main line that was not delivered goes back to the front of the queue; a burst keeps its order.
    fn requeue_main(&mut self, text: String) {
        self.queue.insert(self.requeued.min(self.queue.len()), text);
        self.requeued += 1;
    }

    /// Sent-now bookkeeping off the event stream (UI-only until `message.delivered`), and the
    /// sub-agent names a line can be addressed to.
    fn track_messages(&mut self, e: &synthesis_events::Envelope) {
        use synthesis_events::Event;
        if !matches!(e.event, Event::MessageDropped { .. }) {
            self.requeued = 0;
        }
        match &e.event {
            Event::SubagentStarted(i) => {
                if let Some(n) = &i.name
                    && !self.agents.contains(n)
                {
                    self.agents.push(n.clone());
                }
            }
            Event::SubagentStatusChanged { name, to, .. } if to == "evicted" => {
                self.agents.retain(|n| n != name);
            }
            Event::MessageSent {
                message_id,
                to,
                origin,
                text,
                ..
            } if origin == "user" => {
                // only a line this window sent (a sent-now that is still wanted) counts
                if let Some(i) = self
                    .injected
                    .iter()
                    .position(|p| p.0 == *to && p.1 == *text)
                    .or_else(|| {
                        self.injected
                            .iter()
                            .position(|p| p.0 == *to && text.starts_with(&p.1))
                    })
                {
                    self.injected.remove(i);
                    self.pending
                        .push((message_id.clone(), to.clone(), text.clone()));
                }
            }
            Event::MessageDelivered { message_id, .. } => {
                if let Some(i) = self.pending.iter().position(|p| p.0 == *message_id) {
                    let (_, to, text) = self.pending.remove(i);
                    if to == "main" {
                        self.mark_delivered = true; // the user row follows this event
                    } else {
                        self.note_kind(
                            NoticeKind::Bookkeeping,
                            format!(
                                "sent now to @{}, delivered at its next step: {}",
                                label(&to),
                                clip(&text, 80)
                            ),
                        );
                    }
                }
            }
            Event::MessageDropped { message_id, .. } => {
                if let Some(i) = self.pending.iter().position(|p| p.0 == *message_id) {
                    let (_, to, text) = self.pending.remove(i);
                    if to == "main" {
                        self.requeue_main(text);
                    } else {
                        self.note(
                            format!("not delivered to @{}: {}", label(&to), clip(&text, 80)),
                            true,
                        );
                    }
                }
            }
            Event::UserMessage { .. } if std::mem::take(&mut self.mark_delivered) => {
                self.note_kind(
                    NoticeKind::Bookkeeping,
                    "sent now, delivered at the next step",
                );
            }
            _ => {}
        }
    }

    /// The setup card (`apps/cli/src/tui.rs` `first_run`) as a dialog: re-check, Settings, quit.
    /// No key is asked for here; Settings shows which providers have one.
    pub fn open_setup(&mut self, why: &str, now: i64) {
        self.push_dialog(
            Dialog {
                kind: DialogKind::Picker {
                    kind: PickerKind::Setup,
                    title: "Synthesis needs a model and an API key before it can run a task".into(),
                    headers: vec![(
                        0,
                        format!(
                            "why: {}\nKeep the key in an environment variable you manage (never in a repository); Settings shows each provider's key status.",
                            sanitize_for_display(why)
                        ),
                    )],
                },
                options: vec![
                    ("recheck".into(), "Check again (after setting a model and a key)".into()),
                    ("settings".into(), "Open Settings: provider key status".into()),
                    ("quit".into(), "Quit".into()),
                ],
                sel: 0,
                armed_until: 0,
                engaged: false,
            },
            now,
        );
    }

    /// `#note`: nothing is saved before the person picks the file (the CLI's confirmation).
    fn ask_where_to_remember(&mut self, note: &str) {
        if note.is_empty() {
            return;
        }
        let Some(e) = self.engine.clone() else {
            return self.note("# note: the session is still starting, try again", true);
        };
        if self.dialogs.iter().any(|d| {
            matches!(
                &d.kind,
                DialogKind::Picker {
                    kind: PickerKind::Remember,
                    ..
                }
            )
        }) {
            return self.note("# note: answer the open question first", true);
        }
        self.pending_note = Some(note.to_string());
        let preview = sanitize_for_display(&e.notes.preview(note));
        self.push_dialog(
            Dialog {
                kind: DialogKind::Picker {
                    kind: PickerKind::Remember,
                    title: "Remember this note?".into(),
                    headers: vec![(
                        0,
                        format!("will be saved as: - {preview} (part of the instructions of later sessions)"),
                    )],
                },
                options: vec![
                    ("no".into(), "No, do not save (esc also cancels)".into()),
                    (
                        "project".into(),
                        format!("Save to the project file  {}", e.notes.project.display()),
                    ),
                    (
                        "user".into(),
                        format!("Save to the user file  {}", e.notes.user.display()),
                    ),
                ],
                sel: 0,
                armed_until: 0,
                engaged: false,
            },
            now_ms(),
        );
    }

    /// The next queued line once the task ended and nothing waits for an answer.
    pub fn next_queued(&mut self) -> Option<String> {
        if self.busy
            || self.rewinding
            || self.cleaning
            || self.engine.is_none()
            || !self.dialogs.is_empty()
        {
            return None;
        }
        let t = self.queue.pop_front()?;
        self.busy = true;
        Some(t)
    }

    /// A built-in command (docs/CLI-APP.md "Commands"): what the person sees in the chat, or the
    /// action the window carries out.
    fn command(&mut self, name: &str, arg: &str) -> Submitted {
        let busy_err = |l: &mut Self, what: &str| {
            l.note(
                format!("/{what}: a task is running \u{b7} esc cancels it first"),
                true,
            );
            Submitted::Nothing
        };
        match name {
            "exit" => return Submitted::Quit,
            "help" => self.help(),
            "clear" if self.busy => return busy_err(self, "clear"),
            "clear" => return Submitted::Do(Action::Clear),
            "compact" => {
                return Submitted::Do(Action::Compact((!arg.is_empty()).then(|| arg.into())));
            }
            "config" => return Submitted::Do(Action::OpenSettings),
            "panels" => return Submitted::Do(Action::Panels(arg.into())),
            "new" => return Submitted::Do(Action::New),
            "close" => return Submitted::Do(Action::Close),
            "close-workspace" => self.ask_close_workspace(None, now_ms()),
            "resume" if arg.is_empty() => self.open_resume_picker(now_ms()),
            "resume" => return Submitted::Do(Action::Resume(arg.into())),
            "model" if self.busy => return busy_err(self, "model"),
            "model" if arg.is_empty() => self.open_model_picker(),
            "model" => return Submitted::Do(Action::Model(arg.into())),
            "open" if arg.is_empty() => self.note("/open: type a folder path", true),
            "open" => return Submitted::Do(Action::Open(arg.into())),
            "context" if arg.is_empty() => self.context_info(),
            "context" => return Submitted::Do(Action::SetContext(arg.into())),
            "effort" => self.set_effort(arg),
            "mcp" | "memory" | "agents" | "permissions" | "status" | "name" | "rename"
                if self.engine.is_none() =>
            {
                self.note(format!("/{name}: the session is still starting"), true)
            }
            "mcp" if self.engine.as_ref().is_some_and(|e| e.is_remote()) => {
                if let Some(e) = &self.engine {
                    e.spawn_remote(|r| {
                        Some(match r.mcp_report() {
                            Ok(l) => UiMsg::Note(l.join("\n"), false),
                            Err(why) => UiMsg::Note(format!("/mcp: {why}"), true),
                        })
                    });
                }
            }
            "mcp" => self.info(|e| e.mcp_report()),
            "memory" if !arg.is_empty() => self.memory_cmd(arg, now_ms()),
            "memory" => self.info(|e| e.memory_report.clone()),
            // `--all` (or `-a`): the other sessions too, as the `/peers` table
            "agents" if arg.split_whitespace().any(|a| a == "--all" || a == "-a") => {
                self.info(|e| e.agents_report());
                return self.command("peers", "");
            }
            "agents" => self.info(|e| e.agents_report()),
            "permissions" if arg.is_empty() || arg == "list" => {
                self.info(|e| e.permissions_report())
            }
            "status" => self.status(),
            "doctor" => return self.doctor_cmd(),
            "terminal" => {
                let v = self.calls(|n| n == "Bash", "shell commands");
                self.note(v, false);
            }
            "browser" => {
                let v = self.calls(
                    |n| n.starts_with("browser_") || n == "WebFetch" || n == "WebSearch",
                    "browser and web calls",
                );
                self.note(v, false);
            }
            "images" => self.images_command(arg),
            "verbose" => {
                self.verbose = match arg {
                    "on" => true,
                    "off" => false,
                    _ => !self.verbose,
                };
                self.note(
                    format!("verbose {}", if self.verbose { "on" } else { "off" }),
                    false,
                );
            }
            "permissions" => self.permissions_edit(arg),
            // the file explorer and diff tabs of the centre panel are the views of these two; nothing is posted
            "files" => return Submitted::Do(Action::ShowFiles),
            "reveal" => return Submitted::Do(Action::Reveal(arg.trim().to_string())),
            "diff" => {
                let f = arg.trim();
                return Submitted::Do(Action::ShowDiff((!f.is_empty()).then(|| f.to_string())));
            }
            "peers"
                if self
                    .engine
                    .as_ref()
                    .is_some_and(|e| e.peer_name().is_none()) =>
            {
                self.note(PEERS_OFF, true)
            }
            "peers" => return Submitted::Do(Action::Peers),
            "send" if arg.is_empty() => self.open_send_picker(),
            "send"
                if self
                    .engine
                    .as_ref()
                    .is_some_and(|e| e.peer_name().is_none()) =>
            {
                self.note(PEERS_OFF, true)
            }
            "send" => match arg.trim().split_once(char::is_whitespace) {
                Some((to, text)) if !text.trim().is_empty() => {
                    return Submitted::Do(Action::Send {
                        to: to.into(),
                        text: text.trim().into(),
                    });
                }
                _ => self.note(
                    "usage: /send ws:<workspace>/sess:<session> <message>   (see /peers)",
                    true,
                ),
            },
            "trust" if arg.trim() == "ask" => return Submitted::Do(Action::AskTrust),
            "trust" => self.trust_cmd(arg),
            "rewind" => {
                if let Some(a) = self.rewind_command(arg) {
                    return Submitted::Do(a);
                }
            }
            "clean" => {
                if let Some(a) = self.clean_command(arg) {
                    return Submitted::Do(a);
                }
            }
            "name" => self.name(arg),
            "rename" => self.rename(arg),
            "focus" => {
                self.focus_view = match arg {
                    "on" => true,
                    "off" => false,
                    _ => !self.focus_view,
                };
                self.note(
                    format!("focus view {}", if self.focus_view { "on" } else { "off" }),
                    false,
                );
            }
            _ => self.note(format!("/{name}: {NOT_YET}"), false),
        }
        Submitted::Nothing
    }

    /// `/permissions allow RULE | remove RULE`: this session only, the CLI's check.
    fn permissions_edit(&mut self, arg: &str) {
        let Some(e) = self.engine.clone() else { return };
        let (sub, rule) = arg.split_once(char::is_whitespace).unwrap_or((arg, ""));
        let rule = rule.trim();
        let r = match (sub, rule.is_empty()) {
            ("allow", false) => e.allow_rule(rule),
            ("remove" | "rm", false) => e.remove_rule(rule),
            _ => Err("usage: /permissions [allow RULE | remove RULE]   e.g. /permissions allow Bash(git status:*)".into()),
        };
        match r {
            Ok(t) => self.note(t, false),
            Err(t) => self.note(t, true),
        }
    }

    /// `/name`: shows the name peers address this session by; it is fixed for the run, as in the CLI.
    fn name(&mut self, arg: &str) {
        let Some(shown) = self.engine.as_ref().map(|e| e.peer_name()) else {
            return;
        };
        let Some(shown) = shown else {
            return self.note(format!("/name: {PEERS_OFF}"), true);
        };
        let shown = shown
            .as_deref()
            .map_or("(none)".into(), sanitize_for_display);
        if arg.is_empty() {
            self.note(format!("Session name for peers: {shown}"), false);
        } else {
            self.note(
                format!(
                    "/name: the name is fixed for the run (now {shown}); start with --name {}",
                    sanitize_for_display(arg)
                ),
                true,
            );
        }
    }

    /// `/rename TITLE`: stores the title of this session; the lists show it at once.
    fn rename(&mut self, arg: &str) {
        let Some(e) = self.engine.clone() else { return };
        if arg.is_empty() {
            let shown = match self.title_shown() {
                t if t.is_empty() => "(untitled)".into(),
                t => sanitize_for_display(&t),
            };
            return self.note(
                format!("Session title: {shown}\nusage: /rename <title>"),
                false,
            );
        }
        if e.is_remote() {
            e.rename_remote(arg);
            return self.note_kind(NoticeKind::Bookkeeping, "renaming...");
        }
        match e.rename(arg) {
            Ok(t) => {
                self.view.workspaces.set_title(&self.sid, &t);
                self.renamed = Some((self.sid.clone(), t.clone()));
                self.note(
                    format!("Session renamed: {}", sanitize_for_display(&t)),
                    false,
                );
            }
            Err(why) => self.note(why, true),
        }
    }

    /// The open session's title: the one just set, else the listed one.
    pub fn title_shown(&self) -> String {
        match &self.renamed {
            Some((id, t)) if *id == self.sid => t.clone(),
            _ => self
                .view
                .workspaces
                .workspaces
                .iter()
                .flat_map(|w| &w.sessions)
                .find(|s| s.id == self.sid && s.title != "(untitled)")
                .map(|s| s.title.clone())
                .unwrap_or_default(),
        }
    }

    /// The name of the open session, the same in the Running row and the top bar: its title (set by /rename or
    /// stored), else the first thing the person typed, else "(new session)".
    pub fn session_name(&self) -> String {
        let t = self.title_shown();
        if !t.is_empty() {
            return t;
        }
        self.view
            .convo
            .items
            .iter()
            .find_map(|i| match i {
                Item::User(u) if !u.trim().is_empty() => {
                    Some(u.split_whitespace().collect::<Vec<_>>().join(" "))
                }
                _ => None,
            })
            .unwrap_or_else(|| "(new session)".into())
    }

    /// The `/peers` listing (`None`: peers are off).
    pub fn peers_done(&mut self, rows: Option<Vec<Value>>) {
        match rows {
            Some(r) => self.note(synthesis_cli_ui::peers_view::table(&r).join("\n"), false),
            None => self.note(PEERS_OFF, true),
        }
    }

    /// The outcome of `/send`. The reply itself arrives as the `peer.reply` row; a delivery is echoed with the ids it went to, a failure is said.
    pub fn send_done(&mut self, to: &str, r: Result<String, String>) {
        match r {
            Ok(_) => self.note(send_pick::echo(to), false),
            Err(e) => self.note(format!("/send: {e}"), true),
        }
    }

    fn info(&mut self, f: impl Fn(&Engine) -> Vec<String>) {
        if let Some(e) = &self.engine {
            let text = f(e).join("\n");
            self.note(text, false);
        }
    }

    /// `/status`: session, model, mode, context, verification.
    /// `/context` alone: the window in use and where it comes from, and how to set it.
    fn context_info(&mut self) {
        let Some(e) = &self.engine else { return };
        let msg = format!(
            "context window of {}: {}\nType a size to set it: /context 1m, /context 200k, /context 128000; /context unset removes it (written to your synt.json)",
            e.label,
            sanitize_for_display(window_short(&e.context_source))
        );
        self.note(msg, false);
    }

    fn status(&mut self) {
        let Some(e) = &self.engine else { return };
        let verification = self
            .view
            .convo
            .items
            .iter()
            .rev()
            .find_map(|i| match i {
                Item::Verification { status, summary } => Some(format!("{status}: {summary}")),
                _ => None,
            })
            .unwrap_or_else(|| "none".into());
        let t = self.view.status.context.numbers();
        let lines = [
            format!("session: {}", e.sid),
            format!("folder: {}", sanitize_for_display(&e.cwd)),
            format!("model: {}", e.label),
            format!("mode: {}", e.mode().as_str()),
            format!(
                "context: {} / {} tokens",
                crate::tui_view::snapshot::tok(t.0),
                if t.1 > 0 {
                    crate::tui_view::snapshot::tok(t.1)
                } else {
                    "?".into()
                }
            ),
            format!(
                "context window: {}",
                sanitize_for_display(window_short(&e.context_source))
            ),
            format!("trusted folder: {}", if e.trusted { "yes" } else { "no" }),
            format!("verification: {verification}"),
        ];
        self.note(lines.join("\n"), false);
    }

    /// `/terminal` and `/browser`: the matching tool calls of this conversation.
    fn calls(&self, pick: impl Fn(&str) -> bool, what: &str) -> String {
        let rows: Vec<String> = self
            .view
            .convo
            .items
            .iter()
            .filter_map(|i| match i {
                Item::Tool {
                    name,
                    summary,
                    state,
                    ..
                } if pick(name) => {
                    Some(format!("  {state:?} \u{b7} {name} \u{b7} {summary}").to_lowercase())
                }
                _ => None,
            })
            .collect();
        if rows.is_empty() {
            format!("no {what} in this session yet")
        } else {
            format!("{} ({}):\n{}", what, rows.len(), rows.join("\n"))
        }
    }

    /// `/images [clear]`.
    fn images_command(&mut self, arg: &str) {
        match arg {
            "clear" => {
                let n = std::mem::take(&mut self.images).len();
                self.note(format!("{n} attached image(s) dropped"), false);
            }
            "" if self.images.is_empty() => self.note(
                "no images attached \u{b7} Cmd/Ctrl+V pastes one, or drop a file on the window",
                false,
            ),
            "" => {
                let list: Vec<String> = self
                    .images
                    .iter()
                    .map(|a| format!("{} ({:.1} MB)", a.name, a.bytes as f64 / 1e6))
                    .collect();
                self.note(
                    format!(
                        "attached: {} \u{b7} /images clear drops them",
                        list.join(", ")
                    ),
                    false,
                );
            }
            _ => self.note("usage: /images [clear]", true),
        }
    }

    /// An image for the next message: the CLI's limits (5 images, 6 MiB of base64 together).
    pub fn attach_image(
        &mut self,
        media_type: &'static str,
        bytes: &[u8],
        dims: Option<(u32, u32)>,
    ) -> Result<String, String> {
        if self.images.len() >= MAX_ATTACHMENTS {
            return Err(format!("at most {MAX_ATTACHMENTS} images per message"));
        }
        let base64 = synthesis_cli_ui_base::clipboard::base64(bytes);
        let held: usize = self.images.iter().map(|a| a.base64.len()).sum();
        if held + base64.len() > MAX_ATTACHED_BYTES {
            return Err(format!(
                "image not attached: a message holds at most {} MB of images",
                MAX_ATTACHED_BYTES >> 20
            ));
        }
        let n = self.images.len() + 1;
        let ext = if media_type == "image/png" {
            "png"
        } else {
            "jpg"
        };
        self.images.push(Attached {
            name: format!("image {n}.{ext}"),
            media_type,
            bytes: bytes.len(),
            base64,
        });
        Ok(match dims {
            Some((w, h)) => format!("image {n} attached ({w}x{h})"),
            None => format!("image {n} attached"),
        })
    }

    /// The x on an attachment chip: drops image `i` from the pending message (the file on disk is not touched).
    pub fn remove_image(&mut self, i: usize) {
        if i < self.images.len() {
            self.images.remove(i);
        }
    }

    /// The images of the message being sent, as the harness takes them.
    pub fn take_images(&mut self) -> Vec<synthesis_harness::ImageAttachment> {
        std::mem::take(&mut self.images)
            .into_iter()
            .map(|a| synthesis_harness::ImageAttachment {
                media_type: a.media_type.into(),
                data_base64: a.base64,
            })
            .collect()
    }

    /// `/effort [tier|auto]`; no argument steps through auto, then the model's tiers.
    fn set_effort(&mut self, arg: &str) {
        let Some(e) = self.engine.clone() else { return };
        let tiers = e.effort_tiers.clone();
        if tiers.is_empty() {
            return self.note("/effort: this model declares no effort tiers", true);
        }
        let cur = self.view.status.effort.clone();
        let next = match arg {
            "" => match cur.as_ref().and_then(|c| tiers.iter().position(|t| t == c)) {
                None => tiers.first().cloned(),
                Some(i) => tiers.get(i + 1).cloned(),
            },
            "auto" | "default" => None,
            t if tiers.iter().any(|x| x == t) => Some(t.to_string()),
            _ => return self.note(format!("usage: /effort [auto|{}]", tiers.join("|")), true),
        };
        // the engine's state lives on its own machine: nothing of it is written here, the engine saves it
        if e.is_remote() {
            let tier = next.clone();
            e.spawn_remote(move |r| {
                Some(UiMsg::Effort(r.config_effort(tier.clone()).map(|()| tier)))
            });
            return;
        }
        // kept for the next start too (state.json), like the CLI
        if let Err(err) = synthesis_config::save_effort(&e.synthesis_home, next.as_deref()) {
            self.note(format!("/effort: not saved ({err})"), false);
        }
        e.set_effort(next.clone());
        self.note(
            format!("effort: {}", next.as_deref().unwrap_or("auto")),
            false,
        );
        self.view.status.effort = next;
    }

    /// `/model`: the models of the config (and aliases) with what is known of each, the one in use
    /// marked; typing filters, the first row refreshes the list from the providers.
    fn open_model_picker(&mut self) {
        let Some(e) = &self.engine else { return };
        let mut rows = e.model_rows.clone();
        for m in &e.models {
            if !rows.iter().any(|r| r.value == *m) {
                rows.push(ModelRow {
                    value: m.clone(),
                    detail: crate::models::detail(None, None),
                });
            }
        }
        // the one in use is always there; its window is the provider's live answer
        if !rows.iter().any(|r| r.value == e.label) {
            rows.insert(
                0,
                ModelRow {
                    value: e.label.clone(),
                    detail: crate::models::detail(None, None),
                },
            );
        }
        self.model_pick = Some(ModelPick {
            rows,
            ..Default::default()
        });
        self.dialogs.insert(
            0,
            Dialog {
                kind: DialogKind::Picker {
                    kind: PickerKind::Model,
                    title: format!("Model \u{b7} in use: {}", sanitize_for_display(&e.label)),
                    headers: vec![],
                },
                options: vec![],
                sel: 0,
                armed_until: 0,
                engaged: false,
            },
        );
        self.rebuild_model_dialog(self.held_prompt.is_none());
        // a held prompt is sent only by a deliberate pick: Enter must not land on the fallback model
        if self.held_prompt.is_some()
            && let Some(d) = self.dialogs.first_mut()
        {
            d.sel = 0;
        }
    }

    /// The model picker's options from `model_pick`: Refresh row, error rows, then the models that
    /// match the filter. `reselect`: put the highlight on the model in use (or the first match).
    pub fn rebuild_model_dialog(&mut self, reselect: bool) {
        let (Some(pick), Some(e)) = (&self.model_pick, &self.engine) else {
            return;
        };
        let current = e.label.clone();
        let needle = pick.filter.to_lowercase();
        let shown: Vec<&ModelRow> = pick
            .rows
            .iter()
            .filter(|r| r.value.to_lowercase().contains(&needle))
            .collect();
        let w = shown
            .iter()
            .map(|r| r.value.chars().count())
            .max()
            .unwrap_or(0);
        let mut options: Vec<(String, String)> =
            vec![("refresh".into(), refresh_label(pick, now_ms()))];
        options.extend(
            pick.errors
                .iter()
                .map(|x| ("info".to_string(), format!("! refresh failed - {x}"))),
        );
        if self.held_prompt.is_none() {
            options.push((
                "context".into(),
                "Set the context window of the model in use...".into(),
            ));
        }
        let first = options.len();
        options.extend(shown.iter().map(|r| {
            let mark = if r.value == current { "*" } else { " " };
            (
                r.value.clone(),
                format!(
                    "{mark} {:<w$}{}{}",
                    r.value,
                    if r.detail.is_empty() {
                        String::new()
                    } else {
                        format!("  {}", r.detail)
                    },
                    if mark == "*" { "  (current)" } else { "" }
                ),
            )
        }));
        if shown.is_empty() {
            options.push(("info".into(), "no model matches the filter".into()));
        }
        let header = format!(
            "filter: {}_   (type to filter \u{b7} * marks the one in use \u{b7} a field the provider did not give is left out)",
            pick.filter
        );
        let at = options
            .iter()
            .position(|(k, _)| *k == current)
            .filter(|_| reselect)
            .unwrap_or(first.min(options.len() - 1));
        if let Some(d) = self.dialogs.first_mut()
            && let DialogKind::Picker {
                kind: PickerKind::Model,
                headers,
                ..
            } = &mut d.kind
        {
            *headers = vec![(0, header)];
            d.options = options;
            d.sel = if reselect {
                at
            } else {
                first.min(d.options.len() - 1)
            };
        }
    }

    /// Moves the refresh spinner one frame (only the first row's text; the highlight stays).
    /// True while a refresh runs, so the window keeps repainting.
    pub fn spin_refresh(&mut self, now: i64) -> bool {
        let Some(p) = self.model_pick.as_ref().filter(|p| p.refreshing) else {
            return false;
        };
        let label = refresh_label(p, now);
        if let Some(d) = self.dialogs.first_mut()
            && let Some(o) = d.options.first_mut()
            && o.0 == "refresh"
        {
            o.1 = label;
        }
        true
    }

    fn close_model_pick(&mut self) {
        self.model_pick = None;
    }

    /// A printable key (or Backspace) typed into the filter of the model or send picker. False: not
    /// one of them, the key is the dialog's.
    pub fn model_filter_key(&mut self, text: Option<&str>, backspace: bool) -> bool {
        let send = matches!(
            self.dialogs.first().map(|d| &d.kind),
            Some(DialogKind::Picker {
                kind: PickerKind::Send,
                ..
            })
        );
        let filter = if send {
            self.send_pick.as_mut().map(|p| &mut p.filter)
        } else if matches!(
            self.dialogs.first().map(|d| &d.kind),
            Some(DialogKind::Picker {
                kind: PickerKind::Model,
                ..
            })
        ) {
            self.model_pick.as_mut().map(|p| &mut p.filter)
        } else {
            None
        };
        let Some(filter) = filter else {
            return false;
        };
        if backspace {
            filter.pop();
        } else if let Some(t) = text.filter(|t| !t.chars().any(char::is_control)) {
            filter.push_str(t);
        } else {
            return false;
        }
        if let Some(d) = self.dialogs.first_mut() {
            d.engaged = true;
        }
        if send {
            self.rebuild_send_dialog();
        } else {
            self.rebuild_model_dialog(false);
        }
        true
    }

    /// The polled peer listing, and the activity times read off it. An open `/send` picker follows it.
    pub fn set_peer_rows(&mut self, rows: Vec<Value>, now: i64) {
        self.peer_seen.update(&rows, now);
        self.peer_rows = rows;
        if self.send_pick.is_some() {
            self.rebuild_send_dialog();
        }
    }

    /// `/send` with no arguments: the live peers grouped by workspace; typing filters. When there is
    /// nothing to pick the only row says why.
    fn open_send_picker(&mut self) {
        self.send_pick = Some(SendPick::default());
        // armed like the approvals: an Enter typed before it appeared picks nothing, and Enter picks only
        // once the person engaged it (arrows, Tab, a click, typing in the filter)
        self.push_dialog(
            Dialog {
                kind: DialogKind::Picker {
                    kind: PickerKind::Send,
                    title: "Send to a session \u{b7} then type the message".into(),
                    headers: vec![],
                },
                options: vec![],
                sel: 0,
                armed_until: 0,
                engaged: false,
            },
            now_ms(),
        );
        self.rebuild_send_dialog();
    }

    /// The send picker's options from the listing and the filter; the highlight stays on the same peer.
    fn rebuild_send_dialog(&mut self) {
        let Some(pick) = &mut self.send_pick else {
            return;
        };
        let now = now_ms();
        pick.rows = send_pick::picks(&self.peer_rows, &self.sid, &self.peer_seen);
        let shown = send_pick::visible(&pick.rows, &pick.filter);
        let (mut options, mut headers) = (vec![], vec![]);
        let mut group = "";
        for r in &shown {
            if r.ws() != group {
                group = r.ws();
                // the id comes from the address, after a separator a name cannot hold (the card paints it apart)
                headers.push((
                    options.len(),
                    format!("{}\u{1f}{}", r.workspace, r.ws_short),
                ));
            }
            options.push((r.address.clone(), r.row_label(now)));
        }
        if options.is_empty() {
            let off = self
                .engine
                .as_ref()
                .is_some_and(|e| e.peer_name().is_none());
            let any = !pick.rows.is_empty();
            options.push(("info".into(), send_pick::empty_note(off, any).into()));
        }
        let title = format!(
            "Send to a session \u{b7} filter: {}_   (type to filter, then pick; you type the message after)",
            pick.filter
        );
        if let Some(d) = self.dialogs.iter_mut().find(|d| {
            matches!(
                d.kind,
                DialogKind::Picker {
                    kind: PickerKind::Send,
                    ..
                }
            )
        }) {
            let on = d.options.get(d.sel).map(|o| o.0.clone());
            d.sel = options
                .iter()
                .position(|o| Some(&o.0) == on.as_ref())
                .unwrap_or(0);
            d.options = options;
            if let DialogKind::Picker {
                headers: h,
                title: t,
                ..
            } = &mut d.kind
            {
                (*h, *t) = (headers, title);
            }
        }
    }

    /// The Refresh row: false when one is already running.
    fn start_refresh(&mut self) -> bool {
        let Some(p) = &mut self.model_pick else {
            return false;
        };
        if p.refreshing {
            return false;
        }
        p.refreshing = true;
        p.errors.clear();
        self.rebuild_model_dialog(false);
        true
    }

    /// The refresh ended: the rows as the cache now has them and one line per failed provider.
    pub fn models_refreshed(&mut self, rows: Vec<ModelRow>, errors: Vec<String>) {
        let Some(p) = &mut self.model_pick else {
            return;
        };
        p.refreshing = false;
        if !rows.is_empty() {
            p.rows = rows;
        }
        p.errors = errors;
        self.rebuild_model_dialog(false);
    }

    /// `/resume`: the stored sessions, grouped by workspace; a session without a prompt, the open
    /// one and closed ones are not listed (the workspace list already leaves them out).
    fn open_resume_picker(&mut self, now: i64) {
        let (mut options, mut headers) = (vec![], vec![]);
        for w in &self.view.workspaces.workspaces {
            let first = options.len();
            for s in w
                .sessions
                .iter()
                .filter(|s| s.id != self.sid && s.title != "(untitled)")
            {
                let live = if s.is_live {
                    " \u{b7} open in another window"
                } else {
                    ""
                };
                options.push((
                    s.id.clone(),
                    format!("{} \u{b7} {}{live}", s.title, age(now - s.mtime)),
                ));
            }
            if options.len() > first {
                headers.push((first, format!("{}  {}", w.label, w.root)));
            }
        }
        if options.is_empty() {
            return self.note("/resume: no saved sessions to continue", false);
        }
        self.dialogs.push(Dialog {
            kind: DialogKind::Picker {
                kind: PickerKind::Resume,
                title: "Resume a session".into(),
                headers,
            },
            options,
            sel: 0,
            armed_until: 0,
            engaged: false,
        });
    }

    fn help(&mut self) {
        let mut lines = vec![
            "Enter sends; Shift+Enter adds a line. Esc interrupts a running task (partial output is kept).".to_string(),
            "Up/Down walk the prompt history, Ctrl+R searches it; Tab walks chat, Workspace, Running; Shift+Tab cycles the permission mode.".into(),
            "Ctrl+N opens a folder as a new workspace; Cmd/Ctrl+V pastes text or an image; Ctrl+O shows every tool output whole; Ctrl+C twice quits.".into(),
            "@ mentions a file, / completes a command. Commands:".into(),
        ];
        for c in &self.commands {
            let tag = if c.local && !is_wired(&c.name) {
                format!("  ({NOT_YET})")
            } else {
                String::new()
            };
            let hint = if c.argument_hint.is_empty() {
                String::new()
            } else {
                format!(" {}", c.argument_hint)
            };
            lines.push(format!("  /{}{hint}: {}{tag}", c.name, c.description));
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

    /// Shift+Tab: the CLI's cycle (default, acceptEdits, plan, auto) without its last step:
    /// bypassPermissions is not reachable from the desktop app.
    pub fn cycle_mode(&mut self) {
        let Some(e) = &self.engine else { return };
        let next = match e.mode() {
            Mode::Default => Mode::AcceptEdits,
            Mode::AcceptEdits => Mode::Plan,
            Mode::Plan => Mode::Auto,
            // bypass is reached only through its confirmation dialog; leaving it needs none
            Mode::Auto => return self.ask_bypass(),
            _ => Mode::Default,
        };
        e.set_mode(next);
    }

    /// `/trust ask` in a folder that is not trusted: asks whether to trust it for this session.
    /// Nothing is read from the folder before the answer. "Not now" is first (selected) and Esc declines; the
    /// dialog needs a deliberate engage and waits out the arming delay like an approval. Returns whether a
    /// dialog was raised (not for a trusted folder, a starting session or one already asked).
    pub fn ask_trust(&mut self, now: i64, home: Option<&std::path::Path>) -> bool {
        let asked = self.dialogs.iter().any(|d| {
            matches!(
                &d.kind,
                DialogKind::Picker {
                    kind: PickerKind::Trust,
                    ..
                }
            )
        });
        let Some(e) = self.engine.as_ref().filter(|_| !self.trusted && !asked) else {
            return false;
        };
        // what the dialog needs to know: the local engine's folder is read here; a remote engine says it (it
        // decides and checks everything itself, the paths are display text)
        let (root, unfit, changed, parent, refusal, enforcing) = if e.is_remote() {
            // the snapshot of the engine's start report: no call, nothing to wait for
            let Some(st) = e.trust.clone() else {
                self.note("trust: the engine did not report its trust state", true);
                return false;
            };
            // the engine refuses every grant without a fingerprint (nothing could detect a change): not offered
            if let Some(why) = &st.unfit {
                self.note(
                    format!("trust not offered: change detection is not possible here ({why})"),
                    true,
                );
                return false;
            }
            let changed = (st.remembered == "changed").then(|| {
                (
                    st.changed_files.clone(),
                    st.since.clone().unwrap_or_default(),
                )
            });
            (
                std::path::PathBuf::from(&st.root),
                None,
                changed,
                st.parent.clone().map(std::path::PathBuf::from),
                st.refusal.clone(),
                Some(st.enforcing),
            )
        } else {
            // the grant is the nearest folder with .git or .synt above the session's folder, not the folder itself
            let root = crate::engine::root_of(std::path::Path::new(&e.cwd));
            // the trusted configuration must be fingerprintable: it is what a change is detected against
            let unfit = synthesis_session::trust::fingerprint(&root, std::path::Path::new(&e.cwd))
                .err()
                .map(|u| u.to_string());
            // trusted before, but the configuration moved on: say what changed (names only)
            let changed =
                match crate::engine::remembered_trust(&e.synthesis_home, home, &root, false) {
                    crate::engine::Remembered::Changed { files, since } => Some((files, since)),
                    _ => None,
                };
            // "all its subfolders" only for a real project parent: never the home folder, a filesystem or drive
            // root, a parent of home or the config folder (the same refusal as for the root)
            let parent = root
                .parent()
                .filter(|p| {
                    p.is_dir()
                        && crate::engine::parent_refusal(p, home, &e.synthesis_home).is_none()
                })
                .map(std::path::Path::to_path_buf)
                .filter(|_| unfit.is_none());
            let refusal = crate::engine::trust_refusal(&root, home, &e.synthesis_home);
            (root, unfit, changed, parent, refusal, None)
        };
        let cwd = e.remote_cwd.clone().unwrap_or_else(|| e.cwd.clone());
        if let Some(why) = refusal {
            self.note(why, true);
            return false;
        }
        let mut paragraphs = trust_text(&root, &cwd);
        if let Some((files, since)) = &changed {
            paragraphs.insert(
                1,
                format!(
                    "CHANGED since you trusted it on {since}: {}. Review these files (for example with git diff) before you trust the folder again; remembered trust is not honoured until you do.",
                    clip_left(&sanitize_for_display(&files.join(", ")), 300)
                ),
            );
        }
        if let Some(p) = enforcement_paragraph(enforcing) {
            paragraphs.insert(1, p);
        }
        if let Some(why) = &unfit {
            paragraphs.insert(
                1,
                format!(
                    "Change detection is NOT possible here: {why}. Trusting for this session runs WITHOUT any check that the configuration stays what you reviewed, and remembering is not offered."
                ),
            );
        }
        let headers = paragraphs.into_iter().map(|t| (0, t)).collect();
        let mut options = vec![(
            "no".to_string(),
            "Not now (keep the folder untrusted)".to_string(),
        )];
        if unfit.is_none() {
            options.push((
                "remember".into(),
                "Trust this folder (remember; data only: no hooks, MCP, allow rules, env or providers)".into(),
            ));
        }
        options.push((
            "session".into(),
            "Trust for this session only (applies project settings, hooks and MCP servers)".into(),
        ));
        if let Some(p) = &parent {
            options.push((
                "parent".into(),
                format!(
                    "Trust parent folder {} (all subfolders, including repositories cloned there later; remembered)",
                    clip_left(&sanitize_for_display(&p.display().to_string()), 40)
                ),
            ));
        }
        self.trust_root = Some(root);
        self.trust_parent = parent;
        self.push_dialog(
            Dialog {
                kind: DialogKind::Picker {
                    kind: PickerKind::Trust,
                    title: "Trust this folder?".into(),
                    headers,
                },
                options,
                sel: 0,
                armed_until: 0,
                engaged: false,
            },
            now,
        );
        true
    }

    /// `/trust`: the remembered folders (from the user store) with their date and whether their trusted
    /// configuration changed since; `/trust revoke <n|path>` asks first (Keep is the default).
    fn trust_cmd(&mut self, arg: &str) {
        let Some(e) = self.engine.clone() else {
            return self.note("/trust: the session is still starting", true);
        };
        if e.is_remote() {
            // WSL mode: the engine lists its own store (display text; `/trust revoke` names one of its rows). The
            // list is asked for in the background; a revoke resolves its number against the last one seen
            if arg.trim().starts_with("revoke") {
                let Some(st) = self.trust_cache.clone().or_else(|| e.trust.clone()) else {
                    return self.note("/trust: run /trust first", true);
                };
                return self.trust_cmd_with(arg, Some(st));
            }
            e.spawn_remote(|r| Some(UiMsg::TrustList(r.trust_state())));
            return self.note_kind(NoticeKind::Bookkeeping, "asking the engine...");
        }
        self.trust_cmd_with(arg, None);
    }

    fn trust_cmd_with(&mut self, arg: &str, remote: Option<crate::remote::TrustState>) {
        let Some(e) = &self.engine else {
            return self.note("/trust: the session is still starting", true);
        };
        let listing = if remote.is_some() {
            synthesis_session::trust::Listing {
                records: vec![],
                unverified: 0,
            }
        } else {
            match synthesis_session::trust::list(&e.synthesis_home) {
                Ok(r) => r,
                Err(err) => {
                    return self.note(format!("/trust: cannot read the store: {err}"), true);
                }
            }
        };
        let unverified = remote
            .as_ref()
            .map_or(listing.unverified, |r| r.unverified as usize);
        // the per-project baselines of a parent grant are bookkeeping, not something to list
        let recs: Vec<_> = if let Some(st) = &remote {
            st.records
                .iter()
                .map(|r| synthesis_session::trust::TrustRecord {
                    root: r.root.clone(),
                    scope: r.scope.clone(),
                    trusted_at: r.trusted_at.clone(),
                    fingerprint: r.state.clone(),
                })
                .collect()
        } else {
            listing
                .records
                .into_iter()
                .filter(|r| r.scope != synthesis_session::trust::CHILD)
                .collect()
        };
        let arg = arg.trim();
        if let Some(which) = arg.strip_prefix("revoke").map(str::trim) {
            let pick = which
                .parse::<usize>()
                .ok()
                .and_then(|n| n.checked_sub(1))
                .and_then(|i| recs.get(i))
                .or_else(|| recs.iter().find(|r| r.root == which));
            let session = self.session_grant.clone().filter(|g| g == which);
            let Some(root) = pick.map(|r| r.root.clone()).or(session) else {
                return self.note(
                    "usage: /trust revoke <number or path>   (numbers are those of /trust)",
                    true,
                );
            };
            if self.dialogs.iter().any(|d| {
                matches!(
                    &d.kind,
                    DialogKind::Picker {
                        kind: PickerKind::Revoke,
                        ..
                    }
                )
            }) {
                return;
            }
            let shown = clip_left(&sanitize_for_display(&root), 60);
            self.revoke_root = Some(root);
            self.push_dialog(
                Dialog {
                    kind: DialogKind::Picker {
                        kind: PickerKind::Revoke,
                        title: "Revoke trust?".into(),
                        headers: vec![(0, format!("{shown}: it is asked about again at the next engine start (a new session, /model or a restart). This session keeps what it already loaded."))],
                    },
                    options: vec![
                        ("keep".into(), "Keep it trusted".into()),
                        ("revoke".into(), "Revoke".into()),
                    ],
                    sel: 0,
                    armed_until: 0,
                    engaged: false,
                },
                now_ms(),
            );
            return;
        }
        if !arg.is_empty() && arg != "list" {
            return self.note("usage: /trust [ask | revoke <number or path>]", true);
        }
        let mut lines = vec![format!(
            "this session: {}",
            if self.trusted {
                "folder trusted"
            } else {
                "folder not trusted"
            }
        )];
        if remote.as_ref().is_some_and(|r| !r.enforcing)
            || (remote.is_none() && e.sandbox.mode != "enforce")
        {
            lines.push(format!(
                "remembered trust is NOT honoured now: the sandbox is {} (a command that can run unconfined could have written the store); session-only trust and --trust-project still work",
                if e.sandbox.mode.is_empty() { "not reported" } else { e.sandbox.mode.as_str() }
            ));
        }
        if unverified > 0 {
            lines.push(format!(
                "unverified rows ignored: {unverified} (not written by this app)"
            ));
        }
        if let Some(g) = &self.session_grant {
            lines.push(format!(
                "session-only trust active for {}: until the app exits, for every session of that folder; /trust revoke {} ends it",
                clip_left(&sanitize_for_display(g), 60),
                clip_left(&sanitize_for_display(g), 60)
            ));
        }
        lines.push(
            "a remembered trust is deliberately narrower than a session trust: project hooks, MCP servers, allow rules, env, providers and verification checks are ignored; use \"Trust for this session\" to apply them"
                .into(),
        );
        if recs.is_empty() {
            lines.push("no remembered folder (/trust ask -> Trust this folder (remember))".into());
        }
        for (i, r) in recs.iter().enumerate() {
            let state = if remote.is_some() {
                // the engine's own verdict (the fingerprint field carries it)
                r.fingerprint.as_str()
            } else if r.scope == synthesis_session::trust::PARENT {
                "each project is checked at its first start"
            } else if std::path::Path::new(&r.root).is_dir() {
                match synthesis_session::trust::fingerprint_root(std::path::Path::new(&r.root)) {
                    Ok(f) if f == r.fingerprint => "config unchanged",
                    _ => "CONFIG CHANGED: not honoured until you trust it again",
                }
            } else {
                "folder missing"
            };
            lines.push(format!(
                "{}. {}{}  {}  {state}",
                i + 1,
                clip_left(&sanitize_for_display(&r.root), 70),
                if r.scope == synthesis_session::trust::PARENT {
                    "  (and its subfolders)"
                } else {
                    ""
                },
                r.trusted_at.get(..10).unwrap_or(&r.trusted_at),
            ));
        }
        self.note(lines.join("\n"), false);
    }

    /// The bypassPermissions confirmation. Cancel is first (the selected option); Esc cancels too. The mode
    /// changes only in [`Live::answer`], after the core took it.
    fn ask_bypass(&mut self) {
        let open = self.dialogs.iter().any(|d| {
            matches!(
                &d.kind,
                DialogKind::Picker {
                    kind: PickerKind::Bypass,
                    ..
                }
            )
        });
        if open {
            return;
        }
        self.push_dialog(
            Dialog {
                kind: DialogKind::Picker {
                    kind: PickerKind::Bypass,
                    title: "Enable bypass permissions?".into(),
                    headers: vec![(
                        0,
                        "Bypass skips every tool permission prompt: file edits and commands run unattended, without asking you. Deny rules, ask rules and critical deletions (root, home, working directory) still ask. Protected files such as .env are NOT guarded. It lasts for this session only.".into(),
                    )],
                },
                options: vec![
                    ("cancel".into(), "Cancel (keep the current mode)".into()),
                    ("enable".into(), "Enable bypass".into()),
                ],
                sel: 0,
                armed_until: 0,
                engaged: false,
            },
            now_ms(),
        );
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
        other,
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
    let typed = other[*cur]
        .as_deref()
        .map_or(String::new(), |t| format!(": {}", sanitize_for_display(t)));
    v.push((
        "other".into(),
        format!("Other{typed} (type your own answer)"),
    ));
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

    #[test]
    fn a_rename_answer_for_another_session_is_not_applied() {
        let mut l = Live::new();
        l.sid = "open".into();
        l.on_msg(UiMsg::Renamed("old".into(), Ok("late title".into())), 0);
        assert!(
            l.renamed.is_none(),
            "an answer for a session that is gone changes nothing"
        );
        l.on_msg(UiMsg::Renamed("open".into(), Ok("title".into())), 0);
        assert_eq!(l.renamed, Some(("open".into(), "title".into())));
    }

    #[test]
    fn the_trust_dialog_says_when_a_remembered_trust_will_not_be_honoured() {
        // the engine's sandbox does not enforce: stored, but not honoured later
        let p = enforcement_paragraph(Some(false)).expect("said");
        assert!(p.contains("NOT enforcing") && p.contains("NOT be honoured"));
        assert!(enforcement_paragraph(Some(true)).is_none());
        assert!(
            enforcement_paragraph(None).is_none(),
            "the local engine decides this itself"
        );
    }

    #[test]
    fn a_dialog_queued_behind_a_picker_is_armed_when_the_picker_is_answered() {
        let mut l = Live::new();
        let picker = |kind| Dialog {
            kind: DialogKind::Picker {
                kind,
                title: String::new(),
                headers: vec![],
            },
            options: vec![("a".into(), "a".into())],
            sel: 0,
            armed_until: 0,
            engaged: false,
        };
        l.push_dialog(picker(PickerKind::Bypass), 0);
        l.push_dialog(picker(PickerKind::Model), 0);
        l.answer("cancel", 1_000_000);
        assert_eq!(l.dialogs.len(), 1);
        assert!(l.dialogs[0].armed_until >= 1_000_000 + ARM_MS);
    }
    use serde_json::json;

    fn live_with_commands() -> Live {
        let mut l = Live::new();
        l.commands = synthesis_cli_ui::local_commands();
        l.commands.push(panels_command());
        l.commands.push(send_command());
        l.commands.push(reveal_command());
        l.commands.push(rewind_command());
        l.commands.push(clean_command());
        l.commands.push(close_workspace_command());
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
        assert_eq!(l.submit("/files"), Submitted::Do(Action::ShowFiles));
        assert_eq!(
            l.submit("/reveal /home/a"),
            Submitted::Do(Action::Reveal("/home/a".into()))
        );
        assert_eq!(l.submit("/diff"), Submitted::Do(Action::ShowDiff(None)));
        assert_eq!(
            l.submit("/diff a.rs"),
            Submitted::Do(Action::ShowDiff(Some("a.rs".into())))
        );
        assert_eq!(l.submit("/peers"), Submitted::Do(Action::Peers));
        // the CLI's aliases
        assert_eq!(l.submit("/quit"), Submitted::Quit);
        l.submit("/perms bogus");
        assert!(
            matches!(l.view.convo.items.last(), Some(Item::Notice { warning: true, text }) if !text.contains("unknown command"))
        );
        assert_eq!(
            l.submit("/send ws:api/sess:backend  what sets the TTL? "),
            Submitted::Do(Action::Send {
                to: "ws:api/sess:backend".into(),
                text: "what sets the TTL?".into()
            })
        );
        assert_eq!(l.submit("/send ws:api/sess:backend"), Submitted::Nothing);
        assert!(
            matches!(l.view.convo.items.last(), Some(Item::Notice { warning: true, text }) if text.starts_with("usage: /send"))
        );
        // no arguments: the picker
        assert_eq!(l.submit("/send"), Submitted::Nothing);
        assert!(matches!(
            l.dialogs[0].kind,
            DialogKind::Picker {
                kind: PickerKind::Send,
                ..
            }
        ));
        l.reject();
        assert!(!l.focus_view);
        assert_eq!(l.submit("/focus"), Submitted::Nothing);
        assert!(l.focus_view);
        l.submit("/focus off");
        assert!(!l.focus_view);
        assert_eq!(l.submit("/resume"), Submitted::Nothing);
        assert!(
            matches!(l.view.convo.items.last(), Some(Item::Notice { text, .. }) if text.contains("no saved sessions"))
        );
        assert_eq!(l.submit("/nope"), Submitted::Nothing);
        assert!(
            matches!(l.view.convo.items.last(), Some(Item::Notice { text, .. }) if text == "unknown command /nope")
        );
        // `!cmd` is a line like any other (queued while there is no engine); `!` alone is nothing
        assert_eq!(l.submit("!"), Submitted::Nothing);
        assert!(l.queue.is_empty());
        assert_eq!(l.submit("!ls"), Submitted::Nothing);
        assert_eq!(l.queue.pop_back().as_deref(), Some("!ls"));
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

    fn event(t: &str, payload: Value) -> UiMsg {
        let mut v = json!({"v":1,"seq":1,"ts":"2026-01-01T00:00:00Z","session_id":"s","task_id":null,"turn_id":null,"type":t});
        v["payload"] = payload;
        UiMsg::Event(Box::new(serde_json::from_value(v).unwrap()))
    }

    fn sent(id: &str, to: &str, origin: &str, text: &str) -> UiMsg {
        event(
            "message.sent",
            json!({"message_id": id, "from": "user", "to": to, "origin": origin, "kind": "message", "text": text, "bytes": 1}),
        )
    }

    fn dropped(id: &str) -> UiMsg {
        event(
            "message.dropped",
            json!({"message_id": id, "reason": "cancelled"}),
        )
    }

    #[test]
    fn only_lines_this_window_sent_are_pending_and_dropped_ones_keep_their_order() {
        let mut l = Live::new();
        // a user-origin message this window never sent is not a pending send-now
        l.on_msg(sent("x", "main", "user", "from elsewhere"), 0);
        assert!(l.pending.is_empty());
        l.injected = vec![
            ("main".into(), "one".into()),
            ("main".into(), "two".into()),
            ("worker".into(), "three".into()),
        ];
        l.queue.push_back("later".into());
        for (id, to, t) in [
            ("1", "main", "one"),
            ("2", "main", "two"),
            ("3", "worker", "three"),
        ] {
            l.on_msg(sent(id, to, "user", t), 0);
        }
        assert_eq!(l.pending.len(), 3);
        for id in ["1", "2", "3"] {
            l.on_msg(dropped(id), 0);
        }
        assert_eq!(l.queue, ["one", "two", "later"]);
        assert!(l.pending.is_empty());
        assert!(
            matches!(l.view.convo.items.last(), Some(Item::Notice { warning: true, text }) if text.contains("@worker"))
        );
        // the task ending forgets what was never answered: a late message.sent is ignored
        l.injected = vec![("main".into(), "late".into())];
        l.on_msg(UiMsg::TaskDone(Ok(())), 0);
        l.on_msg(sent("9", "main", "user", "late"), 0);
        assert!(l.pending.is_empty());
    }

    #[test]
    fn an_exact_line_is_matched_before_a_prefix_and_failed_lines_requeue_in_order() {
        let mut l = Live::new();
        l.injected = vec![("main".into(), "abc".into()), ("main".into(), "a".into())];
        l.on_msg(sent("1", "main", "user", "a"), 0);
        assert_eq!(l.pending[0].2, "a");
        assert_eq!(l.injected, [("main".to_string(), "abc".to_string())]);
        l.queue.push_back("later".into());
        l.injected = vec![("main".into(), "one".into()), ("main".into(), "two".into())];
        for t in ["one", "two"] {
            let (to, text, why) = ("main".to_string(), t.to_string(), "closed".to_string());
            l.on_msg(UiMsg::SendFailed { to, text, why }, 0);
        }
        assert_eq!(l.queue, ["one", "two", "later"]);
        assert!(l.injected.is_empty(), "a refused line is no longer awaited");
    }

    #[test]
    fn a_sub_agent_name_is_shown_clean_and_short() {
        let n = label(&format!("a\u{1b}[31mb{}", "x".repeat(100)));
        assert!(!n.contains('\u{1b}') && n.chars().count() <= 40, "{n}");
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
        assert_eq!(l.pick(3, 1000), None, "next question");
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
        assert_eq!(l.pick(3, 1000).as_deref(), Some("done"));
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

    #[test]
    fn every_builtin_command_works_or_says_it_is_not_available() {
        let mut unwired: Vec<String> = synthesis_cli_ui::local_commands()
            .into_iter()
            .map(|c| c.name)
            .filter(|n| !is_wired(n))
            .collect();
        unwired.sort();
        assert!(unwired.is_empty(), "{unwired:?}");
        // the wired ones are all real commands, and the unwired ones answer with the notice
        let mut l = live_with_commands();
        for c in synthesis_cli_ui::local_commands() {
            assert!(!is_wired(&c.name) || WIRED.contains(&c.name.as_str()));
            if !is_wired(&c.name) {
                assert_eq!(l.submit(&format!("/{}", c.name)), Submitted::Nothing);
                assert!(
                    matches!(l.view.convo.items.last(), Some(Item::Notice { text, .. }) if text.ends_with(NOT_YET)),
                    "{}",
                    c.name
                );
            }
        }
        assert!(WIRED.iter().all(|w| {
            synthesis_cli_ui::local_commands()
                .iter()
                .any(|c| c.name == *w)
        }));
        // the actions the window carries out
        assert_eq!(l.submit("/config"), Submitted::Do(Action::OpenSettings));
        assert_eq!(
            l.submit("/panels usage off"),
            Submitted::Do(Action::Panels("usage off".into()))
        );
        assert_eq!(l.submit("/new"), Submitted::Do(Action::New));
        assert_eq!(l.submit("/close"), Submitted::Do(Action::Close));
        assert_eq!(l.submit("/clear"), Submitted::Do(Action::Clear));
        assert_eq!(
            l.submit("/compact keep the plan"),
            Submitted::Do(Action::Compact(Some("keep the plan".into())))
        );
        assert_eq!(
            l.submit("/resume abc"),
            Submitted::Do(Action::Resume("abc".into()))
        );
        assert_eq!(
            l.submit("/model a/b"),
            Submitted::Do(Action::Model("a/b".into()))
        );
        l.busy = true;
        assert_eq!(
            l.submit("/clear"),
            Submitted::Nothing,
            "not while a task runs"
        );
        assert_eq!(l.submit("/model a/b"), Submitted::Nothing);
    }

    #[test]
    fn images_are_capped_like_the_tui() {
        let mut l = Live::new();
        for i in 1..=MAX_ATTACHMENTS {
            assert!(
                l.attach_image("image/png", &[1, 2, 3], Some((1, 1)))
                    .unwrap()
                    .contains(&format!("image {i}"))
            );
        }
        assert!(
            l.attach_image("image/png", &[1], None)
                .unwrap_err()
                .contains("at most 5")
        );
        let taken = l.take_images();
        assert_eq!(taken.len(), 5);
        assert!(l.images.is_empty(), "they go with one message");
        // 6 MiB of base64 in all: 4 MB of bytes is 5.3 MB of base64, a second one does not fit
        let big = vec![7u8; 4 << 20];
        assert!(l.attach_image("image/png", &big, None).is_ok());
        assert!(
            l.attach_image("image/png", &big, None)
                .unwrap_err()
                .contains("at most 6 MB")
        );
    }

    #[test]
    fn an_attachment_chip_can_be_removed() {
        let mut l = Live::new();
        for _ in 0..3 {
            l.attach_image("image/png", &[1], None).unwrap();
        }
        let second = l.images[1].name.clone();
        l.remove_image(1);
        l.remove_image(9);
        assert_eq!(l.images.len(), 2);
        assert!(l.images.iter().all(|a| a.name != second));
    }

    fn question(multi: bool) -> Live {
        let mut l = Live::new();
        let o = |s: &str| synthesis_tools::QuestionOption {
            label: s.into(),
            description: None,
        };
        let q = Question {
            question: "which".into(),
            header: None,
            multi_select: multi,
            options: vec![o("a"), o("b")],
        };
        l.on_msg(
            UiMsg::Input(Box::new(crate::engine::AskInput {
                id: "q".into(),
                req: NeedsUser::AskUserQuestion { questions: vec![q] },
            })),
            0,
        );
        l
    }

    #[test]
    fn other_is_a_free_text_answer() {
        let mut l = question(false);
        assert_eq!(l.pick(0, 1000), None);
        // the "Other" row sits before "send" and "decline"
        let other = l.dialogs[0]
            .options
            .iter()
            .position(|(k, _)| k == "other")
            .unwrap();
        assert_eq!(l.pick(other, 1000), None, "opens the text field");
        assert!(l.typing_other());
        assert!(l.type_other("pur") && l.type_other("ple"));
        l.backspace_other();
        l.commit_other();
        assert!(!l.typing_other());
        let (_, label) = &l.dialogs[0].options[other];
        assert!(label.starts_with("Other: purpl"), "{label}");
        // a single choice: the typed answer replaced the earlier pick
        assert!(l.dialogs[0].options[0].1.starts_with("( )"));
        // Esc while typing leaves the field, not the dialog
        l.pick(other, 1000);
        assert_eq!(l.reject(), None);
        assert!(!l.typing_other() && l.dialogs.len() == 1);
        // multi-select keeps the picked options next to the typed one
        let mut m = question(true);
        m.pick(1, 1000);
        let other = m.dialogs[0]
            .options
            .iter()
            .position(|(k, _)| k == "other")
            .unwrap();
        m.pick(other, 1000);
        m.type_other("x");
        m.commit_other();
        let DialogKind::Question { picked, other, .. } = &m.dialogs[0].kind else {
            panic!()
        };
        assert_eq!(
            (picked[0].clone(), other[0].clone()),
            (vec![1], Some("x".to_string()))
        );
    }

    #[test]
    fn a_picker_closes_with_esc_and_never_answers_the_engine() {
        let mut l = Live::new();
        l.dialogs.push(Dialog {
            kind: DialogKind::Picker {
                kind: PickerKind::Resume,
                title: "t".into(),
                headers: vec![(0, "ws".into()), (2, "other".into())],
            },
            options: vec![
                ("a".into(), "A".into()),
                ("b".into(), "B".into()),
                ("c".into(), "C".into()),
            ],
            sel: 2,
            armed_until: 0,
            engaged: false,
        });
        // the third option is the fifth child: two group headers come before it
        assert_eq!(l.dialogs[0].scroll_child(), Some(4));
        assert_eq!(l.reject(), None);
        assert!(l.dialogs.is_empty());
    }

    fn peer(key: &str, sid: &str, ws: &str, name: &str) -> Value {
        json!({"address": format!("ws:{key}/sess:{sid}/agent:main"),
            "workspace": {"label": ws, "key": key, "root": "/w"},
            "session": {"short_id": &sid[..8], "name": name},
            "agent": {"instance": "main", "kind": "main", "status": "idle"}, "task_summary": ""})
    }

    fn send_picker() -> Live {
        let mut l = live_with_commands();
        l.set_peer_rows(
            vec![
                peer("aaaa1111bbbb", "11111111-0000", "api", "backend"),
                peer("cccc2222dddd", "22222222-0000", "web", "backend"),
            ],
            0,
        );
        assert_eq!(l.submit("/send"), Submitted::Nothing);
        l
    }

    #[test]
    fn the_send_picker_filters_and_a_pick_arms_a_one_shot_chip_by_id() {
        let mut l = send_picker();
        let d = &l.dialogs[0];
        assert_eq!(d.options.len(), 2, "same name twice, told apart by id");
        assert_eq!(d.options[0].0, "ws:aaaa1111bbbb/sess:11111111-0000");
        assert!(
            !d.digits_pick() && !d.answerable(),
            "engaged by typing or arrows first"
        );
        // digits are text in the filter (ids are hex)
        assert!(l.model_filter_key(Some("2"), false));
        assert!(l.model_filter_key(Some("2"), false));
        assert_eq!(l.dialogs[0].options.len(), 1);
        let key = l.pick(0, i64::MAX).unwrap();
        assert_eq!(l.answer(&key, 0), None);
        let chip = l.send_to.clone().unwrap();
        assert_eq!(
            chip.label,
            "web (ws:cccc22) \u{203a} backend (sess:22222222)"
        );
        // the line goes to the id address, whatever the names are, and spends the chip
        assert_eq!(
            l.submit("what sets the TTL?"),
            Submitted::Do(Action::Send {
                to: "ws:cccc2222dddd/sess:22222222-0000".into(),
                text: "what sets the TTL?".into()
            })
        );
        assert!(l.send_to.is_none());
        // the echo comes with the result, not before it
        assert!(
            !matches!(l.view.convo.items.last(), Some(Item::Notice { text, .. }) if text.starts_with("sent to"))
        );
        l.send_done("ws:cccc2222dddd/sess:22222222-0000", Ok(String::new()));
        assert!(
            matches!(l.view.convo.items.last(), Some(Item::Notice { warning: false, text })
            if text == "sent to ws:cccc2222../sess:22222222..")
        );
        // it never reached the prompt queue
        assert!(l.queue.is_empty());
    }

    #[test]
    fn esc_sends_nothing_and_a_chip_ends_with_esc_ctrl_t_or_a_gone_target() {
        let mut l = send_picker();
        assert_eq!(l.reject(), None);
        assert!(l.dialogs.is_empty() && l.send_pick.is_none() && l.send_to.is_none());
        let arm = |l: &mut Live| {
            l.submit("/send");
            l.model_filter_key(Some("web"), false);
            let k = l.pick(0, i64::MAX).unwrap();
            l.answer(&k, 0);
            assert!(l.send_to.is_some());
        };
        arm(&mut l);
        assert!(l.interrupt() && l.send_to.is_none(), "Esc clears the chip");
        arm(&mut l);
        l.busy = true;
        l.cycle_target();
        assert!(l.send_to.is_none());
        // a command leaves the chip for the next line
        arm(&mut l);
        l.busy = false;
        l.submit("/focus");
        assert!(l.send_to.is_some());
        // the peer left: held with a notice, nothing sent, the chip is spent
        l.set_peer_rows(vec![], 0);
        assert_eq!(l.submit("hello"), Submitted::Held);
        assert!(l.send_to.is_none());
        assert!(
            matches!(l.view.convo.items.last(), Some(Item::Notice { warning: true, text })
            if text == "web (ws:cccc22) \u{203a} backend (sess:22222222) is gone, nothing sent")
        );
    }

    #[test]
    fn an_enter_typed_ahead_picks_nothing_and_an_approval_goes_in_front_of_the_picker() {
        let mut l = live_with_commands();
        l.set_peer_rows(
            vec![peer("aaaa1111bbbb", "11111111-0000", "api", "backend")],
            0,
        );
        l.submit("/send");
        let d = &l.dialogs[0];
        // armed against type-ahead, and Enter answers only after the person engaged it
        assert!(d.armed_until > 0 && !d.answerable());
        assert_eq!(l.pick(0, 0), None, "still arming");
        assert!(l.pick(0, now_ms() + ARM_MS + 1).is_some());
        l.move_sel(true);
        assert!(l.dialogs[0].answerable());
        // an approval arrives: it is the front dialog, the picker's keys do nothing
        l.on_approval(
            AskApproval {
                id: "a".into(),
                agent_id: None,
                tool: "Bash".into(),
                input: json!({"command": "ls"}),
                dialog: "permission",
                prompt: String::new(),
                reason: String::new(),
                origin: None,
                suggestions: vec![],
            },
            0,
        );
        assert!(matches!(l.dialogs[0].kind, DialogKind::Approval(_)));
        assert_eq!(l.dialogs.len(), 2);
        assert!(!l.model_filter_key(Some("x"), false));
        assert_eq!(l.send_pick.as_ref().unwrap().filter, "");
        assert!(l.send_to.is_none());
    }

    #[test]
    fn an_empty_send_picker_says_why_and_picks_nothing() {
        let mut l = live_with_commands();
        l.submit("/send");
        let d = &l.dialogs[0];
        assert_eq!(d.options.len(), 1);
        assert!(d.options[0].1.contains("no other live session"));
        let k = l.pick(0, i64::MAX).unwrap();
        assert_eq!(l.answer(&k, 0), None);
        assert!(l.send_to.is_none() && l.dialogs.is_empty());
        assert!(send_pick::empty_note(true, false).contains("--peers"));
    }

    /// The texts of the chat's notice and internal rows that the quiet chat draws.
    fn drawn(l: &Live, verbose: bool, focus: bool) -> Vec<String> {
        l.view
            .convo
            .items
            .iter()
            .filter(|i| crate::tui_view::chat::visible(i, false, verbose, focus))
            .filter_map(|i| match i {
                Item::Notice { text, .. } | Item::Internal(text) => Some(text.clone()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn warnings_and_results_are_always_drawn_and_bookkeeping_only_in_verbose() {
        let mut l = Live::new();
        l.note_kind(NoticeKind::Warning, "warn");
        l.note_kind(NoticeKind::Result, "result");
        l.note_kind(NoticeKind::Bookkeeping, "book");
        l.note("old-style warning", true);
        l.note("old-style result", false);
        assert_eq!(
            drawn(&l, false, false),
            ["warn", "result", "old-style warning", "old-style result"]
        );
        assert_eq!(drawn(&l, true, false).len(), 5, "verbose shows every row");
        assert_eq!(drawn(&l, true, false)[2], "book");
        // /focus keeps prompts, answers, results and notices; bookkeeping is not one
        assert_eq!(drawn(&l, true, true).len(), 4);
        // a warning is a warning row, never an internal one
        assert!(matches!(
            l.view.convo.items[0],
            Item::Notice { warning: true, .. }
        ));
    }

    #[test]
    fn errors_are_never_hidden_whatever_the_switches() {
        let mut l = live_with_commands();
        l.submit("/nope");
        for (verbose, focus) in [(false, false), (true, false), (false, true), (true, true)] {
            let d = drawn(&l, verbose, focus);
            assert!(d.iter().any(|t| t == "unknown command /nope"), "{d:?}");
        }
    }

    #[test]
    fn the_verbose_command_result_shows_even_when_it_turns_verbose_off() {
        let mut l = live_with_commands();
        l.submit("/verbose");
        l.submit("/verbose");
        assert_eq!(drawn(&l, false, false), ["verbose on", "verbose off"]);
    }

    #[test]
    fn a_queued_instead_line_is_bookkeeping() {
        let mut l = Live::new();
        l.note_kind(
            NoticeKind::Bookkeeping,
            "commands are not sent now: queued instead",
        );
        assert!(drawn(&l, false, false).is_empty());
    }

    #[test]
    fn the_window_hint_is_cut_for_status_and_kept_for_the_setter() {
        let src = "unknown (provider \"x\" is not configured); compaction assumes 200k. Set it: /context <tokens>";
        assert_eq!(
            window_short(src),
            "unknown (provider \"x\" is not configured); compaction assumes 200k"
        );
        assert_eq!(window_short("200k (config)"), "200k (config)");
    }

    #[test]
    fn age_reads_like_the_tui() {
        assert_eq!(age(30_000), "just now");
        assert_eq!(age(5 * 60_000), "5m ago");
        assert_eq!(age(3 * 3_600_000), "3h ago");
        assert_eq!(age(3 * 86_400_000), "3d ago");
    }
}
