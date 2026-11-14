//! Running one attempt's worker and settling the result (ADR-017 "Leases, liveness, recovery").
//!
//! The worker is a child process (`synt run --unattended --resume <session> --attempt-session`)
//! in its own process group. The runner records its pid and start token, extends the lease while
//! it lives, forwards a cancel as the store's stop request (the worker stops itself and kills the
//! commands it started) and kills the whole tree if it has not exited after the grace period.
//! Every write is guarded by the attempt's fence.

use std::io;
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use synthesis_core::TaskStatus;
use synthesis_core::outcome::{AttemptState, WorkItemState};
use synthesis_session::outcome::{AttemptEnd, AttemptRecord, Effects, OutcomeWriter};
use synthesis_session::{Error as StoreError, ProcessProbe, Store};
use synthesis_workspace::WorktreeManager;

use crate::{AllocError, AttemptEnv, Profile, finish_attempt};

/// How a worker ended, as far as the attempt is concerned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkerEnd {
    /// Exit 0.
    Completed,
    /// Exit 3: finished, but its own checks do not vouch for it.
    Unverified,
    /// Exit 4: it needs a decision.
    Blocked,
    /// Exit 1, 2, 70, a signal, a crash, or the wall cap.
    Failed,
    /// Exit 130, or the runner stopped it on request.
    Cancelled,
}

impl WorkerEnd {
    pub fn task_status(self) -> TaskStatus {
        match self {
            Self::Completed => TaskStatus::Completed,
            Self::Unverified => TaskStatus::Unverified,
            Self::Blocked => TaskStatus::Blocked,
            Self::Failed => TaskStatus::Failed,
            Self::Cancelled => TaskStatus::Cancelled,
        }
    }
}

/// The exit statuses of `synt run` (docs/specs/events.md section 6): 0 completed, 1 failed,
/// 2 usage or configuration error (no session ran), 3 unverified, 4 blocked, 70 internal error,
/// 130 cancelled. A budget stop ends the task `failed` (exit 1) or `unverified` (3). A process
/// that was killed (no exit code) is a failure unless we cancelled it.
pub fn map_exit(code: Option<i32>, cancelled: bool, wall_exceeded: bool) -> WorkerEnd {
    if cancelled {
        return WorkerEnd::Cancelled;
    }
    if wall_exceeded {
        return WorkerEnd::Failed;
    }
    match code {
        Some(0) => WorkerEnd::Completed,
        Some(3) => WorkerEnd::Unverified,
        Some(4) => WorkerEnd::Blocked,
        Some(130) => WorkerEnd::Cancelled,
        _ => WorkerEnd::Failed,
    }
}

/// Starts the worker. The real one runs `synt`; tests substitute scripts.
pub trait Spawn {
    fn spawn(&self, profile: &Profile) -> io::Result<Child>;
}

/// `synt run ...` from the profile: own process group, no stdin, output to the attempt's log.
pub struct SyntSpawn {
    /// Where the worker's stdout and stderr go (created private).
    pub log: std::path::PathBuf,
    /// `Some`: the worker gets exactly this environment (tests); `None`: the owner's.
    pub env: Option<Vec<(std::ffi::OsString, std::ffi::OsString)>>,
}

impl Spawn for SyntSpawn {
    fn spawn(&self, p: &Profile) -> io::Result<Child> {
        let mut o = std::fs::OpenOptions::new();
        o.create(true).append(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            o.mode(0o600);
        }
        let log = o.open(&self.log)?;
        let mut c = Command::new(p.program());
        if let Some(env) = &self.env {
            c.env_clear().envs(env.iter().map(|(k, v)| (k, v)));
        }
        c.args(p.argv())
            .current_dir(p.cwd())
            .stdin(Stdio::null())
            .stdout(log.try_clone()?)
            .stderr(log);
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            c.process_group(0);
        }
        c.spawn()
    }
}

/// Set by the surface or the owner; the runner turns it into a stop request.
#[derive(Clone, Default)]
pub struct CancelFlag(Arc<AtomicBool>);

impl CancelFlag {
    pub fn cancel(&self) {
        self.0.store(true, Ordering::SeqCst);
    }

    pub fn is_set(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
}

#[derive(Debug)]
pub enum RunError {
    Alloc(AllocError),
    Store(StoreError),
    Io(io::Error),
}

impl std::fmt::Display for RunError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Alloc(e) => write!(f, "{e}"),
            Self::Store(e) => write!(f, "{e}"),
            Self::Io(e) => write!(f, "cannot start the worker: {e}"),
        }
    }
}
impl std::error::Error for RunError {}
impl From<AllocError> for RunError {
    fn from(e: AllocError) -> Self {
        Self::Alloc(e)
    }
}
impl From<StoreError> for RunError {
    fn from(e: StoreError) -> Self {
        Self::Store(e)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Settled {
    pub end: WorkerEnd,
    /// The commit of the attempt's tree (not for a cancelled attempt: its tree is kept as is).
    pub subject_commit: Option<String>,
    pub item: WorkItemState,
}

pub struct Runner<'a> {
    pub store: &'a Store,
    pub worktrees: &'a WorktreeManager,
    pub spawn: &'a dyn Spawn,
    pub probe: &'a dyn ProcessProbe,
    /// Time between checks of the worker, the lease and the cancel flag.
    pub poll: Duration,
    /// After a stop request the worker gets this long before the tree is killed.
    pub grace: Duration,
    /// Replaces the store call of the heartbeat (tests inject transient errors). `None` in use.
    pub beat: Option<&'a BeatFn<'a>>,
}

/// See [`Runner::beat`].
pub type BeatFn<'a> = dyn Fn(&mut OutcomeWriter, &synthesis_core::outcome::AttemptId, i64) -> Result<(), StoreError>
    + 'a;

/// Heartbeats missed in a row (a busy or failing database) before the runner gives the attempt up.
pub const MAX_MISSED_BEATS: u32 = 6;

fn item_row(w: &OutcomeWriter, a: &AttemptRecord) -> Result<(WorkItemState, i64), StoreError> {
    let it = w
        .list_work_items(w.id())?
        .into_iter()
        .find(|i| i.id == a.work_item_id)
        .ok_or_else(|| StoreError::NotFound("work item".into()))?;
    Ok((it.state, it.revision))
}

fn attempt_row(w: &OutcomeWriter, a: &AttemptRecord) -> Result<AttemptRecord, StoreError> {
    w.list_attempts(&a.work_item_id)?
        .into_iter()
        .find(|x| x.id == a.id)
        .ok_or_else(|| StoreError::NotFound("attempt".into()))
}

fn item_to(
    w: &mut OutcomeWriter,
    a: &AttemptRecord,
    to: WorkItemState,
) -> Result<WorkItemState, StoreError> {
    let (_, rev) = item_row(w, a)?;
    w.transition_work_item(&a.work_item_id, rev, to, &Effects::default())?;
    Ok(to)
}

/// Moves the work item after an attempt ended (ADR-017 mapping): completed and unverified hand
/// over to verification; failed retries while the contract's budget allows, then fail for
/// good; blocked waits for a decision; cancelled is cancelled.
pub fn settle_item(
    w: &mut OutcomeWriter,
    a: &AttemptRecord,
    end: WorkerEnd,
) -> Result<WorkItemState, StoreError> {
    let (state, _) = item_row(w, a)?;
    if state == WorkItemState::Leased {
        item_to(w, a, WorkItemState::Running)?;
    }
    match end {
        // `unverified` is what an unattended worker says when no declared check vouches for it
        // (inferred checks never mark a task passed); the independent runner (PR-09) is the authority,
        // so both hand over to verification, and only a failed verification retries
        WorkerEnd::Completed | WorkerEnd::Unverified => item_to(w, a, WorkItemState::Verifying),
        WorkerEnd::Blocked => item_to(w, a, WorkItemState::WaitingDecision),
        WorkerEnd::Cancelled => {
            item_to(w, a, WorkItemState::CancelRequested)?;
            item_to(w, a, WorkItemState::Cancelled)
        }
        WorkerEnd::Failed => {
            // the store derives the budget from the stored contract: a refusal means it is used up
            match item_to(w, a, WorkItemState::RetryableFailure) {
                Ok(s) => Ok(s),
                Err(StoreError::Invalid(m)) if m.contains("retry budget") => {
                    item_to(w, a, WorkItemState::TerminalFailure)
                }
                Err(e) => Err(e),
            }
        }
    }
}

/// A started worker that [`Runner::poll`] watches. Several can be watched by one owner.
pub struct Running {
    env: AttemptEnv,
    child: std::process::Child,
    pid: u32,
    began: Instant,
    wall: Duration,
    stop_at: Option<Instant>,
    cancelled: bool,
    wall_hit: bool,
    last_beat: Instant,
    missed: u32,
}

impl Running {
    pub fn env(&self) -> &AttemptEnv {
        &self.env
    }

    pub fn pid(&self) -> u32 {
        self.pid
    }
}

impl Runner<'_> {
    /// Runs the attempt's worker to its end and records everything (`start`, then `poll` until it
    /// settles). The attempt must be `starting` with its worktree allocated.
    pub fn run_attempt(
        &self,
        w: &mut OutcomeWriter,
        env: &AttemptEnv,
        profile: &Profile,
        cancel: &CancelFlag,
    ) -> Result<Settled, RunError> {
        let mut run = self.start(w, env, profile)?;
        loop {
            if let Some(s) = self.poll(w, &mut run, cancel)? {
                return Ok(s);
            }
            std::thread::sleep(self.poll.min(Duration::from_millis(50)));
        }
    }

    /// Starts the worker: the worktree is marked started (nothing resets it from now on), the
    /// attempt goes `running`, the process is spawned and recorded. A worker that cannot start
    /// fails the attempt (the item retries within its budget) instead of leaving it `running`.
    pub fn start(
        &self,
        w: &mut OutcomeWriter,
        env: &AttemptEnv,
        profile: &Profile,
    ) -> Result<Running, RunError> {
        self.worktrees
            .mark_started(&env.git)
            .map_err(AllocError::from)?;
        let rev = attempt_row(w, &env.attempt)?.revision;
        w.transition_attempt(
            &env.attempt.id,
            rev,
            AttemptState::Running,
            &AttemptEnd::default(),
            &Effects::default(),
        )?;
        if item_row(w, &env.attempt)?.0 == WorkItemState::Leased {
            item_to(w, &env.attempt, WorkItemState::Running)?;
        }
        let mut child = match self.spawn.spawn(profile) {
            Ok(c) => c,
            Err(e) => {
                let _ = self.settle(w, &env.attempt, &env.git, WorkerEnd::Failed);
                return Err(RunError::Io(e));
            }
        };
        let pid = child.id();
        let start = self.probe.start_time(pid);
        if let Err(e) = w.record_worker(&env.attempt.id, env.attempt.fence, pid, start.as_deref()) {
            self.abandon(w, env, &mut child, pid, &RunError::Store(e));
            return Err(RunError::Store(StoreError::Invalid(
                "the worker could not be recorded and was stopped".into(),
            )));
        }
        Ok(Running {
            env: env.clone(),
            child,
            pid,
            began: Instant::now(),
            wall: Duration::from_secs(profile.max_wall_secs()),
            stop_at: None,
            cancelled: false,
            wall_hit: false,
            last_beat: Instant::now(),
            missed: 0,
        })
    }

    /// Kills and reaps a worker after an error of ours, and (unless the worker is stale) marks its
    /// attempt lost with the reason. No worker outlives a failed runner.
    fn abandon(
        &self,
        w: &mut OutcomeWriter,
        env: &AttemptEnv,
        child: &mut std::process::Child,
        pid: u32,
        e: &RunError,
    ) {
        crate::proc::kill_child_tree(self.probe, pid);
        let _ = child.wait();
        let stale = matches!(e, RunError::Store(StoreError::Stale(_)));
        if !stale && let Ok(a) = attempt_row(w, &env.attempt) {
            let _ = w.transition_attempt(
                &a.id,
                a.revision,
                AttemptState::Lost,
                &AttemptEnd {
                    retained_reason: Some(format!("lost: {e}")),
                    ..AttemptEnd::default()
                },
                &Effects::default(),
            );
        }
    }

    fn beat(&self, w: &mut OutcomeWriter, env: &AttemptEnv) -> Result<(), StoreError> {
        match self.beat {
            Some(f) => f(w, &env.attempt.id, env.attempt.fence),
            None => w.heartbeat(&env.attempt.id, env.attempt.fence),
        }
    }

    /// One look at the worker: `Ok(None)` while it runs, `Ok(Some(..))` once it has exited, its
    /// tree is gone and the attempt and work item are settled. The cancel flag becomes a stop
    /// request; the tree is killed after the grace period, at the wall cap, or when the heartbeat
    /// says the worker is stale. On an error of ours the tree is killed and reaped first.
    pub fn poll(
        &self,
        w: &mut OutcomeWriter,
        run: &mut Running,
        cancel: &CancelFlag,
    ) -> Result<Option<Settled>, RunError> {
        match self.look(w, run, cancel) {
            Ok(None) => Ok(None),
            Ok(Some(end)) => self
                .settle(w, &run.env.attempt, &run.env.git, end)
                .map(Some),
            Err(e) => {
                let env = run.env.clone();
                self.abandon(w, &env, &mut run.child, run.pid, &e);
                Err(e)
            }
        }
    }

    fn look(
        &self,
        w: &mut OutcomeWriter,
        r: &mut Running,
        cancel: &CancelFlag,
    ) -> Result<Option<WorkerEnd>, RunError> {
        // The tree is swept BEFORE the child is reaped: an unreaped pid cannot be reused.
        if crate::proc::exited_unreaped(r.pid) {
            crate::proc::kill_child_tree(self.probe, r.pid);
            let code = r.child.wait().ok().and_then(|s| s.code());
            return Ok(Some(map_exit(code, r.cancelled, r.wall_hit)));
        }
        #[cfg(not(unix))]
        if let Ok(Some(s)) = r.child.try_wait() {
            crate::proc::kill_child_tree(self.probe, r.pid);
            return Ok(Some(map_exit(s.code(), r.cancelled, r.wall_hit)));
        }
        if cancel.is_set() && r.stop_at.is_none() {
            // cooperative: the worker polls the stop request, cancels, kills its commands
            self.store.stop_attempt(w, &r.env.session_id)?;
            (r.stop_at, r.cancelled) = (Some(Instant::now()), true);
        }
        if !r.wall_hit && r.began.elapsed() > r.wall + self.grace {
            r.wall_hit = true;
            crate::proc::kill_child_tree(self.probe, r.pid);
        }
        if r.stop_at.is_some_and(|t| t.elapsed() > self.grace) {
            crate::proc::kill_child_tree(self.probe, r.pid);
        }
        if r.last_beat.elapsed() >= self.poll {
            r.last_beat = Instant::now();
            match self.beat(w, &r.env) {
                Ok(()) => r.missed = 0,
                // this worker was superseded: it must not go on
                Err(e @ StoreError::Stale(_)) => return Err(RunError::Store(e)),
                // a busy or failing database says nothing about the worker: try again
                Err(e) => {
                    r.missed += 1;
                    if r.missed >= MAX_MISSED_BEATS {
                        return Err(RunError::Store(StoreError::Invalid(format!(
                            "the heartbeat failed {} times in a row (last: {e}); the attempt is given up",
                            r.missed
                        ))));
                    }
                }
            }
        }
        Ok(None)
    }

    /// Records the end of an attempt whose worker is gone (also used by recovery).
    pub fn settle(
        &self,
        w: &mut OutcomeWriter,
        attempt: &AttemptRecord,
        git: &synthesis_workspace::GitInfo,
        end: WorkerEnd,
    ) -> Result<Settled, RunError> {
        let subject = if end == WorkerEnd::Cancelled {
            // the tree is kept as the worker left it; nothing is committed on a cancel
            let rev = attempt_row(w, attempt)?.revision;
            w.transition_attempt(
                &attempt.id,
                rev,
                AttemptState::Cancelled,
                &AttemptEnd {
                    retained_reason: Some(
                        "cancelled: the worktree is kept as the worker left it".into(),
                    ),
                    ..AttemptEnd::default()
                },
                &Effects::default(),
            )?;
            None
        } else {
            Some(finish_attempt(
                self.worktrees,
                w,
                attempt,
                git,
                end.task_status(),
            )?)
        };
        let item = settle_item(w, attempt, end)?;
        Ok(Settled {
            end,
            subject_commit: subject,
            item,
        })
    }
}
