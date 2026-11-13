//! What a native front end does around one prepared session, shared by the desktop window and the
//! `synt engine --stdio` server: the session start and its hooks, the per-task work (skills, MCP
//! prompts and mentions, the memory block, the stored usage), the task slot, and the shutdown.
//! Mirrors `synt`'s own interactive host (`apps/cli/src/tui.rs`).

use std::sync::{Arc, Mutex};

use serde_json::json;
use synthesis_core::{AgentSession, WorkspaceContext};
use synthesis_events::Event;
use synthesis_harness::{
    EventSink, Harness, ImageAttachment, Scope, TaskOutcome, TaskRequest, sanitize_for_display,
};
use synthesis_hooks::HookEvent;
use synthesis_permissions::Mode;
use synthesis_providers::Message;
use tokio_util::sync::CancellationToken;

use crate::asks::Asks;
use crate::run::{Prepared, finish_store};

/// The refusal of a second task while one runs (also what a remote engine says).
pub const ALREADY_RUNNING: &str = "a task is already running";

pub fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// What `synt run` does around its task, for every task the person starts: MCP mentions and
/// prompts, skills, the memory block for the typed prompt, the memory episode afterwards and the
/// stored usage.
pub struct Host {
    pub mcp: Option<Arc<synthesis_mcp::McpManager>>,
    pub memory: Option<Arc<crate::memcmd::MemoryRun>>,
    pub context: Arc<crate::ctx::SynthesisContext>,
    pub harness: Arc<Harness>,
    pub sink: EventSink,
    pub store: Option<crate::events::Store>,
    pub transcript: Option<Arc<crate::transcript::Transcript>>,
    pub skills: Vec<String>,
    pub workspace: WorkspaceContext,
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
        let (request, notes) = crate::mcpinput::expand(self.mcp.as_ref(), typed, cancel).await?;
        let (request, files_missing) =
            crate::run::expand_file_mentions(&self.harness, &self.workspace, request);
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
                .set_memory(crate::memcmd::retrieve_for_task(m, typed));
        }
        Ok(request)
    }

    async fn after(&self, typed: &str, out: &TaskOutcome, session: &AgentSession) {
        if let Some(m) = &self.memory {
            let verified = self.harness.verified_commands(&out.task_id);
            crate::memcmd::task_ended(m, typed, out, &verified);
            // reflection is a model call (opt-in): the front end does not wait for it
            if m.reflect.is_some() {
                let m = m.clone();
                tokio::spawn(async move {
                    if let Some(rf) = &m.reflect {
                        crate::memcmd::reflect_jobs(&m.store, rf, 2).await;
                    }
                });
            }
        }
        if let Some(s) = &self.store {
            let _ = finish_store(s, self.transcript.as_deref(), out, &session.usage); // told on stderr
        }
    }
}

/// What a front end shows of a started session.
pub struct Info {
    pub label: String,
    pub models: Vec<String>,
    pub stale_model: Option<String>,
    pub effort_tiers: Vec<String>,
    pub context_window: Option<u64>,
    pub sandbox: synthesis_cli_ui::SandboxStatus,
    pub notices: Vec<String>,
    pub synthesis_home: std::path::PathBuf,
    pub notes: crate::memory::Notes,
    pub memory_on: bool,
    pub skills: Vec<synthesis_tools::SkillInfo>,
    pub cwd: String,
    pub root: String,
}

/// One live session: the harness, its task slot and everything `close` must end.
pub struct Live {
    pub sid: String,
    pub harness: Arc<Harness>,
    pub host: Arc<Host>,
    pub rt: Arc<synthesis_terminal::TerminalRuntime>,
    pub mcp: Option<Arc<synthesis_mcp::McpManager>>,
    pub worktree: Option<crate::wt::Worktree>,
    pub cleanup_worktree: bool,
    pub began: bool,
    pub tokio: Option<tokio::runtime::Runtime>,
    pub asks: Asks,
    pub session: Arc<Mutex<Option<AgentSession>>>,
    pub history: Arc<Mutex<Vec<Message>>>,
    pub last: Arc<Mutex<Option<TaskOutcome>>>,
    /// The session-wide token (`synt stop` cancels it); every task's token is its child.
    root_cancel: CancellationToken,
    cancel: Mutex<Option<CancellationToken>>,
}

impl Live {
    /// Starts (or resumes) the session `prepare` built, like the CLI's `round`: the session start and
    /// the `SessionStart` hook follow `session.started`. Blocking: call it from a worker thread, never
    /// from inside a tokio runtime. A start that failed still ends the session and closes what
    /// `prepare` opened.
    pub fn begin(
        p: Prepared,
        asks: Asks,
        history: Vec<Message>,
        cleanup_worktree: bool,
    ) -> Result<(Self, Info), String> {
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
            stale_model,
            worktree,
            label,
            models,
            effort_tiers,
            context_window,
            sandbox,
            mut notices,
            homes,
            tokio,
            notes,
            cancel: root_cancel,
            ..
        } = p;
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
        let (cwd, root) = (
            session.workspace.cwd.to_string_lossy().into_owned(),
            session.workspace.root.to_string_lossy().into_owned(),
        );
        let host = Arc::new(Host {
            mcp: mcp.clone(),
            memory: memory.clone(),
            context,
            harness: harness.clone(),
            sink,
            store,
            transcript,
            skills: skills.iter().map(|s| s.name.clone()).collect(),
            workspace: session.workspace.clone(),
        });
        // the worktree's place is the first thing to know (the finish report comes at the end)
        if let Some(w) = &worktree {
            notices.push(format!(
                "working in worktree {} on branch {}",
                w.info.worktree.display(),
                w.info.branch
            ));
        }
        let live = Self {
            sid,
            harness,
            host,
            rt,
            mcp,
            worktree,
            cleanup_worktree,
            began: started.is_ok(),
            tokio: Some(tokio),
            asks,
            session: Arc::new(Mutex::new(Some(session))),
            history: Arc::new(Mutex::new(history)),
            last: Arc::default(),
            root_cancel,
            cancel: Mutex::new(None),
        };
        // a session never starts in bypassPermissions, whatever a stored session or a setting carried
        if live.mode() == Mode::BypassPermissions {
            let _ = live.harness.set_mode(Mode::Default, false);
        }
        let info = Info {
            label,
            models,
            stale_model,
            effort_tiers,
            context_window,
            sandbox,
            notices,
            synthesis_home: homes.synthesis,
            notes,
            memory_on: memory.is_some(),
            skills,
            cwd,
            root,
        };
        match started {
            Ok(()) => Ok((live, info)),
            Err(e) => {
                live.close();
                Err(e.to_string())
            }
        }
    }

    pub fn mode(&self) -> Mode {
        lock(&self.harness.permissions()).mode()
    }

    pub fn running(&self) -> bool {
        lock(&self.session).is_none()
    }

    pub fn cancel(&self) {
        if let Some(t) = lock(&self.cancel).as_ref() {
            t.cancel();
        }
    }

    /// Takes the session out of its slot for a task; the caller puts it back.
    pub fn take_session(&self) -> Result<(AgentSession, CancellationToken), &'static str> {
        let Some(session) = lock(&self.session).take() else {
            return Err(ALREADY_RUNNING);
        };
        let token = self.root_cancel.child_token();
        *lock(&self.cancel) = Some(token.clone());
        Ok((session, token))
    }

    /// Starts a task; `done` runs when it ended (`Err`: it could not run, or the session did not return to
    /// idle). `Err` here: one is already running, or the engine is closed.
    pub fn start_task(
        &self,
        text: String,
        images: Vec<ImageAttachment>,
        hidden: bool,
        done: impl FnOnce(Result<(), String>) + Send + 'static,
    ) -> Result<(), &'static str> {
        let Some(rt) = &self.tokio else {
            return Err("the engine is closed");
        };
        let (mut session, token) = self.take_session()?;
        // The mode applies from here to the end of the task (a later switch never changes a
        // running turn).
        session.permission_mode = synthesis_harness::from_mode(self.mode());
        let mut req = TaskRequest {
            history: lock(&self.history).clone(),
            images,
            ..TaskRequest::new(text.clone())
        };
        let (h, host) = (self.harness.clone(), self.host.clone());
        let (slot, history, last) = (
            self.session.clone(),
            self.history.clone(),
            self.last.clone(),
        );
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
            let result = match r {
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
            done(result);
        });
        Ok(())
    }

    /// Ends the session like the CLI host does (`round`): the `SessionEnd` hook, the stored usage,
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
            let _ = finish_store(s, self.host.transcript.as_deref(), &outcome, &sess.usage); // told on stderr
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
