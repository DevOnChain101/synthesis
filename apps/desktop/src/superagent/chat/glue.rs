//! The desktop side of the conductor backend (super-agent-s1.md 4.1, 4.7): the window's implementations of
//! `ConductorReads` and `Effects`, and the blocking start of the conductor engine.
//!
//! Outcomes are reached only through `OutcomeClient` (ADR-021), on one worker thread of this module (the
//! client owns a tokio runtime, which must not be entered from the engine's runtime). Sessions and the goal
//! form belong to other phases of the window, so they come through [`WindowPort`]: the default
//! [`NoPort`] has no sessions and refuses every effect on them. Nothing here builds the person's click.

use std::path::{Path, PathBuf};
use std::sync::mpsc::{Sender, channel};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use clap::Parser as _;
use futures::channel::mpsc::UnboundedReceiver;
use synthesis_app::conductor::{
    ConductorConfig, ConductorHost, ConductorReads, Effects, GoalDraft, HostConfig, ItemRow,
    OutcomeRow, ProposalExecutor, SessionReport, SessionRow, SessionStart, Via,
};
use synthesis_app::orch_endpoint::LocalSocket;
use synthesis_app::outcome_client::{ClientError, OutcomeClient};
use synthesis_config::Env;
use synthesis_harness::PersonApproved;
use synthesis_session::outcome::{CommandState, Surface};
use synthesis_tools::BoxFuture;
use synthesis_view::outcomes::BudgetView;

use crate::engine::{Args, ConductorOpts, Engine, StartOpts, UiMsg};

/// How long a read of the conductor waits for the orchestrator.
const READ_WAIT: Duration = Duration::from_secs(10);
/// Outcomes whose budget is read for one list (one call each).
const MAX_ROWS: usize = 50;

/// What the window gives the conductor for sessions and the goal form (phases C1 and B2 own them).
pub trait WindowPort: Send + Sync {
    /// Own sessions only: this window's, live or ended.
    fn sessions(&self) -> Vec<SessionRow>;
    fn report(&self, handle: &str) -> Option<SessionReport>;
    fn start<'a>(
        &'a self,
        req: &'a SessionStart,
        approval: PersonApproved,
    ) -> BoxFuture<'a, Result<String, String>>;
    fn send<'a>(
        &'a self,
        sid: &'a str,
        text: &'a str,
        approval: PersonApproved,
    ) -> BoxFuture<'a, Result<(), String>>;
    fn interrupt<'a>(&'a self, sid: &'a str) -> BoxFuture<'a, Result<(), String>>;
    fn close<'a>(&'a self, sid: &'a str) -> BoxFuture<'a, Result<(), String>>;
    /// Opens the New goal form with the draft filled in. Nothing is created: the person finishes the contract
    /// there and approves it, as for any goal. Returns a label for the card.
    fn open_goal_form(&self, goal: &GoalDraft) -> Result<String, String>;
}

/// No sessions and no form: every effect on them is refused, so a card says so instead of pretending.
pub struct NoPort;

const NOT_WIRED: &str = "this window does not host sessions here yet";

impl WindowPort for NoPort {
    fn sessions(&self) -> Vec<SessionRow> {
        vec![]
    }
    fn report(&self, _: &str) -> Option<SessionReport> {
        None
    }
    fn start<'a>(
        &'a self,
        _: &'a SessionStart,
        _: PersonApproved,
    ) -> BoxFuture<'a, Result<String, String>> {
        Box::pin(async { Err(NOT_WIRED.into()) })
    }
    fn send<'a>(
        &'a self,
        _: &'a str,
        _: &'a str,
        _: PersonApproved,
    ) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(async { Err(NOT_WIRED.into()) })
    }
    fn interrupt<'a>(&'a self, _: &'a str) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(async { Err(NOT_WIRED.into()) })
    }
    fn close<'a>(&'a self, _: &'a str) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(async { Err(NOT_WIRED.into()) })
    }
    fn open_goal_form(&self, _: &GoalDraft) -> Result<String, String> {
        Err("the goal form is not wired here yet".into())
    }
}

// ------------------------------------------------------------------------------------------ outcomes

type Client = OutcomeClient<LocalSocket>;
type Job = Box<dyn FnOnce(Result<&mut Client, String>) + Send>;

/// One thread that owns the (lazily connected) `OutcomeClient` and runs jobs in order. It never starts the
/// orchestrator: with none running a job fails with the connection's own error.
pub struct OutcomeWorker {
    tx: Mutex<Sender<Job>>,
}

impl OutcomeWorker {
    pub fn start(synt_home: PathBuf) -> Self {
        let (tx, rx) = channel::<Job>();
        std::thread::spawn(move || {
            for job in rx {
                // one connection per job: a failed link never outlives the call that saw it
                match LocalSocket::connect_with(&synt_home, Surface::Superagent, false) {
                    Ok(s) => job(Ok(&mut OutcomeClient::new(s, Surface::Superagent))),
                    Err(e) => job(Err(e.to_string())),
                }
            }
        });
        Self { tx: Mutex::new(tx) }
    }

    fn submit(&self, job: Job) {
        let _ = self.tx.lock().unwrap_or_else(|e| e.into_inner()).send(job);
    }

    /// Runs `f` on the worker and waits (the conductor's reads are synchronous).
    fn read<R: Send + 'static>(
        &self,
        f: impl FnOnce(&mut Client) -> Result<R, ClientError> + Send + 'static,
    ) -> Option<R> {
        let (tx, rx) = channel();
        self.submit(Box::new(move |c| {
            let _ = tx.send(c.ok().and_then(|c| f(c).ok()));
        }));
        rx.recv_timeout(READ_WAIT).ok().flatten()
    }

    fn effect<R: Send + 'static>(
        &self,
        f: impl FnOnce(&mut Client) -> Result<R, ClientError> + Send + 'static,
    ) -> BoxFuture<'static, Result<R, String>> {
        let (tx, rx) = futures::channel::oneshot::channel();
        self.submit(Box::new(move |c| {
            let _ = tx.send(c.and_then(|c| f(c).map_err(|e| e.to_string())));
        }));
        Box::pin(async move {
            rx.await
                .unwrap_or_else(|_| Err("the orchestrator link ended".into()))
        })
    }
}

fn row_of(c: &mut Client, o: &synthesis_view::outcomes::OutcomeRow) -> OutcomeRow {
    let budget = c
        .budget(&o.id)
        .map(|v| BudgetView::from_json(&v))
        .unwrap_or_default();
    let kind = c
        .contract(&o.id, None)
        .ok()
        .and_then(|v| v.pointer("/contract/kind")?.as_str().map(str::to_string))
        .unwrap_or_else(|| "?".into());
    OutcomeRow {
        id: o.id.clone(),
        title: o.goal.clone(),
        kind,
        state: o.state.clone(),
        needs_you: o.needs_you.decisions_pending
            + u32::from(o.needs_you.blocked)
            + u32::from(o.needs_you.verified_awaiting_ack),
        spent_usd: budget.cost_usd.spent.unwrap_or(0.0),
        cap_usd: budget.cost_usd.limit.unwrap_or(0.0),
    }
}

/// What the conductor may read: sessions through the window's port, outcomes through the orchestrator.
/// Work items and reports have no read on the client yet, so those digests answer "not found".
pub struct DesktopReads {
    pub port: Arc<dyn WindowPort>,
    pub outcomes: Arc<OutcomeWorker>,
}

impl ConductorReads for DesktopReads {
    fn sessions(&self) -> Vec<SessionRow> {
        self.port.sessions()
    }
    fn session_report(&self, handle: &str) -> Option<SessionReport> {
        self.port.report(handle)
    }
    fn outcomes(&self, state: Option<&str>) -> Vec<OutcomeRow> {
        let want = state.map(str::to_string);
        self.outcomes
            .read(move |c| {
                let rows = c.outcomes(None)?;
                Ok(rows
                    .iter()
                    .filter(|o| want.as_deref().is_none_or(|w| o.state == w))
                    .take(MAX_ROWS)
                    .map(|o| row_of(c, o))
                    .collect())
            })
            .unwrap_or_default()
    }
    fn outcome(&self, id: &str) -> Option<OutcomeRow> {
        let id = id.to_string();
        self.outcomes.read(move |c| {
            let o = c.outcome(&id)?;
            Ok(row_of(c, &o))
        })
    }
    fn item(&self, _: &str) -> Option<ItemRow> {
        None
    }
    fn report(&self, _: &str) -> Option<String> {
        None
    }
}

/// What the person's click does. Reached only from `ProposalExecutor`, which needs the click.
pub struct DesktopEffects {
    pub port: Arc<dyn WindowPort>,
    pub outcomes: Arc<OutcomeWorker>,
}

impl Effects for DesktopEffects {
    fn start_session<'a>(
        &'a self,
        req: &'a SessionStart,
        approval: PersonApproved,
        _: &'a Via,
    ) -> BoxFuture<'a, Result<String, String>> {
        self.port.start(req, approval)
    }
    fn send<'a>(
        &'a self,
        sid: &'a str,
        text: &'a str,
        approval: PersonApproved,
        _: &'a Via,
    ) -> BoxFuture<'a, Result<(), String>> {
        self.port.send(sid, text, approval)
    }
    fn interrupt<'a>(&'a self, sid: &'a str, _: &'a Via) -> BoxFuture<'a, Result<(), String>> {
        self.port.interrupt(sid)
    }
    fn close<'a>(&'a self, sid: &'a str, _: &'a Via) -> BoxFuture<'a, Result<(), String>> {
        self.port.close(sid)
    }
    fn create_goal<'a>(
        &'a self,
        goal: &'a GoalDraft,
        _: &'a Via,
    ) -> BoxFuture<'a, Result<String, String>> {
        let r = self.port.open_goal_form(goal);
        Box::pin(async move { r })
    }
    fn pause_outcome<'a>(&'a self, id: &'a str, via: &'a Via) -> BoxFuture<'a, Result<(), String>> {
        let (id, key) = (id.to_string(), format!("pause-{}", via.proposal_id));
        let r = self.outcomes.effect(move |c| {
            let rev = c.outcome(&id)?.revision;
            let r = c.command("pause", &id, Some(rev), &key)?;
            Ok(r.state)
        });
        Box::pin(async move {
            match r.await? {
                CommandState::Applied | CommandState::Queued => Ok(()),
                s => Err(format!("the orchestrator did not pause it ({s:?})")),
            }
        })
    }
}

// ------------------------------------------------------------------------------------------ the engine

/// A running conductor: its engine, the host that serves its tools, and the executor that is the only way a
/// proposal becomes an effect. Dropping it ends the host (every open card expires) and closes the engine on a
/// thread of its own, so a window that is released mid-start never leaks the engine.
pub struct Live {
    engine: Option<Engine>,
    pub host: Arc<ConductorHost>,
    pub exec: Arc<ProposalExecutor>,
}

impl Live {
    pub fn engine(&self) -> Option<&Engine> {
        self.engine.as_ref()
    }

    pub fn running(&self) -> bool {
        self.engine.as_ref().is_some_and(Engine::running)
    }

    /// A conductor with no engine, for the card tests.
    #[cfg(any(test, feature = "shot"))]
    pub fn for_test(host: Arc<ConductorHost>, exec: Arc<ProposalExecutor>) -> Self {
        Self {
            engine: None,
            host,
            exec,
        }
    }
}

impl Drop for Live {
    fn drop(&mut self) {
        self.host.end();
        if let Some(e) = self.engine.take() {
            std::thread::spawn(move || {
                e.close();
            });
        }
    }
}

pub struct Started {
    pub live: Live,
    pub rx: UnboundedReceiver<UiMsg>,
    pub notes: Vec<String>,
}

/// Builds and starts the conductor. Blocking (config, model list, engine start): a worker thread only.
/// User and managed settings only (`ConductorConfig::load`), the folder is only the engine's workspace.
pub fn start(env: &Env, workspace: &Path, port: Arc<dyn WindowPort>) -> Result<Started, String> {
    // the gate again, now: the setting may have been turned off, or WSL mode on, since the window drew
    match super::model::gate(env) {
        super::model::Gate::Ready => {}
        super::model::Gate::Off => return Err("Super Agent is off".into()),
        super::model::Gate::Unavailable(why) => return Err(why),
    }
    let cfg = ConductorConfig::load(env, &synthesis_settings::managed_settings_path())?;
    let h = synthesis_app::homes(env)?;
    let models = crate::models::rows(env, workspace)
        .into_iter()
        .map(|m| m.value)
        .collect();
    let outcomes = Arc::new(OutcomeWorker::start(h.synthesis.clone()));
    let reads = Arc::new(DesktopReads {
        port: port.clone(),
        outcomes: outcomes.clone(),
    });
    let host = Arc::new(ConductorHost::new(
        HostConfig {
            max_sessions: cfg.max_sessions,
            confirm: cfg.confirm.clone(),
            home: h.user,
            synt_home: h.synthesis,
            models,
            // shortcut: only the built-in secret patterns mask digests; exact key values are not collected here
            secrets: vec![],
        },
        reads,
    ));
    let exec = Arc::new(ProposalExecutor::new(
        host.clone(),
        Arc::new(DesktopEffects { port, outcomes }),
    ));
    let mut common = Args::try_parse_from(["synthesis-desktop"])
        .map_err(|e| e.to_string())?
        .common;
    common.model.clone_from(&cfg.model);
    let (engine, rx) = Engine::start_conductor(
        StartOpts {
            project: workspace.to_path_buf(),
            common,
            resume: None,
            notes: vec![],
            verify: None,
        },
        ConductorOpts {
            host: host.clone(),
            max_cost_usd: cfg.max_cost_usd,
        },
        env,
    )?;
    Ok(Started {
        live: Live {
            engine: Some(engine),
            host,
            exec,
        },
        rx,
        notes: cfg.diagnostics,
    })
}
