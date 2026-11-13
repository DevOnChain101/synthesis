//! The real engine in this process (docs/specs/desktop.md section 1, E1): `synthesis_app::run::prepare`
//! builds the harness, the window talks to it through channels. It mirrors what `synt` does around
//! one prepared session (`apps/cli/src/tui.rs` `round` and its `Host`): the session start, the
//! `SessionStart`/`SessionEnd` hooks, skills, MCP prompts and mentions, the memory block, the stored
//! usage and the shutdown. Events arrive as typed envelopes, approvals and questions wait in maps until
//! the person answers, a task runs on the engine's own tokio runtime. The API key only lives in the
//! `Env` snapshot handed to `prepare`, never in the process env.
//!
//! The session runs in this process (`synthesis_app::host::Live`, shared with `synt engine --stdio`) or, as a
//! remote engine, in a child process that speaks that protocol (`remote.rs`).

use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use clap::Parser;
use futures::channel::mpsc::{UnboundedSender, unbounded};
use serde_json::{Value, json};
use synthesis_app::asks::{Ask, Asks, Notify};
use synthesis_app::host::{Live, lock};
use synthesis_app::run::{Common, Front, Limits, Native, prepare};
use synthesis_cli_ui::Resume;
use synthesis_cli_ui_base::complete::Command;
use synthesis_config::Env;
use synthesis_events::Envelope;
use synthesis_harness::{ImageAttachment, UserReply, sanitize_for_display};
use synthesis_permissions::Mode;
use tokio_util::sync::CancellationToken;

/// A `/command` or `!shell` line (its first line starts with the mark): never a send-now delivery.
pub fn is_command(text: &str) -> bool {
    text.starts_with('/') || text.starts_with('!')
}

/// What the engine sends to the window.
pub enum UiMsg {
    Event(Box<Envelope>),
    /// An event line this build cannot read: visible, not skipped (like the CLI).
    BadEvent(String),
    /// A permission ask; answer it with [`Engine::answer`].
    Approval(Box<AskApproval>),
    /// A plan or question dialog; answer it with [`Engine::answer_input`].
    Input(Box<AskInput>),
    /// The ask with this id is gone (cancelled, expired or answered).
    Withdraw(String),
    /// The task ended (`Err`: it could not run, or the session did not return to idle).
    TaskDone(Result<(), String>),
    /// [`Engine::send_now`] was refused (inbox full, hook block, no task running yet).
    SendFailed {
        to: String,
        text: String,
        why: String,
    },
    /// `/compact` between tasks ended (`Err`: the reason, history unchanged).
    Compacted(Result<(), String>),
    /// `/compact` was accepted while a task runs: the harness compacts at its next safe point.
    CompactQueued,
    /// Something a background call to the remote engine has to say (`true`: a problem).
    Note(String, bool),
    /// `/rename` ended (`Ok`: the stored title).
    /// `(session id it was asked for, result)`: applied only while that session is the open one.
    Renamed(String, Result<String, String>),
    /// `/trust` asked the engine for its state.
    TrustList(Result<crate::remote::TrustState, String>),
}

pub use synthesis_app::asks::{AskApproval, AskInput, Choice, approval_for};

/// `Write` that turns the JSONL event lines of the sink into `UiMsg::Event`.
struct Lines {
    tx: UnboundedSender<UiMsg>,
    buf: Vec<u8>,
}

impl io::Write for Lines {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.buf.extend_from_slice(bytes);
        while let Some(i) = self.buf.iter().position(|b| *b == b'\n') {
            let line: Vec<u8> = self.buf.drain(..=i).collect();
            let line = &line[..line.len() - 1];
            if line.is_empty() {
                continue;
            }
            let msg = match serde_json::from_slice::<Envelope>(line) {
                Ok(e) => UiMsg::Event(Box::new(e)),
                Err(e) => UiMsg::BadEvent(format!("unreadable event: {e}")),
            };
            // the window is gone (quit): the harness runs to its own end, nobody to tell
            let _ = self.tx.unbounded_send(msg);
        }
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// The flags of `synt` plus `--cwd` and `--sample`: the desktop takes the same `Common` flags as
/// `synt` (`--model`, `--sandbox`, `--trust-project`, `--approve-mcp`, `--peers`, `--resume`, ...).
#[derive(Parser, Clone)]
#[command(name = "synthesis-desktop", about = "Synthesis desktop app")]
pub struct Args {
    /// Start folder (default: the current directory).
    #[arg(long)]
    pub cwd: Option<PathBuf>,
    /// Draw the window on sample data; no engine, no model, nothing is read or written.
    #[arg(long)]
    pub sample: bool,
    #[command(flatten)]
    pub common: Common,
}

#[derive(Parser)]
struct Defaults {
    #[command(flatten)]
    c: Common,
}

pub use synthesis_app::trust::{
    Remembered, TRUST_CHANGED, TrustCheck, common_for, display_path, launch_common, parent_refusal,
    remembered_trust, remembered_trusted, root_of, session_trusted, trust_changed_error,
    trust_policy, trust_refusal,
};

/// `--resume ID` (id or unique prefix) or `--continue` (newest session of the start folder).
pub fn requested_resume(
    c: &Common,
    start: &Path,
    synthesis_home: &Path,
) -> Result<Option<Resume>, String> {
    if c.fork_session {
        return Err("--fork-session is only for `synt run`".into());
    }
    let id = match (&c.resume, c.continue_session) {
        (Some(id), _) => id.clone(),
        (None, true) => {
            synthesis_cli_ui::list_saved(synthesis_home, Some(&start.to_string_lossy()))?
                .into_iter()
                .next()
                .map(|s| s.id)
                .ok_or("--continue: no saved session for this directory")?
        }
        (None, false) => return Ok(None),
    };
    load_resume_in_place(synthesis_home, &id).map(Some)
}

/// `load_resume`, and the session's folder must still exist.
pub fn load_resume_in_place(home: &Path, id: &str) -> Result<Resume, String> {
    let r = synthesis_cli_ui::load_resume(home, id)?;
    if !Path::new(&r.record.cwd).is_dir() {
        return Err(format!(
            "cannot resume {id}: its folder {} no longer exists",
            r.record.cwd
        ));
    }
    Ok(r)
}

/// What `synt` does once at start, before the first session: stored images older than
/// `synt.images.retentionDays` become text, and sessions nobody typed into (older than an hour,
/// not `keep`, not locked) are deleted. Problems come back as lines for the chat, never as a
/// reason not to start.
pub fn housekeeping(synthesis_home: &Path, keep: &str) -> Vec<String> {
    let mut out = vec![];
    let (days, diag) =
        synthesis_app::ctx::read_image_retention(&synthesis_home.join("settings.json"));
    out.extend(diag.into_iter().map(|d| format!("warning: {d}")));
    let open = || {
        synthesis_session::Store::open(synthesis_home, synthesis_session::StoreOptions::default())
    };
    if days != 0 {
        match open().and_then(|s| s.purge_images(days, time::OffsetDateTime::now_utc())) {
            Ok(r) if r.images == 0 => {}
            Ok(r) => out.push(format!(
                "rewrote {} stored image(s) older than {days} days to text",
                r.images
            )),
            Err(e) => out.push(format!("warning: image retention: {e}")),
        }
    }
    let hour = std::time::Duration::from_secs(3600);
    if let Err(e) =
        open().and_then(|s| s.prune_empty_sessions(keep, hour, time::OffsetDateTime::now_utc()))
    {
        out.push(format!("warning: pruning empty sessions: {e}"));
    }
    out
}

/// Everything `start_with` needs besides the environment.
pub struct StartOpts {
    pub project: PathBuf,
    pub common: Common,
    /// A stored session that continues in place (workspace, history, store).
    pub resume: Option<Resume>,
    /// Lines for the chat's start-up notes (why a relaunch happened, ...).
    pub notes: Vec<String>,
    /// The configuration the person trusted, checked again once the engine has loaded it and before any
    /// hook runs (`None`: nothing to check, as with `--trust-project`).
    pub verify: Option<TrustCheck>,
}

/// One row of the MCP panel.
#[derive(Debug, Clone)]
pub struct McpInfo {
    pub name: String,
    /// `connected`, `connecting`, `failed`, ... (the manager's state, lowercased).
    pub state: String,
    pub tools: usize,
}

/// What a method that only the local engine has says in remote mode (PR3 fills them in).
pub const NOT_REMOTE: &str = "not supported in remote mode yet";

pub struct Engine {
    pub sid: String,
    pub label: String,
    pub models: Vec<String>,
    /// The picker's rows with what the cache knows of each model.
    pub model_rows: Vec<crate::models::ModelRow>,
    /// The stored model that could not be used (the session runs on `label`; the stored id is kept).
    pub stale_model: Option<String>,
    /// The context window and where it comes from, or why it is unknown (`/status`).
    pub context_source: String,
    pub effort_tiers: Vec<String>,
    pub notices: Vec<String>,
    pub cwd: String,
    /// The workspace root (what the Workspace panel lists).
    pub root: String,
    /// Slash commands: the local list, skills and MCP prompts.
    pub commands: Vec<Command>,
    pub context_window: Option<u64>,
    /// The folder is trusted for project config (`--trust-project`). Never true in remote mode.
    pub trusted: bool,
    /// Remote mode only: what the engine says its folder and root are. Display text, never a path to open.
    pub remote_cwd: Option<String>,
    pub remote_root: Option<String>,
    /// WSL mode only: the distro the engine runs in (to turn its Linux paths into Windows ones).
    pub remote_distro: Option<String>,
    /// Remote mode: what the engine said the Trust dialog should show, in its start report.
    pub trust: Option<crate::remote::TrustState>,
    pub synthesis_home: PathBuf,
    /// The environment the session started with (`/memory` and `/doctor` read it, names only).
    pub env: Env,
    /// The session in this process (`None`: it runs in a child process, see `remote`).
    live: Option<Live>,
    remote: Option<crate::remote::Remote>,
    tx: UnboundedSender<UiMsg>,
    pub sandbox: synthesis_cli_ui::SandboxStatus,
    pub memory_on: bool,
    /// Lines of `/memory`: the instruction files loaded at start and where `#` notes go.
    pub memory_report: Vec<String>,
    /// Where `#` notes go (the CLI's `MemoryHook`).
    pub notes: synthesis_app::memory::Notes,
    /// A `!cmd` ended and the model should read its output (the window starts that turn).
    followup: Arc<std::sync::atomic::AtomicBool>,
}

impl Engine {
    fn live(&self) -> Result<&Live, &'static str> {
        self.live.as_ref().ok_or(NOT_REMOTE)
    }

    /// A method without a result that the remote engine cannot do yet: the window is told.
    fn unsupported(&self) {
        let _ = self
            .tx
            .unbounded_send(UiMsg::BadEvent(NOT_REMOTE.to_string()));
    }

    /// Stops a background process of this session (the Terminal tab's Kill button). The runtime knows only this
    /// session's processes, so another id is refused. `Ok(true)`: it was running and got the signal; `Ok(false)`:
    /// it had already exited, nothing was sent.
    pub fn kill_process(&self, process_id: &str) -> Result<bool, String> {
        let live = self.live()?;
        let err = |e: synthesis_terminal::Error| sanitize_for_display(&e.to_string());
        if !live
            .rt
            .get_status(&self.sid, process_id)
            .map_err(err)?
            .running
        {
            return Ok(false);
        }
        live.rt
            .signal(&self.sid, process_id, synthesis_terminal::Signal::Kill)
            .map_err(err)?;
        Ok(true)
    }

    /// Tests and tools: defaults plus a model, provider and sandbox mode. Blocking, like `start_with`.
    #[cfg(any(feature = "shot", feature = "local-engine-for-tests"))]
    pub(crate) fn start(
        project: &Path,
        env: &Env,
        model: Option<&str>,
        provider: Option<&str>,
        sandbox: Option<&str>,
    ) -> Result<(Self, futures::channel::mpsc::UnboundedReceiver<UiMsg>), String> {
        // `sandbox` is `--sandbox`'s value (enforce|best-effort|off); `None` keeps the default (enforce).
        let mut args = vec!["synthesis-desktop"];
        args.extend(sandbox.into_iter().flat_map(|m| ["--sandbox", m]));
        let mut common = Defaults::try_parse_from(args).map_err(|e| e.to_string())?.c;
        common.model = model.map(str::to_string);
        common.provider = provider.map(str::to_string);
        Self::start_with(
            StartOpts {
                project: project.to_path_buf(),
                common,
                resume: None,
                notes: vec![],
                verify: None,
            },
            env,
        )
    }

    /// Starts the session in a child process that speaks `synt engine --stdio` (the engine of another
    /// OS or machine). Blocking: call it from a worker thread. The window gets the same [`UiMsg`]s.
    pub fn start_remote(
        launcher: &crate::remote::Launcher,
        project: &Path,
        spec: &crate::remote::StartSpec,
    ) -> Result<(Self, futures::channel::mpsc::UnboundedReceiver<UiMsg>), String> {
        let (tx, rx) = unbounded();
        let (remote, v) = crate::remote::Remote::connect(launcher, project, tx.clone(), spec)?;
        Ok((Self::from_report(remote, &v, project, tx), rx))
    }

    /// The window's view of a remote session from the engine's start report.
    fn from_report(
        remote: crate::remote::Remote,
        v: &Value,
        project: &Path,
        tx: UnboundedSender<UiMsg>,
    ) -> Self {
        let text = |k: &str| v[k].as_str().map(sanitize_for_display).unwrap_or_default();
        let list = |k: &str| {
            v[k].as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(Value::as_str)
                        .map(sanitize_for_display)
                        .collect()
                })
                .unwrap_or_default()
        };
        Self {
            sid: text("sid"),
            label: text("label"),
            models: list("models"),
            model_rows: vec![],
            stale_model: v["stale_model"].as_str().map(sanitize_for_display),
            context_source: String::new(),
            effort_tiers: list("effort_tiers"),
            notices: list("notices"),
            // `cwd` and `root` are what the host opens files by (the file tree, trust fingerprint, memory,
            // workspace key): the folder the person chose on this machine, never a string the engine sent
            cwd: project.to_string_lossy().into_owned(),
            root: project.to_string_lossy().into_owned(),
            remote_cwd: Some(text("cwd")),
            remote_root: Some(text("root")),
            remote_distro: None,
            trust: crate::remote::TrustState::parse(v["trust"].clone()).ok(),
            commands: vec![crate::live::reveal_command()],
            context_window: v["context_window"].as_u64(),
            // the engine decided it (the person's choice went through `trust.set`); display state only
            trusted: v["trusted"].as_bool() == Some(true),
            synthesis_home: PathBuf::new(),
            // the desktop's own environment is not the engine's: nothing is read here
            env: Env::from_pairs::<&str, &str>([]),
            live: None,
            remote: Some(remote),
            tx,
            sandbox: Default::default(),
            memory_on: false,
            memory_report: vec![],
            // empty paths: a `#` note is refused, never written on this machine
            notes: synthesis_app::memory::Notes {
                user: PathBuf::new(),
                project: PathBuf::new(),
                root: PathBuf::new(),
                secrets: vec![],
                loaded: vec![],
                long_term: false,
            },
            followup: Arc::default(),
        }
    }

    /// Ends this session and starts another one in the same engine process (`trust.set`, `model.set`,
    /// `session.*`, `workspace.close` of the own folder). Blocking. The window gets a new receiver; `Ok(note)`
    /// when the engine refused and the session goes on as it was. `Err`: the engine is gone.
    pub fn restart_remote(
        mut self,
        call: synthesis_app::proto::Call,
    ) -> Result<
        (
            Self,
            futures::channel::mpsc::UnboundedReceiver<UiMsg>,
            Option<String>,
            Value,
        ),
        String,
    > {
        let Some(remote) = self.remote.take() else {
            return Err(NOT_REMOTE.into());
        };
        let (tx, rx) = unbounded();
        match remote.restart(call, tx.clone()) {
            Ok(v) => {
                let project = PathBuf::from(&self.cwd);
                let distro = self.remote_distro.take();
                let mut e = Self::from_report(remote, &v, &project, tx);
                e.remote_distro = distro;
                Ok((e, rx, None, v))
            }
            // the engine refused before ending anything: the same session continues on the new channel
            Err(e) if e.kept => {
                self.remote = Some(remote);
                self.tx = tx;
                Ok((self, rx, Some(sanitize_for_display(&e.why)), Value::Null))
            }
            // anything else: the engine is dropped (killed), never kept
            Err(e) => Err(sanitize_for_display(&e.why)),
        }
    }

    /// Builds the harness and starts (or resumes) the session, like `prepare` + `round` in
    /// `apps/cli/src/tui.rs`. Blocking: call it from a worker thread, never from inside a tokio runtime.
    pub(crate) fn start_with(
        o: StartOpts,
        env: &Env,
    ) -> Result<(Self, futures::channel::mpsc::UnboundedReceiver<UiMsg>), String> {
        let StartOpts {
            project,
            mut common,
            resume,
            notes,
            verify,
        } = o;
        let (tx, rx) = unbounded();
        let asks = Asks::default();
        // the stored session continues in place; the picker may name a session of another
        // folder, which is why the workspace check is off
        common.resume = resume.as_ref().map(|r| r.record.id.clone());
        common.continue_session = false;
        common.resume_any_workspace = resume.is_some();
        let trusted = common.trust_project;
        let cleanup_worktree = common.cleanup_worktree;
        let notify: Notify = {
            let tx = tx.clone();
            Arc::new(move |a| {
                tx.unbounded_send(match a {
                    Ask::Approval(a) => UiMsg::Approval(a),
                    Ask::Input(a) => UiMsg::Input(a),
                    Ask::Withdraw(id) => UiMsg::Withdraw(id),
                })
                .is_ok()
            })
        };
        let native = Native {
            out: Box::new(Lines {
                tx: tx.clone(),
                buf: vec![],
            }),
            approver: asks.approver(notify.clone()),
            input: asks.input(notify),
            // the approved configuration is checked before the project's files are read and before any MCP
            // server starts (hooks run later still, after the check that follows `prepare`)
            pre_start: verify.clone().map(|v| {
                Arc::new(move || v.verify().map_err(|files| trust_changed_error(&files)))
                    as synthesis_app::run::PreStart
            }),
        };
        let t_prepare = crate::trace::span(
            env,
            "start_with: prepare (config, MCP, memory, skills, store)",
        );
        let p = prepare(
            &common,
            &project,
            env,
            &Limits::default(),
            Front::Native(native),
        )?;
        // the configuration is loaded now and no hook has run yet: it must still be what the person approved
        if let Some(Err(files)) = verify.as_ref().map(TrustCheck::verify) {
            return Err(trust_changed_error(&files));
        }
        drop(t_prepare);
        let _t = crate::trace::span(env, "start_with: session start + hooks");
        // Started here (not by the app) so the session hooks follow `session.started`.
        let (live, info) = Live::begin(
            p,
            asks,
            resume.map(|r| r.history).unwrap_or_default(),
            cleanup_worktree,
        )?;
        let mcp = live.mcp.clone();
        let commands: Vec<Command> = synthesis_cli_ui::local_commands()
            .into_iter()
            // desktop only: a typed folder path as a new workspace
            .chain(std::iter::once(Command {
                name: "open".into(),
                aliases: vec![],
                description: "open a folder as a new workspace".into(),
                argument_hint: "<path>".into(),
                local: true,
            }))
            .chain(std::iter::once(Command {
                name: "context".into(),
                aliases: vec![],
                description: "set (or unset) the context window of the model in use".into(),
                argument_hint: "<tokens|unset>".into(),
                local: true,
            }))
            .chain([crate::live::send_command(), crate::live::reveal_command()])
            .chain(std::iter::once(crate::live::rewind_command()))
            .chain(std::iter::once(crate::live::clean_command()))
            .chain(std::iter::once(crate::live::close_workspace_command()))
            .chain(
                info.skills
                    .iter()
                    .map(|s| {
                        (
                            s.name.clone(),
                            s.description.clone(),
                            "skill",
                            String::new(),
                        )
                    })
                    .chain(mcp.iter().flat_map(|m| m.prompts()).map(|p| {
                        let name = format!(
                            "mcp__{}__{}",
                            synthesis_app::mcpinput::sanitize_server(&p.server),
                            p.name
                        );
                        let hint = p
                            .arguments
                            .iter()
                            .map(|(n, _)| n.as_str())
                            .collect::<Vec<_>>();
                        (name, p.description, "MCP prompt", hint.join(" "))
                    }))
                    .map(|(name, desc, kind, hint)| Command {
                        name: sanitize_for_display(&name),
                        aliases: vec![],
                        description: sanitize_for_display(&format!("{kind}: {desc}")),
                        argument_hint: sanitize_for_display(&hint),
                        local: false,
                    }),
            )
            .collect();
        let mut all_notices = info.notices;
        all_notices.extend(notes);
        let t_note = crate::trace::span(env, "start_with: window_note");
        let context_source = crate::models::window_note(env, &project, &info.label);
        drop(t_note);
        let t_rows = crate::trace::span(env, "start_with: models::rows");
        let model_rows = crate::models::rows(env, &project);
        drop(t_rows);
        let engine = Self {
            sid: live.sid.clone(),
            label: info.label,
            models: info.models,
            model_rows,
            stale_model: info.stale_model,
            context_source,
            effort_tiers: info.effort_tiers,
            notices: all_notices,
            cwd: info.cwd,
            root: info.root,
            commands,
            context_window: info.context_window,
            trusted,
            remote_cwd: None,
            remote_root: None,
            remote_distro: None,
            trust: None,
            synthesis_home: info.synthesis_home,
            env: env.clone(),
            live: Some(live),
            remote: None,
            tx,
            sandbox: info.sandbox,
            memory_on: info.memory_on,
            memory_report: (info.notes.clone().hook().report)(),
            notes: info.notes,
            followup: Arc::default(),
        };
        Ok((engine, rx))
    }

    pub fn mcp(&self) -> Option<&Arc<synthesis_mcp::McpManager>> {
        self.live.as_ref()?.mcp.as_ref()
    }

    /// State of every configured MCP server (the panel's rows).
    pub fn mcp_status(&self) -> Vec<McpInfo> {
        self.mcp().map_or_else(Vec::new, |m| {
            m.status()
                .into_iter()
                .map(|s| McpInfo {
                    name: sanitize_for_display(&s.name),
                    state: format!("{:?}", s.state).to_lowercase(),
                    tools: s.tool_count,
                })
                .collect()
        })
    }

    /// `(path, branch)` of the worktree this session works in (`--worktree`).
    pub fn worktree_info(&self) -> Option<(String, String)> {
        let i = &self.live.as_ref()?.worktree.as_ref()?.info;
        Some((i.worktree.display().to_string(), i.branch.clone()))
    }

    /// Rows of the sessions other processes advertise (`/peers`); `None` when peers are off.
    /// Blocking: call it off the UI thread.
    pub fn peer_rows(&self) -> Option<Vec<Value>> {
        let live = self.live.as_ref()?;
        live.harness.peer_summary()?;
        live.tokio
            .as_ref()
            .and_then(|rt| rt.block_on(live.harness.peer_rows(true)))
    }

    /// `/send`: the user's message to another session's main agent; the reply text, or why not.
    /// Blocking (waits for the answer): call it off the UI thread.
    pub fn peer_ask(&self, to: &str, text: &str) -> Result<String, String> {
        let live = self.live()?;
        let rt = live.tokio.as_ref().ok_or("peers are off")?;
        rt.block_on(live.harness.peer_ask_user(to, text))
    }

    /// Runs `f` against the remote engine on a worker thread (never the UI thread: a store call can take
    /// seconds); the message it returns reaches the window like any other. Bounded by the call's own timeout.
    pub fn spawn_remote(
        self: &Arc<Self>,
        f: impl FnOnce(&crate::remote::Remote) -> Option<UiMsg> + Send + 'static,
    ) {
        let e = self.clone();
        std::thread::spawn(move || {
            if let Some(m) = e.remote().and_then(f) {
                let _ = e.tx.unbounded_send(m);
            }
        });
    }

    /// The remote engine's handle: its store, trust and restarts (`None`: the session runs in this process).
    pub fn remote(&self) -> Option<&crate::remote::Remote> {
        self.remote.as_ref()
    }

    /// The session runs in a child process (see `remote.rs`).
    pub fn is_remote(&self) -> bool {
        self.remote.is_some()
    }

    /// The permission mode in force: the engine's own, as it reported it for a remote engine.
    pub fn mode(&self) -> Mode {
        match (&self.live, &self.remote) {
            (Some(l), _) => l.mode(),
            (_, Some(r)) => r.mode(),
            _ => Mode::Default,
        }
    }

    pub fn set_mode(&self, mode: Mode) {
        match (&self.live, &self.remote) {
            (Some(l), _) => {
                let _ = l.harness.set_mode(mode, false);
            }
            (_, Some(r)) => {
                if let Err(e) = r.set_mode(mode) {
                    let _ = self.tx.unbounded_send(UiMsg::BadEvent(e));
                }
            }
            _ => {}
        }
    }

    /// Enters bypassPermissions after the person said yes to the confirmation dialog; the core still takes the
    /// flag from here only and answers with its own error otherwise.
    pub fn enable_bypass(&self) -> Result<(), String> {
        self.live()?
            .harness
            .set_mode(Mode::BypassPermissions, true)
            .map_err(|e| e.to_string())
    }

    pub fn set_effort(&self, effort: Option<String>) {
        match self.live() {
            Ok(l) => l.harness.set_reasoning_effort(effort),
            Err(_) => self.unsupported(),
        }
    }

    pub fn running(&self) -> bool {
        match (&self.live, &self.remote) {
            (Some(l), _) => l.running(),
            (_, Some(r)) => r.running(),
            _ => false,
        }
    }

    /// Starts a task. `Err`: one is already running.
    pub fn send(&self, text: String) -> Result<(), &'static str> {
        self.send_images(text, vec![])
    }

    /// [`Engine::send`] with pasted images (the harness keeps the 2 newest messages' images in a
    /// request and drops them for a model without vision).
    pub fn send_images(
        &self,
        text: String,
        images: Vec<ImageAttachment>,
    ) -> Result<(), &'static str> {
        match (&self.remote, images.is_empty()) {
            (Some(r), true) => r.send(text),
            (Some(_), false) => Err(NOT_REMOTE),
            _ => self.start_task(text, images, false),
        }
    }

    /// The model turn after a `!cmd` (see [`Engine::take_followup`]): the harness's own note,
    /// not shown in the chat.
    pub fn send_followup(&self) -> Result<(), &'static str> {
        self.start_task(synthesis_harness::user_command_note().into(), vec![], true)
    }

    fn start_task(
        &self,
        text: String,
        images: Vec<ImageAttachment>,
        hidden: bool,
    ) -> Result<(), &'static str> {
        let tx = self.tx.clone();
        self.live()?.start_task(text, images, hidden, move |done| {
            let _ = tx.unbounded_send(UiMsg::TaskDone(done));
        })
    }

    /// Send now: delivers `text` to `to` (`main` or a sub-agent instance) at its next safe point
    /// (after the tool batch, never during an approval); `UiMsg::SendFailed` follows when refused.
    pub fn send_now(&self, to: String, text: String) -> Result<(), &'static str> {
        if is_command(&text) {
            return Err("commands are not sent now");
        }
        let live = self.live()?;
        let Some(rt) = &live.tokio else {
            return Err("the engine is closed");
        };
        let (h, tx) = (live.harness.clone(), self.tx.clone());
        rt.spawn(async move {
            if let Err(why) = h.send_user_message(&to, text.clone()).await {
                let _ = tx.unbounded_send(UiMsg::SendFailed { to, text, why });
            }
        });
        Ok(())
    }

    /// What the permission layer says about a tool call now. For tests only: not in release builds, hidden from
    /// the docs (the integration tests compile against the library with default features).
    #[doc(hidden)]
    #[cfg(debug_assertions)]
    pub fn check_permission(&self, tool: &str, input: &Value) -> synthesis_permissions::Decision {
        let Ok(live) = self.live() else {
            return synthesis_permissions::Decision::Deny;
        };
        lock(&live.harness.permissions())
            .check_current(tool, input)
            .decision
    }

    /// `!cmd` (`record_only`: `!!cmd`): the harness runs it through the same Bash permission check,
    /// sandbox, hooks and redaction as the CLI's, and appends it with its output to the history.
    /// Plan mode refuses it (stricter than the CLI, which lets a typed command through in plan).
    /// `TaskDone` follows. `Err`: a task is running, or the command was refused before it started.
    pub fn user_command(&self, command: String, record_only: bool) -> Result<(), &'static str> {
        let live = self.live()?;
        if self.mode() == Mode::Plan {
            return Err("plan mode does not run shell commands: Shift+Tab leaves it first");
        }
        let Some(rt) = &live.tokio else {
            return Err("the engine is closed");
        };
        let (session, token) = live.take_session()?;
        let (h, tx) = (live.harness.clone(), self.tx.clone());
        let (slot, history, followup) = (
            live.session.clone(),
            live.history.clone(),
            self.followup.clone(),
        );
        rt.spawn(async move {
            let before = lock(&history).clone();
            let done = match h.run_user_command(&session, before, &command, token).await {
                Ok((messages, out)) => {
                    *lock(&history) = messages;
                    // the model reads the result unless `!!`, the settings or Esc say not to
                    followup.store(
                        out.ran
                            && !out.cancelled
                            && !record_only
                            && h.shell_settings().user_commands_model_turn,
                        std::sync::atomic::Ordering::SeqCst,
                    );
                    Ok(())
                }
                Err(e) => Err(e.to_string()),
            };
            *lock(&slot) = Some(session);
            let _ = tx.unbounded_send(UiMsg::TaskDone(done));
        });
        Ok(())
    }

    /// True once after a `!cmd` whose output the model should read.
    pub fn take_followup(&self) -> bool {
        self.followup
            .swap(false, std::sync::atomic::Ordering::SeqCst)
    }

    /// An image the person dropped: its resolved path, when the session may read it. The CLI's
    /// `@file` rule (`WorkspaceContext::resolve`): inside the workspace or an additional
    /// directory with links resolved (a link out is outside), never a secret file, no read-deny
    /// rule. Read the returned path, not the one given.
    pub fn image_path(&self, p: &Path) -> Result<PathBuf, String> {
        let live = self.live()?;
        let real = live
            .host
            .workspace
            .resolve(&p.to_string_lossy())
            .map_err(|_| {
                "outside the workspace: only files inside it can be attached".to_string()
            })?;
        let real = synthesis_core::canonicalize(&real).map_err(|e| e.to_string())?;
        if synthesis_core::secrets::is_secret_path(p)
            || synthesis_core::secrets::is_secret_path(&real)
        {
            return Err("a secret file is never attached".into());
        }
        let denied = lock(&live.harness.permissions())
            .check(
                "Read",
                &json!({ "file_path": real }),
                Mode::BypassPermissions,
            )
            .decision
            == synthesis_permissions::Decision::Deny;
        if denied {
            return Err("blocked by a permission rule".into());
        }
        Ok(real)
    }

    /// `/clear`: the next task starts without the earlier conversation.
    pub fn clear_history(self: &Arc<Self>) {
        match (self.live(), &self.remote) {
            (Ok(l), _) => lock(&l.history).clear(),
            (_, Some(_)) => self.spawn_remote(|r| r.clear().err().map(|e| UiMsg::Note(e, true))),
            _ => self.unsupported(),
        }
    }

    /// `/compact`. While a task runs the harness compacts at its next safe point (`Ok(true)`);
    /// otherwise the conversation between tasks is compacted now and [`UiMsg::Compacted`] follows
    /// (`Ok(false)`). `Err`: nothing to compact.
    pub fn compact(self: &Arc<Self>, instructions: Option<String>) -> Result<bool, &'static str> {
        if self.remote.is_some() {
            // asked in the background: `Compacted` follows (or `CompactQueued` while a task runs, or the error)
            self.spawn_remote(move |r| match r.compact(instructions) {
                Ok(true) => Some(UiMsg::CompactQueued),
                Ok(false) => None,
                Err(e) => Some(UiMsg::Compacted(Err(e))),
            });
            return Ok(false);
        }
        let live = self.live()?;
        if live.running() {
            live.harness.request_compact(instructions);
            return Ok(true);
        }
        let Some(session) = lock(&live.session).clone() else {
            return Err("a task is running");
        };
        let history = lock(&live.history).clone();
        let Some(rt) = live.tokio.as_ref().filter(|_| !history.is_empty()) else {
            return Err("nothing to compact yet");
        };
        let (h, slot, tx) = (live.harness.clone(), live.history.clone(), self.tx.clone());
        rt.spawn(async move {
            let r = h
                .compact_now(&session, history, instructions, CancellationToken::new())
                .await
                .map(|m| *lock(&slot) = m);
            let _ = tx.unbounded_send(UiMsg::Compacted(r));
        });
        Ok(false)
    }

    /// `/agents`: this session's agents and the types `@agent:name` starts.
    pub fn agents_report(&self) -> Vec<String> {
        let Ok(live) = self.live() else {
            return vec![NOT_REMOTE.into()];
        };
        let mut v = vec!["Agents in this session (@name routes a line):".to_string()];
        v.extend(live.harness.agents().into_iter().map(|a| {
            let q = if a.queued > 0 {
                format!(" \u{b7} {} queued", a.queued)
            } else {
                String::new()
            };
            sanitize_for_display(&format!(
                "  @{} \u{b7} {} \u{b7} {}{q}",
                a.name, a.kind, a.status
            ))
        }));
        v.push("Types @agent:<type> starts (Task tool and chat):".to_string());
        v.extend(
            live.harness
                .agent_defs()
                .into_iter()
                .map(|(n, d)| sanitize_for_display(&format!("  {n} \u{b7} {d}"))),
        );
        v
    }

    /// `/permissions`: the mode and the rules in force.
    pub fn permissions_report(&self) -> Vec<String> {
        let Ok(live) = self.live() else {
            return vec![NOT_REMOTE.into()];
        };
        let perms = live.harness.permissions();
        let p = lock(&perms);
        let r = p.rules();
        let mut v = vec![format!("mode: {}", p.mode().as_str())];
        for (k, rules) in [("allow", &r.allow), ("ask", &r.ask), ("deny", &r.deny)] {
            v.push(format!("{k} rules ({}):", rules.len()));
            v.extend(
                rules
                    .iter()
                    .map(|x| format!("  {}", sanitize_for_display(x))),
            );
        }
        v
    }

    /// The rules of this session (allow, ask, deny), for the `/permissions` remove picker.
    pub fn permission_rules(&self) -> Vec<String> {
        let Ok(live) = self.live() else {
            return vec![];
        };
        let perms = live.harness.permissions();
        let p = lock(&perms);
        let r = p.rules();
        let mut all: Vec<String> = [&r.allow, &r.ask, &r.deny]
            .into_iter()
            .flatten()
            .cloned()
            .collect();
        all.dedup();
        all
    }

    /// `/permissions allow RULE`: a session-only rule, the CLI's check and wording (`Err`: an error line).
    pub fn allow_rule(&self, rule: &str) -> Result<String, String> {
        let live = self.live()?;
        if !synthesis_permissions::Permissions::is_well_formed_rule(rule) {
            return Err(
                "expected a rule like Bash(cargo test:*), Edit or mcp__server__tool".into(),
            );
        }
        if !lock(&live.harness.permissions()).add_allow(rule) {
            return Err("that allow rule already exists".into());
        }
        Ok(format!(
            "allowed for this session: {}",
            sanitize_for_display(rule)
        ))
    }

    /// `/permissions remove RULE`: from allow, ask and deny of this session only.
    pub fn remove_rule(&self, rule: &str) -> Result<String, String> {
        let live = self.live()?;
        let shown = sanitize_for_display(rule);
        match lock(&live.harness.permissions()).remove_rule(rule) {
            0 => Err(format!("no rule {shown}")),
            n => Ok(format!("removed {n} rule(s): {shown}")),
        }
    }

    /// `/rename TITLE`: validates and stores the title of this session; `Ok` is the stored title.
    pub fn rename(&self, title: &str) -> Result<String, String> {
        match &self.live()?.host.store {
            Some(s) => synthesis_app::rename::rename(s, title),
            None => Err("/rename: this session is not stored (--no-session-store)".into()),
        }
    }

    /// `/rename` for a remote engine, in the background: `UiMsg::Renamed` follows.
    pub fn rename_remote(self: &Arc<Self>, title: &str) {
        let (sid, title) = (self.sid.clone(), title.to_string());
        self.spawn_remote(move |r| {
            let done = r.rename(&sid, &title);
            Some(UiMsg::Renamed(sid, done))
        });
    }

    /// `/name`: the name other sessions address this one by; `None` when peers are off.
    pub fn peer_name(&self) -> Option<Option<String>> {
        self.live.as_ref()?.harness.peer_summary().map(|(n, _)| n)
    }

    /// `/mcp`: one block per server with state, transport, tools and the last error.
    pub fn mcp_report(&self) -> Vec<String> {
        if self.live.is_none() {
            return vec![NOT_REMOTE.into()];
        }
        let Some(m) = self.mcp() else {
            return vec!["MCP: no servers are configured for this session".into()];
        };
        let status = m.status();
        if status.is_empty() {
            return vec!["MCP: no servers are configured for this session".into()];
        }
        let clean = sanitize_for_display;
        let tools = m.tools();
        let mut v = vec![format!("MCP servers ({}):", status.len())];
        for s in status {
            let state = format!("{:?}", s.state).to_lowercase();
            v.push(format!(
                "  {} \u{b7} {state} \u{b7} {} \u{b7} {} tools",
                clean(&s.name),
                s.transport,
                s.tool_count
            ));
            if let Some(e) = &s.error {
                v.push(format!("    error: {}", clean(e)));
            }
            for t in tools.iter().filter(|t| t.server == s.name && !t.synthetic) {
                v.push(format!(
                    "    {} \u{b7} {}",
                    clean(&t.tool),
                    clean(t.description.lines().next().unwrap_or(""))
                ));
            }
        }
        v
    }

    pub fn cancel(&self) {
        match (&self.live, &self.remote) {
            (Some(l), _) => l.cancel(),
            (_, Some(r)) => r.cancel(),
            _ => {}
        }
    }

    /// Answers the approval `id`; false when it is no longer waiting.
    pub fn answer(&self, id: &str, choice: Choice) -> bool {
        match (&self.live, &self.remote) {
            (Some(l), _) => l.asks.answer(id, choice),
            (_, Some(r)) => r.answer(id, choice),
            _ => false,
        }
    }

    /// Answers the plan or question `id` (`None`: nobody answers); false when it is gone.
    pub fn answer_input(&self, id: &str, reply: Option<UserReply>) -> bool {
        match (&self.live, &self.remote) {
            (Some(l), _) => l.asks.answer_input(id, reply),
            (_, Some(r)) => r.answer_input(id, reply),
            _ => false,
        }
    }

    /// Ends the session like the CLI host does (`round`): the `SessionEnd` hook, the stored usage,
    /// `session.completed`, the MCP servers, the worktree; then stops the runtime. Returns what the
    /// worktree finish found (path, branch, changes, cleanup), for the caller to show. A remote engine is
    /// asked to close and, if it does not within a few seconds, killed.
    pub fn close(mut self) -> Option<String> {
        if let Some(r) = self.remote.take() {
            return r.close();
        }
        self.live.take()?.close()
    }
}

/// Ends an engine other threads may still hold for a moment (a peer poll): waits for them, then
/// [`Engine::close`]. Blocking; call it off the UI thread.
/// Another thread (a background call to the engine, a peer poll) still holds this engine: it must not be
/// restarted or closed now (the window says "busy, try again" instead of dropping it).
pub fn in_use(e: &Arc<Engine>) -> bool {
    Arc::strong_count(e) > 1
}

/// Takes an engine other threads may still hold for a moment (a peer poll) out of its `Arc`.
pub fn take_shared(mut e: Arc<Engine>) -> Result<Engine, String> {
    for _ in 0..250 {
        match Arc::try_unwrap(e) {
            Ok(e) => return Ok(e),
            Err(back) => {
                e = back;
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
        }
    }
    Err("the engine is still in use; try again".into())
}

pub fn close_shared(mut e: Arc<Engine>) -> Option<String> {
    for _ in 0..250 {
        match Arc::try_unwrap(e) {
            Ok(e) => return e.close(),
            Err(back) => {
                e = back;
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
        }
    }
    eprintln!("synthesis-desktop: the engine is still in use; its session was not closed cleanly");
    None
}

#[cfg(test)]
mod trust_tests {
    use super::*;

    #[test]
    fn roots_that_must_not_be_trusted_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        let base = synthesis_core::canonicalize(dir.path()).unwrap();
        let (home, synt) = (base.join("home"), base.join("home/.synt"));
        std::fs::create_dir_all(&synt).unwrap();
        let proj = home.join("work/app");
        std::fs::create_dir_all(&proj).unwrap();
        let why = |r: &Path| trust_refusal(r, Some(&home), &synt);
        assert!(why(&proj).is_none());
        assert!(why(&home).unwrap().contains("home folder"));
        assert!(
            why(&base).unwrap().contains("home folder"),
            "a parent of home"
        );
        assert!(why(&synt).unwrap().contains("config folder"));
        assert!(
            why(&synt.join("sessions"))
                .unwrap()
                .contains("config folder")
        );
        // a filesystem root, spelled the way this platform spells it (`/` or `C:\`)
        let fs_root = base.ancestors().last().unwrap();
        assert!(why(fs_root).unwrap().contains("filesystem root"));
        // an unknown home fails closed
        assert!(
            trust_refusal(&proj, None, &synt)
                .unwrap()
                .contains("home folder is unknown")
        );
    }

    #[test]
    fn a_remembered_trust_is_honoured_only_while_unchanged_enforcing_and_checked() {
        use synthesis_session::trust;
        let dir = tempfile::tempdir().unwrap();
        let base = synthesis_core::canonicalize(dir.path()).unwrap();
        let (home, uhome, proj, sib, sub) = (
            base.join("home"),
            base.join("uhome"),
            base.join("w/app"),
            base.join("w/app2"),
            base.join("w/app/sub"),
        );
        for d in [&home, &uhome, &proj, &sib, &sub] {
            std::fs::create_dir_all(d).unwrap();
        }
        let policy = trust_policy(Some(&uhome), &home);
        let c = Defaults::try_parse_from(["x", "--sandbox", "enforce", "--approve-mcp", "db"])
            .unwrap()
            .c;
        let go = |c: &Common, p: &Path| {
            let (c, n, _) =
                launch_common(c, Path::new("/none"), None, true, p, &home, Some(&uhome));
            (c, n)
        };
        assert!(!go(&c, &proj).0.trust_project, "nothing remembered yet");
        let fp = |p: &Path| trust::fingerprint(p, p).unwrap();
        trust::add(&home, &proj, trust::FOLDER, &fp(&proj), &policy).unwrap();
        let (t, notes) = go(&c, &proj);
        assert!(t.trust_project && t.approve_mcp.is_empty() && notes.is_empty());
        assert!(!go(&c, &sib).0.trust_project, "a sibling is not trusted");
        assert!(
            !go(&c, &sub).0.trust_project,
            "a folder grant is not inherited by a subfolder"
        );
        // the configuration changed since: NOT honoured, the files are named, the folder starts untrusted
        std::fs::create_dir_all(proj.join(".synt")).unwrap();
        std::fs::write(proj.join(".synt/settings.json"), "{\"hooks\":{}}").unwrap();
        let (t, notes) = go(&c, &proj);
        assert!(!t.trust_project);
        assert!(
            notes.len() == 1
                && notes[0].contains(".synt/settings.json")
                && notes[0].contains("untrusted"),
            "{notes:?}"
        );
        match remembered_trust(&home, Some(&uhome), &proj, false) {
            Remembered::Changed { files, .. } => assert_eq!(files, [".synt/settings.json"]),
            o => panic!("{o:?}"),
        }
        // trusting it again records the new manifest: honoured again
        trust::add(&home, &proj, trust::FOLDER, &fp(&proj), &policy).unwrap();
        assert!(go(&c, &proj).0.trust_project);
        // a sandbox that is not enforcing disables remembered trust (and says so); the launch flag is untouched
        let off = Defaults::try_parse_from(["x", "--sandbox", "off"])
            .unwrap()
            .c;
        let (t, notes) = go(&off, &proj);
        assert!(
            !t.trust_project && notes.iter().any(|n| n.contains("not honoured")),
            "{notes:?}"
        );
        let best = Defaults::try_parse_from(["x", "--sandbox", "best-effort"])
            .unwrap()
            .c;
        assert!(!go(&best, &proj).0.trust_project);
        let flag = Defaults::try_parse_from(["x", "--sandbox", "off", "--trust-project"])
            .unwrap()
            .c;
        assert!(
            launch_common(
                &flag,
                &root_of(&proj),
                None,
                true,
                &proj,
                &home,
                Some(&uhome)
            )
            .0
            .trust_project
        );
        // without the key the rows cannot be verified: nothing is honoured
        let key = home.join("run/trust.key");
        let kept = std::fs::read(&key).unwrap();
        std::fs::remove_file(&key).unwrap();
        assert!(!go(&c, &proj).0.trust_project);
        std::fs::write(&key, kept).unwrap();
        assert!(go(&c, &proj).0.trust_project);
        // a revoked root trusts nothing again
        trust::revoke(&home, &proj.to_string_lossy()).unwrap();
        assert!(!go(&c, &proj).0.trust_project);
        // a parent grant covers what is inside it: each project is baselined at its first start, then checked
        let parent = base.join("w");
        trust::add(&home, &parent, trust::PARENT, "", &policy).unwrap();
        assert!(go(&c, &proj).0.trust_project && go(&c, &sib).0.trust_project);
        assert!(!go(&c, &base.join("elsewhere")).0.trust_project);
        std::fs::write(sib.join(".mcp.json"), "{}").unwrap();
        let (t, notes) = go(&c, &sib);
        assert!(
            !t.trust_project && notes[0].contains(".mcp.json"),
            "{notes:?}"
        );
        assert!(
            go(&c, &proj).0.trust_project,
            "the other project is unchanged"
        );
    }

    #[test]
    fn the_parent_refusal_covers_containers_and_the_refusals_cover_home() {
        let dir = tempfile::tempdir().unwrap();
        let base = synthesis_core::canonicalize(dir.path()).unwrap();
        let (home, synt) = (base.join("h"), base.join("h/.synt"));
        std::fs::create_dir_all(home.join("Downloads")).unwrap();
        std::fs::create_dir_all(home.join("src")).unwrap();
        std::fs::create_dir_all(&synt).unwrap();
        let p = |d: &Path| parent_refusal(d, Some(&home), &synt);
        assert!(p(&home.join("src")).is_none());
        assert!(p(&home.join("Downloads")).unwrap().contains("catch-all"));
        // the temp folder is refused (as a temporary folder, or, when this test's home lies inside it, as a
        // parent of home: either reason keeps it from being a parent grant)
        let why_tmp = p(&std::env::temp_dir()).unwrap();
        assert!(
            why_tmp.contains("temporary") || why_tmp.contains("home folder"),
            "{why_tmp}"
        );
        #[cfg(unix)]
        assert!(p(Path::new("/tmp")).is_some());
        let fs_root = base.ancestors().last().unwrap();
        assert!(p(&home).is_some() && p(fs_root).is_some());
        // the root rules still allow a folder grant in Downloads
        assert!(trust_refusal(&home.join("Downloads/proj"), Some(&home), &synt).is_none());
    }

    #[test]
    fn a_session_trust_drops_the_mcp_approvals_of_the_launch() {
        let c = Defaults::try_parse_from([
            "x",
            "--approve-mcp",
            "db",
            "--approve-mcp",
            "web@abcdef1234",
        ])
        .unwrap()
        .c;
        assert!(!c.trust_project && c.approve_mcp.len() == 2);
        let t = session_trusted(&c);
        assert!(t.trust_project && t.approve_mcp.is_empty());
        // the root named must be the project's: another project does not inherit the grant
        let dir = tempfile::tempdir().unwrap();
        let (a, b) = (dir.path().join("a"), dir.path().join("b"));
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();
        let root = root_of(&a);
        assert!(
            launch_common(
                &c,
                Path::new("/none"),
                Some(&(root.clone(), None, true)),
                true,
                &a,
                dir.path(),
                None
            )
            .0
            .trust_project
        );
        assert!(
            !launch_common(
                &c,
                Path::new("/none"),
                Some(&(root.clone(), None, true)),
                true,
                &b,
                dir.path(),
                None
            )
            .0
            .trust_project
        );
        assert!(
            !launch_common(&c, Path::new("/none"), None, true, &a, dir.path(), None)
                .0
                .trust_project
        );
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_lines_split_across_writes_and_a_broken_one_is_reported() {
        use std::io::Write as _;
        let (tx, mut rx) = unbounded();
        let mut w = Lines { tx, buf: vec![] };
        let good = r#"{"v":1,"seq":1,"ts":"2026-01-01T00:00:00Z","session_id":"s","task_id":null,"turn_id":null,"agent_id":null,"type":"notice","payload":{"level":"info","text":"hi"}}"#;
        w.write_all(&good.as_bytes()[..40]).unwrap();
        assert!(rx.try_recv().is_err(), "no newline yet");
        w.write_all(&good.as_bytes()[40..]).unwrap();
        w.write_all(b"\nnot json\n\n").unwrap();
        assert!(matches!(rx.try_recv().unwrap(), UiMsg::Event(e) if e.seq == 1));
        assert!(
            matches!(rx.try_recv().unwrap(), UiMsg::BadEvent(m) if m.contains("unreadable event"))
        );
        assert!(rx.try_recv().is_err());
    }
}

/// The integration tests start a local engine through these (feature `local-engine-for-tests`, a
/// dev-dependency of this crate only): in the product only `tui_view/driver.rs` may, behind the WSL gate.
#[cfg(feature = "local-engine-for-tests")]
pub mod for_tests {
    use super::*;

    pub fn start(
        project: &Path,
        env: &Env,
        model: Option<&str>,
        provider: Option<&str>,
        sandbox: Option<&str>,
    ) -> Result<(Engine, futures::channel::mpsc::UnboundedReceiver<UiMsg>), String> {
        Engine::start(project, env, model, provider, sandbox)
    }

    pub fn start_with(
        o: StartOpts,
        env: &Env,
    ) -> Result<(Engine, futures::channel::mpsc::UnboundedReceiver<UiMsg>), String> {
        Engine::start_with(o, env)
    }
}
