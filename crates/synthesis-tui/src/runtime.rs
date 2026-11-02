//! The event loop: one thread owns the UI; everything else talks to it through `Msg`s.

use std::io::{self, Read, Write};
use std::sync::Arc;
use std::sync::mpsc::RecvTimeoutError;
use std::time::Duration;
use synthesis_permissions::Mode;

use synthesis_config::Env;
use synthesis_core::AgentSession;
use synthesis_events::Envelope;
use synthesis_harness::{Harness, ImageAttachment, TaskAgent, TaskOutcome, TaskRequest, from_mode};
use synthesis_providers::Message;
use synthesis_tui_base::Platform;
use synthesis_tui_base::clipboard::{
    ClipboardSink, ClipboardSource, copy_to_clipboard, default_chain, paste_chain, read_clipboard,
};
use synthesis_tui_base::complete::Command as CmdDef;
use synthesis_tui_base::imgclip::{ImageSource, default_source};
use synthesis_tui_base::keys::{Key, KeyParser, Named};
use synthesis_tui_base::screen::{BRACKETED_PASTE_ON, Screen};
use synthesis_tui_base::sgr::{ColorMode, encode};
use synthesis_tui_base::terminal::{GuardOptions, TerminalGuard};

use crate::SavedSession;
use crate::app::{App, AppConfig, Cmd, Info, PermAction};
use crate::link::{Msg, TaskDone, UiLink, UserDone};
use crate::links::{
    LinkCache, OpenTarget, Opener, SystemOpener, hyperlink_line_cached, hyperlinks_enabled,
};
use crate::resume::Resume;
use crate::status::SandboxStatus;
use crate::tty::{SharedOut, Tty};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExitReason {
    Quit,
    /// stdin ended (terminal closed or pipe drained).
    InputClosed,
    /// The user picked a folder: the host starts a new session there.
    OpenFolder(std::path::PathBuf),
    /// The user picked a saved session: the host reopens it (see [`crate::load_resume`]).
    Resume(String),
    /// `/model NAME`: the host rebuilds the harness for that model and reopens this session.
    SwitchModel(String),
}

pub struct Exit {
    pub reason: ExitReason,
    /// `None` only when a task did not end within `quit_grace` (it still owns the session).
    pub session: Option<AgentSession>,
    /// Outcome of the last finished task, for `Harness::end_session`.
    pub last_outcome: Option<TaskOutcome>,
}

type SaveNote = Box<dyn Fn(bool, &str) -> Result<String, String>>;

/// `#` notes and `/memory`, supplied by the host that knows the instruction files.
pub struct MemoryHook {
    /// Display paths of the user and project instruction files a note can go to.
    pub user_file: String,
    pub project_file: String,
    /// Lines for `/memory`.
    pub report: Box<dyn Fn() -> Vec<String>>,
    /// The note as it will be written (one line, secrets masked): shown before the person confirms.
    pub preview: std::rc::Rc<dyn Fn(&str) -> String>,
    /// Appends the note (`user` = user file) after the person confirmed; `Ok` is the text to show.
    /// The host redacts secrets and never overwrites.
    pub save: SaveNote,
}

pub type BoxFut<'a, T> = std::pin::Pin<Box<dyn std::future::Future<Output = T> + Send + 'a>>;

/// What the host does around each task the person starts (not for sub-agent tasks): the same
/// steps `synth run` takes, kept out of this crate because they need the MCP, memory and session
/// store wiring.
pub trait TaskHost: Send + Sync {
    /// The typed line -> the request the model gets (MCP `@server:uri` and `/mcp__server__prompt`,
    /// skills, the memory block for this prompt). `Err` ends the task with that message.
    fn before<'a>(
        &'a self,
        typed: &'a str,
        cancel: &'a tokio_util::sync::CancellationToken,
    ) -> BoxFut<'a, Result<String, String>>;
    /// After a task ended, before the screen shows it as done: memory episode, stored usage.
    fn after<'a>(
        &'a self,
        typed: &'a str,
        out: &'a TaskOutcome,
        session: &'a AgentSession,
    ) -> BoxFut<'a, ()>;
}

type ModelSwitch = Box<dyn FnMut(&str) -> Result<(Arc<Harness>, String), String>>;

pub struct Options {
    pub harness: Arc<Harness>,
    /// Not yet started (`Created`) sessions are started here; resumed ones are used as given.
    pub session: AgentSession,
    pub link: UiLink,
    /// Runs the tasks. Call `run_interactive` from a thread that is not one of its workers.
    pub runtime: tokio::runtime::Handle,
    pub env: Env,
    pub platform: Platform,
    pub version: String,
    /// Shown as the model; defaults to `session.model_config_id`.
    pub model: Option<String>,
    pub sandbox: SandboxStatus,
    /// Entries of the `/model` picker.
    pub models: Vec<String>,
    /// `effort_tiers` of the current model; empty disables `/effort`.
    pub effort_tiers: Vec<String>,
    /// Applies a model choice: returns the rebuilt harness and its label. Without it `/model`
    /// only lists.
    pub switch_model: Option<ModelSwitch>,
    pub extra_commands: Vec<CmdDef>,
    /// Lines for `/mcp`: servers, state, tools, errors (called each time the command runs).
    pub mcp_report: Option<Box<dyn Fn() -> Vec<String>>>,
    pub memory: Option<MemoryHook>,
    pub host: Option<Arc<dyn TaskHost>>,
    /// Where Alt+V reads an image from; default: the Windows clipboard on WSL / Windows.
    pub image_source: Option<Arc<dyn ImageSource>>,
    pub saved: Vec<SavedSession>,
    pub resume: Option<Resume>,
    /// Shown as warnings in the pane from the first frame (sanitised on ingest); the matching
    /// `Notice` events arriving later are not shown twice.
    pub startup_notes: Vec<String>,
    /// Default: the OS tool, then OSC 52.
    pub clipboard: Option<Vec<Box<dyn ClipboardSink>>>,
    /// Right-click paste. Default: the OS tool(s) for the platform.
    pub paste_source: Option<Vec<Box<dyn ClipboardSource>>>,
    pub opener: Option<Box<dyn Opener>>,
    /// Minutes east of UTC (the greeting follows the local hour); the host knows the zone.
    pub tz_offset_min: i32,
    pub clock: fn() -> i64,
    /// Timer period while something animates / while idle (idle only polls the terminal size).
    pub tick: Duration,
    pub idle_tick: Duration,
    /// Minimum gap between two frames after input, and while a reply merely streams.
    pub min_frame: Duration,
    pub stream_frame: Duration,
    /// How long to wait for a cancelled task before leaving anyway.
    pub quit_grace: Duration,
}

impl Options {
    pub fn new(
        harness: Arc<Harness>,
        session: AgentSession,
        link: UiLink,
        runtime: tokio::runtime::Handle,
        env: Env,
    ) -> Self {
        Self {
            harness,
            session,
            link,
            runtime,
            env,
            platform: Platform::current(),
            version: String::new(),
            model: None,
            sandbox: SandboxStatus::default(),
            models: vec![],
            effort_tiers: vec![],
            switch_model: None,
            extra_commands: vec![],
            mcp_report: None,
            memory: None,
            host: None,
            image_source: None,
            saved: vec![],
            resume: None,
            startup_notes: vec![],
            clipboard: None,
            paste_source: None,
            opener: None,
            tz_offset_min: 0,
            clock: crate::wall_clock_ms,
            tick: Duration::from_millis(80),
            idle_tick: Duration::from_millis(500),
            min_frame: Duration::from_millis(30),
            stream_frame: Duration::from_millis(200),
            quit_grace: Duration::from_secs(5),
        }
    }
}

/// `COLUMNS` / `LINES` from the injected env, else 80x24.
pub fn size_from_env(env: &Env) -> (u32, u32) {
    let get = |k: &str, d: u32| {
        env.get_nonempty(k)
            .and_then(|v| v.parse::<u32>().ok())
            .filter(|v| *v > 0)
            .unwrap_or(d)
    };
    (get("COLUMNS", 80), get("LINES", 24))
}

fn cfg_for(o: &Options) -> AppConfig {
    let s = &o.session;
    AppConfig {
        cwd: s.workspace.cwd.to_string_lossy().into_owned(),
        session_id: s.id.clone(),
        model: o.model.clone().unwrap_or_else(|| s.model_config_id.clone()),
        version: o.version.clone(),
        budget: s.budget.clone(),
        sandbox: o.sandbox.clone(),
        // a session never starts in bypassPermissions
        mode: match synthesis_harness::to_mode(s.permission_mode) {
            Mode::BypassPermissions => Mode::Default,
            m => m,
        },
        models: o.models.clone(),
        effort_tiers: o.effort_tiers.clone(),
        extra_commands: o
            .extra_commands
            .iter()
            .filter(|c| !crate::app::is_local_command(&c.name))
            .cloned()
            .collect(),
        saved: o.saved.clone(),
        home: o
            .env
            .get_nonempty(if cfg!(windows) { "USERPROFILE" } else { "HOME" })
            .map(Into::into),
        mouse_debug: o.env.get("SYNTHESIS_MOUSE_DEBUG") == Some("1"),
        tz_offset_min: o.tz_offset_min,
        plain_logo: matches!(
            ColorMode::detect(&o.env, o.platform),
            ColorMode::Ansi16 | ColorMode::Mono
        ),
        agent_defs: o.harness.agent_defs().into_iter().map(|(n, _)| n).collect(),
    }
}

struct Runner {
    images: Option<Arc<dyn ImageSource>>,
    host: Option<Arc<dyn TaskHost>>,
    mcp_report: Option<Box<dyn Fn() -> Vec<String>>>,
    memory: Option<MemoryHook>,
    harness: Arc<Harness>,
    session: Option<AgentSession>,
    history: Vec<Message>,
    last_outcome: Option<TaskOutcome>,
    /// The running task answers as a sub-agent: its conversation is not `history`.
    agent_task: bool,
    runtime: tokio::runtime::Handle,
    tx: crate::link::Tx,
}

impl Runner {
    /// `/compact`: while a task runs the harness compacts at its next safe point, otherwise the
    /// conversation between tasks is compacted now (on the runtime; `Msg::Compacted` returns it).
    fn compact(&mut self, app: &mut App, instructions: Option<String>, now: i64) {
        if app.task_running() {
            self.harness.request_compact(instructions);
            app.note(
                "compaction requested: it runs before the next model request",
                "",
            );
            return;
        }
        let Some(session) = self.session.clone().filter(|_| !self.history.is_empty()) else {
            app.note("/compact: nothing to compact yet", "warn");
            return;
        };
        app.compact_started(now);
        let (h, history, tx) = (self.harness.clone(), self.history.clone(), self.tx.clone());
        self.runtime.spawn(async move {
            let cancel = tokio_util::sync::CancellationToken::new();
            let r = h.compact_now(&session, history, instructions, cancel).await;
            let _ = tx.send(Msg::Compacted(r));
        });
    }

    /// `/mcp`, `/agents`, `/permissions`: text for the pane and whether it is an error.
    fn info(&mut self, i: Info) -> (Vec<String>, bool) {
        match i {
            Info::Mcp => match &self.mcp_report {
                Some(f) => (f(), false),
                None => (
                    vec!["MCP: no servers are configured for this session".into()],
                    false,
                ),
            },
            Info::Memory => match &self.memory {
                Some(m) => ((m.report)(), false),
                None => (vec!["/memory: not available in this session".into()], true),
            },
            Info::Agents => {
                let mut v = vec!["Agents in this session (@name routes a line):".to_string()];
                v.extend(self.harness.agents().into_iter().map(|a| {
                    format!(
                        "  @{} · {} · {}{}",
                        a.name,
                        a.kind,
                        a.status,
                        if a.queued > 0 {
                            format!(" · {} queued", a.queued)
                        } else {
                            String::new()
                        }
                    )
                }));
                v.push("Types @agent:<type> starts (Task tool and chat):".to_string());
                v.extend(
                    self.harness
                        .agent_defs()
                        .into_iter()
                        .map(|(n, d)| format!("  {n} · {d}")),
                );
                (v, false)
            }
            Info::Permissions(a) => {
                let perms = self.harness.permissions();
                let mut p = perms.lock().unwrap_or_else(|e| e.into_inner());
                match a {
                    PermAction::List => {
                        let r = p.rules();
                        let mut v = vec![format!(
                            "mode: {}",
                            synthesis_tui_render::dialogs::mode_name(Some(p.mode().as_str()))
                        )];
                        for (k, rules) in [("allow", &r.allow), ("ask", &r.ask), ("deny", &r.deny)]
                        {
                            v.push(format!("{k} rules ({}):", rules.len()));
                            v.extend(
                                rules
                                    .iter()
                                    .map(|x| format!("  {}", crate::sanitize::clean_line(x))),
                            );
                        }
                        v.push("add: /permissions allow RULE · remove: /permissions remove RULE (this session only)".into());
                        (v, false)
                    }
                    PermAction::Allow(rule) => {
                        if p.add_allow(&rule) {
                            (
                                vec![format!(
                                    "allowed for this session: {}",
                                    crate::sanitize::clean_line(&rule)
                                )],
                                false,
                            )
                        } else {
                            (vec!["that allow rule already exists".into()], true)
                        }
                    }
                    PermAction::Remove(rule) => match p.remove_rule(&rule) {
                        0 => (
                            vec![format!("no rule {}", crate::sanitize::clean_line(&rule))],
                            true,
                        ),
                        n => (
                            vec![format!(
                                "removed {n} rule(s): {}",
                                crate::sanitize::clean_line(&rule)
                            )],
                            false,
                        ),
                    },
                }
            }
        }
    }

    /// `!cmd`: runs on the runtime; `Msg::UserCommandDone` brings the history back.
    fn user_command(
        &mut self,
        app: &mut App,
        command: String,
        record_only: bool,
        cancel: tokio_util::sync::CancellationToken,
    ) {
        let Some(session) = self.session.clone() else {
            app.command_done(Some(
                "the session is not available (an earlier task did not end)",
            ));
            return;
        };
        let (h, history, tx) = (self.harness.clone(), self.history.clone(), self.tx.clone());
        self.runtime.spawn(async move {
            let result = h
                .run_user_command(&session, history, &command, cancel)
                .await
                .map_err(|e| e.to_string());
            let _ = tx.send(Msg::UserCommandDone(Box::new(UserDone {
                result,
                record_only,
            })));
        });
    }

    fn start(
        &mut self,
        app: &mut App,
        text: String,
        images: Vec<ImageAttachment>,
        display_text: Option<String>,
        cancel: tokio_util::sync::CancellationToken,
        agent: Option<TaskAgent>,
    ) {
        let Some(mut session) = self.session.take() else {
            app.task_done(
                &Err("the session is not available (an earlier task did not end)".into()),
                0,
            );
            return;
        };
        // The mode applies from here to the end of the task (a later switch never changes a
        // running turn).
        // bypassPermissions only counts when `Harness::set_mode` took it with a confirmation.
        let mode = {
            let perms = self.harness.permissions();
            let mut p = perms.lock().unwrap_or_else(|e| e.into_inner());
            let m = if app.mode == Mode::BypassPermissions && p.mode() != Mode::BypassPermissions {
                p.mode()
            } else {
                app.mode
            };
            p.set_mode(m);
            m
        };
        session.permission_mode = from_mode(mode);
        // the host's steps are for what the person typed (an empty display text marks the
        // harness's own follow-up note) and for the main agent
        let host = self
            .host
            .clone()
            .filter(|_| agent.is_none() && display_text.as_deref() != Some(""));
        let typed = text.clone();
        let mut req = TaskRequest {
            request: text,
            images,
            criteria: vec![],
            history: self.history.clone(),
            display_text,
        };
        let (h, tx) = (self.harness.clone(), self.tx.clone());
        self.agent_task = agent.is_some();
        let inner = self.runtime.spawn(async move {
            if let Some(host) = &host {
                match host.before(&typed, &cancel).await {
                    Ok(request) => {
                        if request != typed && req.display_text.is_none() {
                            req.display_text = Some(typed.clone());
                        }
                        req.request = request;
                    }
                    Err(e) => return (Err(e), None, session),
                }
            }
            let r = match agent {
                Some(a) => h.run_agent_task(&mut session, a, req, cancel).await,
                None => h.run_task(&mut session, req, cancel).await,
            }
            .map_err(|e| e.to_string());
            if let (Some(host), Ok(out)) = (&host, &r) {
                host.after(&typed, out, &session).await;
            }
            let warn = match &r {
                Ok(_) => h.to_idle(&mut session).err().map(|e| e.to_string()),
                Err(_) => None,
            };
            (r, warn, session)
        });
        self.runtime.spawn(async move {
            let done = match inner.await {
                Ok((result, warn, session)) => TaskDone {
                    result,
                    warn,
                    session: Some(session),
                },
                Err(e) => TaskDone {
                    result: Err(format!("the task stopped unexpectedly: {e}")),
                    warn: None,
                    session: None,
                },
            };
            let _ = tx.send(Msg::TaskDone(Box::new(done)));
        });
    }
}

fn spawn_reader(mut input: Box<dyn Read + Send>, tx: crate::link::Tx) {
    std::thread::spawn(move || {
        let mut buf = [0u8; 4096];
        loop {
            match input.read(&mut buf) {
                // A signal interrupting the read is not the end of the input.
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Ok(0) | Err(_) => {
                    let _ = tx.send(Msg::InputClosed);
                    return;
                }
                Ok(n) => {
                    if tx.send(Msg::Bytes(buf[..n].to_vec())).is_err() {
                        return;
                    }
                }
            }
        }
    });
}

/// SIGINT (an external `kill -INT`; raw mode turns ctrl-c into a key) and SIGWINCH.
#[cfg(unix)]
fn spawn_signals(tx: crate::link::Tx) -> io::Result<signal_hook::iterator::backend::Handle> {
    use signal_hook::consts::{SIGINT, SIGWINCH};
    let mut sigs = signal_hook::iterator::Signals::new([SIGINT, SIGWINCH])?;
    let handle = sigs.handle();
    std::thread::spawn(move || {
        for s in sigs.forever() {
            let m = if s == SIGINT {
                Msg::Interrupt
            } else {
                Msg::Wake
            };
            if tx.send(m).is_err() {
                return;
            }
        }
    });
    Ok(handle)
}

pub fn run_interactive(mut o: Options, tty: Tty) -> io::Result<Exit> {
    let clock = o.clock;
    let color = ColorMode::detect(&o.env, o.platform);
    let replay: Vec<Envelope> = o
        .resume
        .as_ref()
        .map(|r| r.events.clone())
        .unwrap_or_default();
    let history = o
        .resume
        .as_ref()
        .map(|r| r.history.clone())
        .unwrap_or_default();
    let mut app = App::new(cfg_for(&o), &replay, clock());
    if let Some(m) = &o.memory {
        app.set_memory_files(m.user_file.clone(), m.project_file.clone());
        app.set_note_preview(m.preview.clone());
    }
    app.startup_notes(&o.startup_notes);

    if o.session.status == synthesis_core::SessionStatus::Created {
        o.harness
            .start_session(&mut o.session, "startup")
            .map_err(io::Error::other)?;
    }
    let clipboard: Vec<Box<dyn ClipboardSink>> = o
        .clipboard
        .take()
        .unwrap_or_else(|| default_chain(o.platform, &o.env));
    let paste: Vec<Box<dyn ClipboardSource>> = o
        .paste_source
        .take()
        .unwrap_or_else(|| paste_chain(o.platform, &o.env));
    let opener: Box<dyn Opener> = o.opener.take().unwrap_or_else(|| {
        Box::new(SystemOpener {
            platform: o.platform,
            env: o.env.clone(),
        })
    });

    let Tty {
        input,
        output,
        size,
        raw_mode,
        signals: watch_signals,
        stop,
    } = tty;
    // however this function returns, its reader thread lets go of the terminal input
    struct StopReading(Arc<std::sync::atomic::AtomicBool>);
    impl Drop for StopReading {
        fn drop(&mut self) {
            self.0.store(true, std::sync::atomic::Ordering::SeqCst);
        }
    }
    let _stop_reading = StopReading(stop);
    let out = SharedOut::new(output);
    let guard = TerminalGuard::enter(out.clone(), GuardOptions { raw_mode })?;
    let mut screen = Screen::new(out.clone(), color);
    if hyperlinks_enabled(&o.env) {
        let (base, home) = (
            Some(o.session.workspace.cwd.clone()),
            o.env
                .get_nonempty(if cfg!(windows) { "USERPROFILE" } else { "HOME" })
                .map(std::path::PathBuf::from),
        );
        let cache = std::cell::RefCell::new(LinkCache::default());
        screen.set_linker(Some(Box::new(move |l| {
            hyperlink_line_cached(l, base.as_deref(), home.as_deref(), &mut cache.borrow_mut())
        })));
    }
    screen.enter()?;
    out.clone().write_all(BRACKETED_PASTE_ON.as_bytes())?;

    #[cfg(not(unix))]
    let _ = watch_signals; // Ctrl-C arrives as a key in raw mode; there is no SIGWINCH to watch
    let UiLink { tx, rx, .. } = std::mem::take(&mut o.link);
    spawn_reader(input, tx.clone());
    #[cfg(unix)]
    let signals = if watch_signals {
        Some(spawn_signals(tx.clone())?)
    } else {
        None
    };

    let mut runner = Runner {
        images: o
            .image_source
            .take()
            .or_else(|| default_source(o.platform).map(Arc::from)),
        host: o.host.take(),
        mcp_report: o.mcp_report.take(),
        memory: o.memory.take(),
        harness: o.harness.clone(),
        session: Some(o.session.clone()),
        history,
        last_outcome: None,
        agent_task: false,
        runtime: o.runtime.clone(),
        tx: tx.clone(),
    };
    let mut switch = o.switch_model.take();
    let mut parser = KeyParser::default();
    let mut last_size = size();
    let mut dirty = true;
    let mut urgent = true;
    let mut last_draw: Option<i64> = None;
    let mut quit_deadline: Option<i64> = None;

    let result = loop {
        let now = clock();
        // ---- draw (throttled)
        if dirty {
            let gap = if urgent || !app.calm_streaming() {
                o.min_frame
            } else {
                o.stream_frame
            }
            .as_millis() as i64;
            if last_draw.is_none_or(|t| now - t >= gap) {
                let (cols, rows) = last_size;
                let lines = app.render(cols, rows, now);
                screen.set_caret(app.caret());
                screen.draw(&lines, cols, rows)?;
                last_draw = Some(now);
                dirty = false;
                urgent = false;
            }
        }
        // ---- leave?
        if let Some(reason) = app.quit.clone() {
            if app.task_running() {
                app.cancel_task();
                let dl = *quit_deadline.get_or_insert(now + o.quit_grace.as_millis() as i64);
                if now >= dl {
                    break Exit {
                        reason,
                        session: None,
                        last_outcome: runner.last_outcome.take(),
                    };
                }
            } else {
                break Exit {
                    reason,
                    session: runner.session.take(),
                    last_outcome: runner.last_outcome.take(),
                };
            }
        }
        // ---- wait
        let wait = if dirty {
            let gap = o.min_frame.as_millis() as i64;
            Duration::from_millis(
                (gap - (clock() - last_draw.unwrap_or(0))).clamp(1, gap.max(1)) as u64,
            )
        } else if app.animating() {
            o.tick
        } else {
            o.idle_tick
        };
        // a held escape string or a paste without an end marker is resolved by the clock
        let wait = if parser.state.busy() {
            wait.min(Duration::from_millis(25))
        } else {
            wait
        };
        let msg = match rx.recv_timeout(wait) {
            Ok(m) => Some(m),
            Err(RecvTimeoutError::Timeout) => None,
            Err(RecvTimeoutError::Disconnected) => {
                break closed(&mut runner, ExitReason::InputClosed);
            }
        };
        let now = clock();
        let mut batch: Vec<Msg> = msg.into_iter().collect();
        while let Ok(m) = rx.try_recv() {
            batch.push(m);
            if batch.len() >= 256 {
                break;
            }
        }
        if batch.is_empty() {
            dirty |= app.tick(now);
            batch.push(Msg::Bytes(Vec::new())); // lets the key parser see the time pass
        }
        for m in batch {
            match m {
                Msg::Bytes(b) => {
                    let keys = parser.feed_at(&b, now);
                    if parser.state.take_paste_activity() {
                        app.paste_activity(now);
                    }
                    for k in keys {
                        urgent = true;
                        dirty = true;
                        for c in app.on_key(k, now) {
                            exec(
                                c,
                                &mut app,
                                &mut runner,
                                &mut screen,
                                &clipboard,
                                &paste,
                                &*opener,
                                &mut switch,
                                raw_mode,
                                &out,
                                now,
                            )?;
                        }
                    }
                }
                Msg::InputClosed => {
                    app.quit.get_or_insert(ExitReason::InputClosed);
                    dirty = true;
                }
                Msg::Interrupt => {
                    urgent = true;
                    dirty = true;
                    for c in app.on_key(Key::Named(Named::CtrlC), now) {
                        exec(
                            c,
                            &mut app,
                            &mut runner,
                            &mut screen,
                            &clipboard,
                            &paste,
                            &*opener,
                            &mut switch,
                            raw_mode,
                            &out,
                            now,
                        )?;
                    }
                }
                Msg::ImagePasted {
                    result,
                    fallback_text,
                } => {
                    app.image_done();
                    match result {
                        Ok(img) => app.attach_image(img, now),
                        Err(synthesis_tui_base::imgclip::ImageError::NoImage(_))
                            if fallback_text =>
                        {
                            match read_clipboard(&paste) {
                                Ok(text) => app.paste_clipboard(&text, now),
                                Err(e) => {
                                    app.toast(format!("clipboard unavailable: {e}"), now, true)
                                }
                            }
                        }
                        Err(e) => app.image_failed(&e.to_string()),
                    }
                    dirty = true;
                    urgent = true;
                }
                Msg::Wake => {}
                Msg::Event(e) => {
                    app.on_event(&e, now);
                    dirty = true;
                }
                Msg::BadEvent(t) => {
                    app.note(t, "warn");
                    dirty = true;
                }
                Msg::Approval(a) => {
                    app.on_approval(*a, now);
                    urgent = true;
                    dirty = true;
                }
                Msg::Input(a) => {
                    app.on_input(*a, now);
                    urgent = true;
                    dirty = true;
                }
                Msg::Withdraw(id) => {
                    app.on_withdraw(&id, now);
                    dirty = true;
                }
                Msg::Compacted(r) => {
                    match r {
                        Ok(history) => runner.history = history,
                        Err(e) => {
                            let (text, tone) = crate::chat::friendly_compact_error(&e);
                            app.note(format!("/compact: {text}"), tone);
                        }
                    }
                    app.compact_done();
                    dirty = true;
                    urgent = true;
                }
                Msg::SendFailed(text, why) => {
                    app.send_failed(&text, &why);
                    dirty = true;
                }
                Msg::UserCommandDone(d) => {
                    let UserDone {
                        result,
                        record_only,
                    } = *d;
                    match result {
                        Ok((history, out)) => {
                            runner.history = history;
                            app.command_done(None);
                            // the model reads the result, unless the person said not to (`!!`),
                            // the settings say not to, it never ran, or Esc cancelled it
                            if out.ran
                                && !out.cancelled
                                && !record_only
                                && runner.harness.shell_settings().user_commands_model_turn
                            {
                                let cancel = app.begin_followup(now);
                                runner.start(
                                    &mut app,
                                    synthesis_harness::user_command_note().to_string(),
                                    vec![],
                                    Some(String::new()),
                                    cancel,
                                    None,
                                );
                            }
                        }
                        Err(e) => app.command_done(Some(&e)),
                    }
                    dirty = true;
                    urgent = true;
                }
                Msg::TaskDone(d) => {
                    let TaskDone {
                        result,
                        warn,
                        session,
                    } = *d;
                    if let Some(s) = session {
                        runner.session = Some(s);
                    }
                    if let Ok(out) = &result {
                        // a sub-agent's conversation stays with the agent
                        if !std::mem::take(&mut runner.agent_task) {
                            runner.history.clone_from(&out.messages);
                        }
                        runner.last_outcome = Some(out.clone());
                    }
                    app.task_done(&result, now);
                    if let Some(w) = warn {
                        app.note(format!("session state: {w}"), "warn");
                    }
                    dirty = true;
                    urgent = true;
                }
            }
        }
        for c in app.next_queued(now) {
            urgent = true;
            dirty = true;
            exec(
                c,
                &mut app,
                &mut runner,
                &mut screen,
                &clipboard,
                &paste,
                &*opener,
                &mut switch,
                raw_mode,
                &out,
                now,
            )?;
        }
        // ---- terminal size (SIGWINCH only wakes us; the size is always re-read)
        let s = size();
        if s != last_size {
            last_size = s;
            app.set_size(s.0, s.1);
            screen.resize()?;
            dirty = true;
            urgent = true;
        }
    };

    guard.restore();
    #[cfg(unix)]
    if let Some(h) = signals {
        h.close();
    }
    Ok(result)
}

fn closed(runner: &mut Runner, reason: ExitReason) -> Exit {
    Exit {
        reason,
        session: runner.session.take(),
        last_outcome: runner.last_outcome.take(),
    }
}

#[allow(clippy::too_many_arguments)]
fn exec(
    c: Cmd,
    app: &mut App,
    runner: &mut Runner,
    screen: &mut Screen<SharedOut>,
    clipboard: &[Box<dyn ClipboardSink>],
    paste: &[Box<dyn ClipboardSource>],
    opener: &dyn Opener,
    switch: &mut Option<ModelSwitch>,
    raw_mode: bool,
    out: &SharedOut,
    now: i64,
) -> io::Result<()> {
    match c {
        Cmd::StartTask {
            text,
            images,
            display_text,
            cancel,
            agent,
        } => runner.start(app, text, images, display_text, cancel, agent),
        Cmd::PasteImage { fallback_text } => match &runner.images {
            Some(src) => {
                // off the UI thread: powershell.exe takes a second or more
                let (src, tx) = (src.clone(), runner.tx.clone());
                std::thread::spawn(move || {
                    let _ = tx.send(Msg::ImagePasted {
                        result: src.read(),
                        fallback_text,
                    });
                });
            }
            None if fallback_text => {
                app.image_done();
                match read_clipboard(paste) {
                    Ok(text) => app.paste_clipboard(&text, now),
                    Err(e) => app.toast(format!("clipboard unavailable: {e}"), now, true),
                }
            }
            None => app.image_failed(synthesis_tui_base::imgclip::unavailable()),
        },
        Cmd::SendTo { to, text } => {
            let (h, tx) = (runner.harness.clone(), runner.tx.clone());
            runner.runtime.spawn(async move {
                if let Err(e) = h.send_user_message(&to, text.clone()).await {
                    let _ = tx.send(Msg::SendFailed(text, e));
                }
            });
        }
        Cmd::CancelAgent(name) => {
            if !runner.harness.cancel_agent(&name) {
                app.note(format!("@{name} is not running"), "warn");
            }
        }
        Cmd::UserCommand {
            command,
            record_only,
            cancel,
        } => runner.user_command(app, command, record_only, cancel),
        Cmd::ClearConversation => runner.history.clear(),
        Cmd::Copy(text) => {
            let sinks: Vec<&dyn ClipboardSink> = clipboard.iter().map(|b| &**b).collect();
            if copy_to_clipboard(&text, &sinks) {
                app.toast(format!("Copied {} chars", text.chars().count()), now, false);
            } else {
                app.toast(
                    "copy failed: no clipboard tool accepted the text",
                    now,
                    true,
                );
            }
        }
        // shortcut: reads on the UI thread (bounded by the 2 s tool timeout); move to a worker if
        // a slow clipboard manager shows up in practice
        Cmd::PasteClipboard => match read_clipboard(paste) {
            Ok(text) => app.paste_clipboard(&text, now),
            Err(e) => app.toast(format!("clipboard unavailable: {e}"), now, true),
        },
        Cmd::Info(i) => {
            let (lines, error) = runner.info(i);
            app.output(lines.join("\n"), error);
        }
        Cmd::Remember { user, text } => match runner.memory.as_ref().map(|m| (m.save)(user, &text))
        {
            Some(Ok(done)) => app.output(done, false),
            Some(Err(e)) => app.output(format!("# note not saved: {e}"), true),
            None => app.output("# notes are not available in this session".into(), true),
        },
        Cmd::Compact { instructions } => runner.compact(app, instructions, now),
        Cmd::Open(t) => match opener.open(&t) {
            Ok(()) => {
                let verb = if matches!(t, OpenTarget::Url(_)) {
                    "opening"
                } else {
                    "revealed"
                };
                app.toast(format!("{verb} {}", t.describe()), now, false);
            }
            Err(e) => {
                app.toast(format!("could not open {}: {e}", t.describe()), now, true);
                app.note(format!("could not open {}: {e}", describe(&t)), "error");
            }
        },
        Cmd::SetMode {
            mode,
            confirmed_bypass,
        } => {
            if let Err(e) = runner.harness.set_mode(mode, confirmed_bypass) {
                let now = runner
                    .harness
                    .permissions()
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .mode();
                app.mode_rejected(now, &e.to_string());
            }
        }
        Cmd::SetEffort(e) => runner.harness.set_reasoning_effort(e),
        Cmd::SwitchModel(name) => match switch.as_mut() {
            // the host restarts the app on the same session with the other model
            None => app.quit = Some(ExitReason::SwitchModel(name)),
            Some(f) => match f(&name) {
                Ok((h, label)) => {
                    runner.harness = h;
                    app.model_switched(&label);
                }
                Err(e) => app.note(format!("/model {name}: {e}"), "error"),
            },
        },
        Cmd::ResetScreen => {
            screen.reset()?;
            if raw_mode {
                // ask the terminal for its real size; the reply arrives as a cursor-position key
                out.clone().write_all(
                    format!("{BRACKETED_PASTE_ON}\x1b7\x1b[999;999H\x1b[6n\x1b8").as_bytes(),
                )?;
            }
        }
    }
    Ok(())
}

fn describe(t: &OpenTarget) -> String {
    t.describe()
}

// ---------------------------------------------------------------- --once

pub struct OnceOptions {
    pub cwd: String,
    pub session_id: String,
    pub model: String,
    pub version: String,
    pub budget: synthesis_events::Budget,
    pub sandbox: SandboxStatus,
    pub mode: synthesis_permissions::Mode,
    /// Events to show (a saved session, or a recording).
    pub events: Vec<Envelope>,
    pub saved: Vec<SavedSession>,
    pub cols: u32,
    pub rows: u32,
    /// Explicit time: the frame is a pure function of its inputs.
    pub now: i64,
    pub color: ColorMode,
    pub startup_notes: Vec<String>,
}

/// `--once`: one frame to `out`, no raw mode, no input. The text is exactly the lines the
/// interactive app would draw (the dashboard from `render_frame`, then the status line).
pub fn render_once(o: &OnceOptions, out: &mut impl Write) -> io::Result<()> {
    let cfg = AppConfig {
        cwd: o.cwd.clone(),
        session_id: o.session_id.clone(),
        model: o.model.clone(),
        version: o.version.clone(),
        budget: o.budget.clone(),
        sandbox: o.sandbox.clone(),
        mode: o.mode,
        models: vec![],
        effort_tiers: vec![],
        extra_commands: vec![],
        saved: o.saved.clone(),
        home: None,
        mouse_debug: false,
        tz_offset_min: 0,
        plain_logo: matches!(o.color, ColorMode::Ansi16 | ColorMode::Mono),
        agent_defs: vec![],
    };
    let mut app = App::new(cfg, &o.events, o.now);
    app.startup_notes(&o.startup_notes);
    let lines = app.render(o.cols, o.rows, o.now);
    for l in &lines {
        writeln!(out, "{}", encode(l, o.color))?;
    }
    Ok(())
}
