//! The real engine in this process (docs/specs/desktop.md section 1, E1): `synthesis_app::run::prepare`
//! builds the harness, the window talks to it through channels. It mirrors what `synt tui` does around
//! one prepared session (`apps/cli/src/tui.rs` `round` and its `Host`): the session start, the
//! `SessionStart`/`SessionEnd` hooks, skills, MCP prompts and mentions, the memory block, the stored
//! usage and the shutdown. Events arrive as typed envelopes, approvals and questions wait in maps until
//! the person answers, a task runs on the engine's own tokio runtime. The API key only lives in the
//! `Env` snapshot handed to `prepare`, never in the process env.
//!
//! Follow-up: `Host` and `end` are copies of the CLI's; both belong in `synthesis-app` so the two
//! front ends cannot drift.

use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use clap::Parser;
use futures::channel::mpsc::{UnboundedSender, unbounded};
use serde_json::{Value, json};
use synthesis_app::run::{Common, Front, Limits, Native, Prepared, finish_store, prepare};
use synthesis_config::Env;
use synthesis_core::{AgentSession, WorkspaceContext};
use synthesis_events::{Envelope, Event};
use synthesis_harness::{
    Approval, ApprovalRequest, Approver, BoxFut, EventSink, Harness, ImageAttachment,
    InputProvider, Scope, TaskOutcome, TaskRequest, UserReply, sanitize_for_display,
};
use synthesis_hooks::HookEvent;
use synthesis_permissions::{Mode, Suggestion};
use synthesis_providers::Message;
use synthesis_tools::NeedsUser;
use synthesis_tui::Resume;
use synthesis_tui_base::complete::Command;
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

/// What the engine sends to the window.
pub enum UiMsg {
    Event(Box<Envelope>),
    /// An event line this build cannot read: visible, not skipped (like the TUI).
    BadEvent(String),
    /// A permission ask; answer it with [`Engine::answer`].
    Approval(Box<AskApproval>),
    /// A plan or question dialog; answer it with [`Engine::answer_input`].
    Input(Box<AskInput>),
    /// The ask with this id is gone (cancelled, expired or answered).
    Withdraw(String),
    /// The task ended (`Err`: it could not run, or the session did not return to idle).
    TaskDone(Result<(), String>),
    /// `/compact` between tasks ended (`Err`: the reason, history unchanged).
    Compacted(Result<(), String>),
}

/// What the dialog shows of an [`ApprovalRequest`]. `suggestions` is the list the person can accept:
/// "allow always" never switches into bypassPermissions (that takes its own confirmation).
#[derive(Debug, Clone)]
pub struct AskApproval {
    pub id: String,
    pub agent_id: Option<String>,
    pub tool: String,
    pub input: Value,
    /// `permission` or `outside_read`.
    pub dialog: &'static str,
    pub prompt: String,
    pub reason: String,
    pub origin: Option<String>,
    pub suggestions: Vec<Suggestion>,
}

#[derive(Debug, Clone)]
pub struct AskInput {
    pub id: String,
    pub req: NeedsUser,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Choice {
    Once,
    Always,
    Deny,
}

const DENY_MESSAGE: &str = "The user denied this tool call. Do not retry it unchanged.";

/// Allow once carries no rule; Always carries the rules the dialog offered; anything else denies.
pub fn approval_for(choice: Choice, offered: &[Suggestion]) -> Approval {
    match choice {
        Choice::Once => Approval::allow(),
        Choice::Always => Approval {
            accepted: offered.to_vec(),
            ..Approval::allow()
        },
        Choice::Deny => Approval::deny(DENY_MESSAGE),
    }
}

struct Pending {
    reply: oneshot::Sender<Approval>,
    offered: Vec<Suggestion>,
}

type PendingMap = Arc<Mutex<HashMap<String, Pending>>>;
type InputMap = Arc<Mutex<HashMap<String, oneshot::Sender<Option<UserReply>>>>>;

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Tells the window to drop a dialog when its future is dropped (cancel or answered).
struct Withdraw {
    tx: UnboundedSender<UiMsg>,
    id: String,
}

impl Drop for Withdraw {
    fn drop(&mut self) {
        let _ = self
            .tx
            .unbounded_send(UiMsg::Withdraw(std::mem::take(&mut self.id)));
    }
}

struct ChanApprover {
    pending: PendingMap,
    tx: UnboundedSender<UiMsg>,
}

impl Approver for ChanApprover {
    fn approve<'a>(
        &'a self,
        req: ApprovalRequest,
        cancel: &'a CancellationToken,
    ) -> BoxFut<'a, Approval> {
        Box::pin(async move {
            let id = req.approval_id.clone();
            let offered: Vec<Suggestion> = req
                .suggestions
                .into_iter()
                .filter(|s| !matches!(s, Suggestion::SetMode(Mode::BypassPermissions)))
                .collect();
            let (reply, answer) = oneshot::channel();
            // registered before the window hears of it, so an answer can never land first
            lock(&self.pending).insert(
                id.clone(),
                Pending {
                    reply,
                    offered: offered.clone(),
                },
            );
            let ask = AskApproval {
                id: id.clone(),
                agent_id: req.agent_id,
                tool: req.tool,
                input: req.input,
                dialog: req.dialog,
                prompt: req.prompt,
                reason: req.reason,
                origin: req.origin,
                suggestions: offered,
            };
            if self
                .tx
                .unbounded_send(UiMsg::Approval(Box::new(ask)))
                .is_err()
            {
                lock(&self.pending).remove(&id);
                return Approval::auto_denied(); // the window is gone: nobody can approve
            }
            let _withdraw = Withdraw {
                tx: self.tx.clone(),
                id: id.clone(),
            };
            let r = tokio::select! {
                a = answer => a.unwrap_or_else(|_| Approval::auto_denied()),
                () = cancel.cancelled() => Approval {
                    reviewer: "policy",
                    ..Approval::deny("Cancelled before the approval was answered.")
                },
            };
            lock(&self.pending).remove(&id);
            r
        })
    }
}

/// Plan and question dialogs (`ExitPlanMode`, `AskUserQuestion`).
struct ChanInput {
    pending: InputMap,
    tx: UnboundedSender<UiMsg>,
}

impl InputProvider for ChanInput {
    fn ask<'a>(
        &'a self,
        req: &'a NeedsUser,
        cancel: &'a CancellationToken,
    ) -> BoxFut<'a, Option<UserReply>> {
        Box::pin(async move {
            use std::sync::atomic::{AtomicU64, Ordering};
            static N: AtomicU64 = AtomicU64::new(1);
            let id = format!("ask-{}", N.fetch_add(1, Ordering::Relaxed));
            let (reply, answer) = oneshot::channel();
            lock(&self.pending).insert(id.clone(), reply);
            let ask = AskInput {
                id: id.clone(),
                req: req.clone(),
            };
            if self.tx.unbounded_send(UiMsg::Input(Box::new(ask))).is_err() {
                lock(&self.pending).remove(&id);
                return None;
            }
            let _withdraw = Withdraw {
                tx: self.tx.clone(),
                id: id.clone(),
            };
            let r = tokio::select! {
                a = answer => a.unwrap_or(None),
                () = cancel.cancelled() => None,
            };
            lock(&self.pending).remove(&id);
            r
        })
    }
}

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
/// `synt tui` (`--model`, `--sandbox`, `--trust-project`, `--approve-mcp`, `--peers`, `--resume`, ...).
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

/// Canonical project root of `cwd` (what `--trust-project` was granted for).
pub fn root_of(cwd: &Path) -> PathBuf {
    let r = synthesis_settings::find_project_root(cwd);
    synthesis_core::canonicalize(&r).unwrap_or(r)
}

/// `--trust-project` / `--approve-mcp` were granted for ONE project root. After a switch to another
/// root they are dropped (untrusted, no re-prompt: relaunch with --cwd for that project).
pub fn common_for(c: &Common, granted: &Path, cwd: &Path) -> (Common, Option<String>) {
    let mut c = c.clone();
    let mut note = None;
    if c.trust_project && root_of(cwd) != granted {
        note = Some(format!(
            "project root changed; --trust-project and --approve-mcp apply only to {}, so this project is untrusted",
            sanitize_for_display(&granted.display().to_string())
        ));
        c.trust_project = false;
        c.approve_mcp.clear();
    }
    (c, note)
}

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
        (None, true) => synthesis_tui::list_saved(synthesis_home, Some(&start.to_string_lossy()))?
            .into_iter()
            .next()
            .map(|s| s.id)
            .ok_or("--continue: no saved session for this directory")?,
        (None, false) => return Ok(None),
    };
    load_resume_in_place(synthesis_home, &id).map(Some)
}

/// `load_resume`, and the session's folder must still exist.
pub fn load_resume_in_place(home: &Path, id: &str) -> Result<Resume, String> {
    let r = synthesis_tui::load_resume(home, id)?;
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
}

/// What `synt run` does around its task, for every task the person starts: MCP mentions and
/// prompts, skills, the memory block for the typed prompt, the memory episode afterwards and the
/// stored usage (the CLI's `Host`).
struct Host {
    mcp: Option<Arc<synthesis_mcp::McpManager>>,
    memory: Option<Arc<synthesis_app::memcmd::MemoryRun>>,
    context: Arc<synthesis_app::ctx::SynthesisContext>,
    harness: Arc<Harness>,
    sink: EventSink,
    store: Option<synthesis_app::events::Store>,
    transcript: Option<Arc<synthesis_app::transcript::Transcript>>,
    skills: Vec<String>,
    workspace: WorkspaceContext,
}

impl Host {
    /// `/skill-name args` runs that skill: the model is told to load it.
    fn skill_request(&self, typed: &str) -> Option<String> {
        let rest = typed.trim_start().strip_prefix('/')?;
        let (name, args) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
        self.skills.iter().any(|s| s == name).then(|| {
            let args = args.trim();
            if args.is_empty() {
                format!("Run the skill \"{name}\" with the Skill tool.")
            } else {
                format!("Run the skill \"{name}\" with the Skill tool. Arguments: {args}")
            }
        })
    }

    async fn before(&self, typed: &str, cancel: &CancellationToken) -> Result<String, String> {
        if let Some(r) = self.skill_request(typed) {
            return Ok(r);
        }
        let (request, notes) =
            synthesis_app::mcpinput::expand(self.mcp.as_ref(), typed, cancel).await?;
        let (request, files_missing) =
            synthesis_app::run::expand_file_mentions(&self.harness, &self.workspace, request);
        for n in notes.into_iter().chain(files_missing) {
            self.sink.emit(
                &Scope::default(),
                Event::Notice {
                    level: "warning".into(),
                    text: sanitize_for_display(&n),
                },
            );
        }
        if let Some(m) = &self.memory {
            self.context
                .set_memory(synthesis_app::memcmd::retrieve_for_task(m, typed));
        }
        Ok(request)
    }

    async fn after(&self, typed: &str, out: &TaskOutcome, session: &AgentSession) {
        if let Some(m) = &self.memory {
            let verified = self.harness.verified_commands(&out.task_id);
            synthesis_app::memcmd::task_ended(m, typed, out, &verified);
            // reflection is a model call (opt-in): the window does not wait for it
            if m.reflect.is_some() {
                let m = m.clone();
                tokio::spawn(async move {
                    if let Some(rf) = &m.reflect {
                        synthesis_app::memcmd::reflect_jobs(&m.store, rf, 2).await;
                    }
                });
            }
        }
        if let Some(s) = &self.store {
            finish_store(s, self.transcript.as_deref(), out, &session.usage);
        }
    }
}

/// One row of the MCP panel.
#[derive(Debug, Clone)]
pub struct McpInfo {
    pub name: String,
    /// `connected`, `connecting`, `failed`, ... (the manager's state, lowercased).
    pub state: String,
    pub tools: usize,
}

pub struct Engine {
    pub sid: String,
    pub label: String,
    pub models: Vec<String>,
    pub effort_tiers: Vec<String>,
    pub notices: Vec<String>,
    pub cwd: String,
    /// The workspace root (what the Workspace panel lists).
    pub root: String,
    /// Slash commands: the local list, skills and MCP prompts.
    pub commands: Vec<Command>,
    pub context_window: Option<u64>,
    /// The folder is trusted for project config (`--trust-project`).
    pub trusted: bool,
    pub synthesis_home: PathBuf,
    harness: Arc<Harness>,
    tokio: Option<tokio::runtime::Runtime>,
    session: Arc<Mutex<Option<AgentSession>>>,
    history: Arc<Mutex<Vec<Message>>>,
    last: Arc<Mutex<Option<TaskOutcome>>>,
    pending: PendingMap,
    inputs: InputMap,
    cancel: Mutex<Option<CancellationToken>>,
    tx: UnboundedSender<UiMsg>,
    mcp: Option<Arc<synthesis_mcp::McpManager>>,
    host: Arc<Host>,
    rt: Arc<synthesis_terminal::TerminalRuntime>,
    worktree: Option<synthesis_app::wt::Worktree>,
    cleanup_worktree: bool,
    began: bool,
    pub sandbox: synthesis_tui::SandboxStatus,
    pub memory_on: bool,
    /// Lines of `/memory`: the instruction files loaded at start and where `#` notes go.
    pub memory_report: Vec<String>,
    /// Where `#` notes go (the TUI's `MemoryHook`).
    pub notes: synthesis_app::memory::Notes,
    /// A `!cmd` ended and the model should read its output (the window starts that turn).
    followup: Arc<std::sync::atomic::AtomicBool>,
}

impl Engine {
    /// Tests and tools: defaults plus a model, provider and sandbox mode. Blocking, like `start_with`.
    pub fn start(
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
            },
            env,
        )
    }

    /// Builds the harness and starts (or resumes) the session, like `prepare` + `round` in
    /// `apps/cli/src/tui.rs`. Blocking: call it from a worker thread, never from inside a tokio runtime.
    pub fn start_with(
        o: StartOpts,
        env: &Env,
    ) -> Result<(Self, futures::channel::mpsc::UnboundedReceiver<UiMsg>), String> {
        let StartOpts {
            project,
            mut common,
            resume,
            notes,
        } = o;
        let (tx, rx) = unbounded();
        let pending: PendingMap = PendingMap::default();
        let inputs: InputMap = InputMap::default();
        // the stored session continues in place; the picker may name a session of another
        // folder, which is why the workspace check is off
        common.resume = resume.as_ref().map(|r| r.record.id.clone());
        common.continue_session = false;
        common.resume_any_workspace = resume.is_some();
        let trusted = common.trust_project;
        let cleanup_worktree = common.cleanup_worktree;
        let native = Native {
            out: Box::new(Lines {
                tx: tx.clone(),
                buf: vec![],
            }),
            approver: Arc::new(ChanApprover {
                pending: pending.clone(),
                tx: tx.clone(),
            }),
            input: Arc::new(ChanInput {
                pending: inputs.clone(),
                tx: tx.clone(),
            }),
        };
        let p = prepare(
            &common,
            &project,
            env,
            &Limits::default(),
            Front::Native(native),
        )?;
        let Prepared {
            harness,
            context,
            skills,
            sink,
            mut session,
            sid,
            rt,
            store,
            transcript,
            mcp,
            memory,
            resumed_from_seq,
            model_changed,
            worktree,
            label,
            models,
            effort_tiers,
            context_window,
            sandbox,
            notices,
            homes,
            tokio,
            notes: memory_notes,
            ..
        } = p;
        // Started here (not by the app) so the session hooks follow `session.started`.
        let started = match resumed_from_seq {
            Some(seq) => {
                harness.resume_session(&session, seq);
                if let Some(m) = &model_changed {
                    harness.model_changed(m);
                }
                Ok(())
            }
            None => harness.start_session(&mut session, "startup"),
        };
        let source = if resumed_from_seq.is_some() {
            "resume"
        } else {
            "startup"
        };
        if started.is_ok() {
            tokio.block_on(harness.session_hook(
                HookEvent::SessionStart,
                json!({"source": source}),
                Some(source),
            ));
        }
        let cwd = session.workspace.cwd.to_string_lossy().into_owned();
        let root = session.workspace.root.to_string_lossy().into_owned();
        let workspace = session.workspace.clone();
        let commands: Vec<Command> = synthesis_tui::local_commands()
            .into_iter()
            .chain(
                skills
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
        let host = Arc::new(Host {
            mcp: mcp.clone(),
            memory: memory.clone(),
            context,
            harness: harness.clone(),
            sink,
            store,
            transcript,
            skills: skills.iter().map(|s| s.name.clone()).collect(),
            workspace,
        });
        let mut all_notices = notices;
        // the worktree's place is the first thing to know (the finish report comes at the end)
        if let Some(w) = &worktree {
            all_notices.push(format!(
                "working in worktree {} on branch {}",
                w.info.worktree.display(),
                w.info.branch
            ));
        }
        all_notices.extend(notes);
        let engine = Self {
            sid,
            label,
            models,
            effort_tiers,
            notices: all_notices,
            cwd,
            root,
            commands,
            context_window,
            trusted,
            synthesis_home: homes.synthesis,
            harness,
            tokio: Some(tokio),
            session: Arc::new(Mutex::new(Some(session))),
            history: Arc::new(Mutex::new(resume.map(|r| r.history).unwrap_or_default())),
            last: Arc::default(),
            pending,
            inputs,
            cancel: Mutex::new(None),
            tx,
            mcp,
            host,
            rt,
            worktree,
            cleanup_worktree,
            began: started.is_ok(),
            sandbox,
            memory_on: memory.is_some(),
            memory_report: (memory_notes.clone().hook().report)(),
            notes: memory_notes,
            followup: Arc::default(),
        };
        // a session never starts in bypassPermissions, whatever a stored session or a setting carried
        if engine.mode() == Mode::BypassPermissions {
            engine.set_mode(Mode::Default);
        }
        match started {
            Ok(()) => Ok((engine, rx)),
            // a start that failed still ends the session and closes what `prepare` opened
            Err(e) => {
                engine.close();
                Err(e.to_string())
            }
        }
    }

    pub fn mcp(&self) -> Option<&Arc<synthesis_mcp::McpManager>> {
        self.mcp.as_ref()
    }

    /// State of every configured MCP server (the panel's rows).
    pub fn mcp_status(&self) -> Vec<McpInfo> {
        self.mcp.as_ref().map_or_else(Vec::new, |m| {
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
        let i = &self.worktree.as_ref()?.info;
        Some((i.worktree.display().to_string(), i.branch.clone()))
    }

    /// Rows of the sessions other processes advertise (`/peers`); `None` when peers are off.
    /// Blocking: call it off the UI thread.
    pub fn peer_rows(&self) -> Option<Vec<Value>> {
        self.harness.peer_summary()?;
        self.tokio
            .as_ref()
            .and_then(|rt| rt.block_on(self.harness.peer_rows(true)))
    }

    pub fn mode(&self) -> Mode {
        lock(&self.harness.permissions()).mode()
    }

    pub fn set_mode(&self, mode: Mode) {
        let _ = self.harness.set_mode(mode, false);
    }

    /// Enters bypassPermissions after the person said yes to the confirmation dialog; the core still takes the
    /// flag from here only and answers with its own error otherwise.
    pub fn enable_bypass(&self) -> Result<(), String> {
        self.harness
            .set_mode(Mode::BypassPermissions, true)
            .map_err(|e| e.to_string())
    }

    pub fn set_effort(&self, effort: Option<String>) {
        self.harness.set_reasoning_effort(effort);
    }

    pub fn running(&self) -> bool {
        lock(&self.session).is_none()
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
        self.start_task(text, images, false)
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
        let Some(mut session) = lock(&self.session).take() else {
            return Err("a task is already running");
        };
        let token = CancellationToken::new();
        *lock(&self.cancel) = Some(token.clone());
        // The mode applies from here to the end of the task (a later switch never changes a
        // running turn).
        let mode = self.mode();
        session.permission_mode = synthesis_harness::from_mode(mode);
        let mut req = TaskRequest {
            history: lock(&self.history).clone(),
            images,
            ..TaskRequest::new(text.clone())
        };
        let (h, tx, host) = (self.harness.clone(), self.tx.clone(), self.host.clone());
        let (slot, history, last) = (
            self.session.clone(),
            self.history.clone(),
            self.last.clone(),
        );
        let Some(rt) = &self.tokio else {
            *lock(&self.session) = Some(session);
            return Err("the engine is closed");
        };
        rt.spawn(async move {
            let before = host.before(&text, &token).await;
            let r = match before {
                Ok(request) => {
                    if hidden {
                        req.display_text = Some(String::new());
                    } else if request != text {
                        req.display_text = Some(text.clone());
                    }
                    req.request = request;
                    h.run_task(&mut session, req, token)
                        .await
                        .map_err(|e| e.to_string())
                }
                Err(e) => Err(e),
            };
            let done = match r {
                Ok(o) => {
                    host.after(&text, &o, &session).await;
                    let warn = h.to_idle(&mut session).err().map(|e| e.to_string());
                    *lock(&history) = o.messages.clone();
                    *lock(&last) = Some(o);
                    warn.map_or(Ok(()), Err)
                }
                Err(e) => Err(e),
            };
            *lock(&slot) = Some(session);
            let _ = tx.unbounded_send(UiMsg::TaskDone(done));
        });
        Ok(())
    }

    /// `!cmd` (`record_only`: `!!cmd`): the harness runs it through the same Bash permission check,
    /// sandbox, hooks and redaction as the TUI's, and appends it with its output to the history.
    /// Plan mode refuses it (stricter than the TUI, which lets a typed command through in plan).
    /// `TaskDone` follows. `Err`: a task is running, or the command was refused before it started.
    pub fn user_command(&self, command: String, record_only: bool) -> Result<(), &'static str> {
        if self.mode() == Mode::Plan {
            return Err("plan mode does not run shell commands: Shift+Tab leaves it first");
        }
        let Some(session) = lock(&self.session).take() else {
            return Err("a task is already running");
        };
        let Some(rt) = &self.tokio else {
            *lock(&self.session) = Some(session);
            return Err("the engine is closed");
        };
        let token = CancellationToken::new();
        *lock(&self.cancel) = Some(token.clone());
        let (h, tx) = (self.harness.clone(), self.tx.clone());
        let (slot, history, followup) = (
            self.session.clone(),
            self.history.clone(),
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

    /// An image the person dropped: its resolved path, when the session may read it. The TUI's
    /// `@file` rule (`WorkspaceContext::resolve`): inside the workspace or an additional
    /// directory with links resolved (a link out is outside), never a secret file, no read-deny
    /// rule. Read the returned path, not the one given.
    pub fn image_path(&self, p: &Path) -> Result<PathBuf, String> {
        let real = self
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
        let denied = lock(&self.harness.permissions())
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
    pub fn clear_history(&self) {
        lock(&self.history).clear();
    }

    /// `/compact`. While a task runs the harness compacts at its next safe point (`Ok(true)`);
    /// otherwise the conversation between tasks is compacted now and [`UiMsg::Compacted`] follows
    /// (`Ok(false)`). `Err`: nothing to compact.
    pub fn compact(&self, instructions: Option<String>) -> Result<bool, &'static str> {
        if self.running() {
            self.harness.request_compact(instructions);
            return Ok(true);
        }
        let Some(session) = lock(&self.session).clone() else {
            return Err("a task is running");
        };
        let history = lock(&self.history).clone();
        let Some(rt) = self.tokio.as_ref().filter(|_| !history.is_empty()) else {
            return Err("nothing to compact yet");
        };
        let (h, slot, tx) = (self.harness.clone(), self.history.clone(), self.tx.clone());
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
        let mut v = vec!["Agents in this session (@name routes a line):".to_string()];
        v.extend(self.harness.agents().into_iter().map(|a| {
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
            self.harness
                .agent_defs()
                .into_iter()
                .map(|(n, d)| sanitize_for_display(&format!("  {n} \u{b7} {d}"))),
        );
        v
    }

    /// `/permissions`: the mode and the rules in force.
    pub fn permissions_report(&self) -> Vec<String> {
        let perms = self.harness.permissions();
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

    /// `/permissions allow RULE`: a session-only rule, the TUI's check and wording (`Err`: an error line).
    pub fn allow_rule(&self, rule: &str) -> Result<String, String> {
        if !synthesis_permissions::Permissions::is_well_formed_rule(rule) {
            return Err(
                "expected a rule like Bash(cargo test:*), Edit or mcp__server__tool".into(),
            );
        }
        if !lock(&self.harness.permissions()).add_allow(rule) {
            return Err("that allow rule already exists".into());
        }
        Ok(format!(
            "allowed for this session: {}",
            sanitize_for_display(rule)
        ))
    }

    /// `/permissions remove RULE`: from allow, ask and deny of this session only.
    pub fn remove_rule(&self, rule: &str) -> Result<String, String> {
        let shown = sanitize_for_display(rule);
        match lock(&self.harness.permissions()).remove_rule(rule) {
            0 => Err(format!("no rule {shown}")),
            n => Ok(format!("removed {n} rule(s): {shown}")),
        }
    }

    /// `/rename TITLE`: validates and stores the title of this session; `Ok` is the stored title.
    pub fn rename(&self, title: &str) -> Result<String, String> {
        match &self.host.store {
            Some(s) => synthesis_app::rename::rename(s, title),
            None => Err("/rename: this session is not stored (--no-session-store)".into()),
        }
    }

    /// `/name`: the name other sessions address this one by; `None` when peers are off.
    pub fn peer_name(&self) -> Option<Option<String>> {
        self.harness.peer_summary().map(|(n, _)| n)
    }

    /// `/mcp`: one block per server with state, transport, tools and the last error.
    pub fn mcp_report(&self) -> Vec<String> {
        let Some(m) = &self.mcp else {
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
        if let Some(t) = lock(&self.cancel).as_ref() {
            t.cancel();
        }
    }

    /// Answers the approval `id`; false when it is no longer waiting.
    pub fn answer(&self, id: &str, choice: Choice) -> bool {
        let Some(p) = lock(&self.pending).remove(id) else {
            return false;
        };
        p.reply.send(approval_for(choice, &p.offered)).is_ok()
    }

    /// Answers the plan or question `id` (`None`: nobody answers); false when it is gone.
    pub fn answer_input(&self, id: &str, reply: Option<UserReply>) -> bool {
        let Some(r) = lock(&self.inputs).remove(id) else {
            return false;
        };
        r.send(reply).is_ok()
    }

    /// Ends the session like the TUI host does (`round`): the `SessionEnd` hook, the stored usage,
    /// `session.completed`, the MCP servers, the worktree; then stops the runtime. Returns what the
    /// worktree finish found (path, branch, changes, cleanup), for the caller to show.
    pub fn close(mut self) -> Option<String> {
        self.cancel();
        let outcome = lock(&self.last).take();
        let mut outcome = outcome.unwrap_or(TaskOutcome {
            task_id: String::new(),
            status: synthesis_events::TaskStatus::Completed,
            reason: "closed".into(),
            verification_report_id: None,
            final_text: String::new(),
            usage: synthesis_events::Usage {
                input_tokens: 0,
                output_tokens: 0,
                cache_read_input_tokens: None,
                cache_creation_input_tokens: None,
                cost_usd: None,
            },
            changed_files: vec![],
            needed_permissions: vec![],
            messages: vec![],
        });
        if !self.began {
            outcome.status = synthesis_events::TaskStatus::Failed;
            outcome.reason = "session_start_failed".into();
        }
        let rt = self.tokio.take()?;
        if self.began {
            rt.block_on(self.harness.session_hook(
                HookEvent::SessionEnd,
                json!({"reason": outcome.reason}),
                Some(&outcome.reason),
            ));
        }
        // the usage of the tasks that ran (the session is back unless a task outlived the window)
        if let (Some(s), Some(sess)) = (&self.host.store, lock(&self.session).as_ref()) {
            finish_store(s, self.host.transcript.as_deref(), &outcome, &sess.usage);
        }
        self.harness.end_session(&outcome);
        if let Some(t) = &self.host.transcript {
            t.clear_notifier(); // break the sink <-> transcript cycle: the next session needs the store lock
        }
        self.rt.close_session(&self.sid);
        if let Some(m) = &self.mcp {
            rt.block_on(m.close());
        }
        let finished = self
            .worktree
            .as_ref()
            .map(|w| w.finish(self.cleanup_worktree));
        rt.shutdown_background();
        // the global hook holds the memory run, and with it the event sink and the session
        // store's write lock: the restarted session (resume, new folder) must be able to open it
        synthesis_tools::set_learned_skills(None);
        finished
    }
}

/// Ends an engine other threads may still hold for a moment (a peer poll): waits for them, then
/// [`Engine::close`]. Blocking; call it off the UI thread.
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
mod tests {
    use super::*;
    use synthesis_harness::ApprovalDecision;
    use synthesis_permissions::RuleSpec;

    fn offered() -> Vec<Suggestion> {
        vec![Suggestion::AddRules {
            rules: vec![RuleSpec {
                tool_name: "Bash".into(),
                rule_content: Some("ls".into()),
            }],
            behavior: synthesis_permissions::Decision::Allow,
        }]
    }

    #[test]
    fn choices_map_to_decisions_and_only_always_carries_rules() {
        let s = offered();
        let once = approval_for(Choice::Once, &s);
        assert_eq!(once.decision, ApprovalDecision::Allow);
        assert!(once.accepted.is_empty());
        let always = approval_for(Choice::Always, &s);
        assert_eq!(always.decision, ApprovalDecision::Allow);
        assert_eq!(always.accepted, s);
        let deny = approval_for(Choice::Deny, &s);
        assert_eq!(deny.decision, ApprovalDecision::Deny);
        assert!(deny.accepted.is_empty());
    }

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

    #[test]
    fn a_gone_window_denies_instead_of_hanging() {
        let (tx, rx) = unbounded();
        drop(rx);
        let ap = ChanApprover {
            pending: PendingMap::default(),
            tx: tx.clone(),
        };
        let inp = ChanInput {
            pending: InputMap::default(),
            tx,
        };
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let cancel = CancellationToken::new();
        let a = rt.block_on(ap.approve(
            ApprovalRequest {
                approval_id: "a".into(),
                agent_id: None,
                tool_call_id: "t".into(),
                tool: "Bash".into(),
                input: json!({}),
                dialog: "permission",
                prompt: String::new(),
                reason: String::new(),
                suggestions: vec![],
                origin: None,
            },
            &cancel,
        ));
        assert_eq!(a.decision, ApprovalDecision::AutoDenied);
        let q = NeedsUser::ExitPlanMode { plan: "p".into() };
        assert!(rt.block_on(inp.ask(&q, &cancel)).is_none());
    }

    #[test]
    fn allow_always_never_offers_bypass() {
        let (tx, mut rx) = unbounded();
        let pending = PendingMap::default();
        let ap = ChanApprover {
            pending: pending.clone(),
            tx,
        };
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap();
        let cancel = CancellationToken::new();
        let c2 = cancel.clone();
        // cancelled right after the ask is out: the approver answers with a policy denial
        let req = ApprovalRequest {
            approval_id: "a".into(),
            agent_id: None,
            tool_call_id: "t".into(),
            tool: "Bash".into(),
            input: json!({}),
            dialog: "permission",
            prompt: String::new(),
            reason: "why".into(),
            suggestions: vec![
                Suggestion::SetMode(Mode::BypassPermissions),
                offered().remove(0),
            ],
            origin: None,
        };
        let r = rt.block_on(async {
            let f = ap.approve(req, &cancel);
            tokio::pin!(f);
            tokio::select! {
                _ = &mut f => unreachable!("nobody answered"),
                () = tokio::time::sleep(std::time::Duration::from_millis(20)) => {}
            }
            c2.cancel();
            f.await
        });
        assert_eq!(r.decision, ApprovalDecision::Deny);
        let Some(UiMsg::Approval(a)) = rx.try_recv().ok() else {
            panic!("the ask reaches the window")
        };
        assert_eq!(a.suggestions, offered());
        assert_eq!(a.reason, "why");
        assert!(matches!(rx.try_recv().ok(), Some(UiMsg::Withdraw(id)) if id == "a"));
    }
}
