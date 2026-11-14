//! The scheduler (ADR-017 "Scheduler"): a bounded-parallel, deterministic walk over the plan's
//! work items.
//!
//! One owner thread drives it. Each [`Scheduler::tick`] polls the running workers, lets the
//! verifier judge items waiting in `verifying`, promotes retries whose backoff is over, makes
//! items whose dependencies all succeeded `ready`, and leases `ready` items in sorted key order
//! while fewer than `max_parallel` workers run. Every state change is the store's checked
//! transition, so an item is never leased twice and a plan cycle cannot run (the plan was
//! validated when stored; a dependency that can never succeed ends the walk as `blocked`).
//!
//! Pause and cancel are runtime flags of [`Control`]. The persistent commands that set them are
//! PR-11/12's (`outcome_commands`); a pause is not stored, so a restart resumes.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use synthesis_core::outcome::{
    AttemptState, OutcomeState, WorkItemId, WorkItemKind, WorkItemState,
};
use synthesis_session::Error as StoreError;
use synthesis_session::outcome::{Effects, OutcomeWriter, WorkItemRecord};
use time::OffsetDateTime;

use crate::{AllocError, CancelFlag, ProfileError, RunError, Settled};

#[derive(Debug)]
pub enum SchedError {
    Store(StoreError),
    Run(RunError),
    Alloc(AllocError),
    Profile(ProfileError),
    /// The outcome is not in a state the scheduler can walk.
    State(String),
}

impl std::fmt::Display for SchedError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Store(e) => write!(f, "{e}"),
            Self::Run(e) => write!(f, "{e}"),
            Self::Alloc(e) => write!(f, "{e}"),
            Self::Profile(e) => write!(f, "{e}"),
            Self::State(s) => write!(f, "{s}"),
        }
    }
}
impl std::error::Error for SchedError {}
impl From<StoreError> for SchedError {
    fn from(e: StoreError) -> Self {
        Self::Store(e)
    }
}
impl From<RunError> for SchedError {
    fn from(e: RunError) -> Self {
        Self::Run(e)
    }
}
impl From<AllocError> for SchedError {
    fn from(e: AllocError) -> Self {
        Self::Alloc(e)
    }
}
impl From<ProfileError> for SchedError {
    fn from(e: ProfileError) -> Self {
        Self::Profile(e)
    }
}

/// Allocates an attempt's environment and runs its worker. The real one is
/// [`crate::ProcessHost`]; tests substitute a fake.
pub trait WorkerHost {
    type Handle;
    /// `item` is `leased`; allocate attempt `ordinal` and start its worker. `spent_usd` is what the
    /// outcome's earlier attempts cost. On `Ok` the work item is `running`.
    fn start(
        &mut self,
        w: &mut OutcomeWriter,
        item: &WorkItemRecord,
        ordinal: u32,
        spent_usd: f64,
    ) -> Result<Self::Handle, SchedError>;

    /// One look at the worker; `Some` once it ended and everything is recorded.
    fn poll(
        &mut self,
        w: &mut OutcomeWriter,
        handle: &mut Self::Handle,
        cancel: &CancelFlag,
    ) -> Result<Option<Settled>, SchedError>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    Passed,
    /// Verification failed: the item retries within its budget.
    Failed(String),
    /// Not decided yet (look again on a later tick).
    Pending,
}

/// Judges an item that handed over to verification. PR-09's independent runner is the real one;
/// it records the evidence the store requires before `succeeded`.
pub trait Verifier {
    fn verify(
        &mut self,
        w: &mut OutcomeWriter,
        item: &WorkItemRecord,
    ) -> Result<Verdict, SchedError>;
}

/// No verifier yet: items stay in `verifying` ("unverified handoff"). The walk ends as
/// [`Stop::WaitingVerification`]; nothing is ever marked succeeded without evidence.
pub struct HandOff;

impl Verifier for HandOff {
    fn verify(&mut self, _: &mut OutcomeWriter, _: &WorkItemRecord) -> Result<Verdict, SchedError> {
        Ok(Verdict::Pending)
    }
}

#[derive(Debug, Clone)]
pub struct SchedConfig {
    /// Workers at once: the smaller of the contract's and the settings' (default 2).
    pub max_parallel: u32,
    /// Attempts per work item (contract under settings): bounds orphan recovery.
    pub max_attempts: u32,
    /// Wait before retry n is `base * 2^n`, capped.
    pub backoff_base: Duration,
    pub backoff_cap: Duration,
    /// The outcome's cost cap: no worker is started once the attempts' recorded spend reached it.
    pub max_cost_usd: Option<f64>,
}

impl SchedConfig {
    pub fn new(max_parallel: u32, max_attempts: u32) -> Self {
        Self {
            max_parallel: max_parallel.max(1),
            max_attempts,
            backoff_base: Duration::from_secs(5),
            backoff_cap: Duration::from_secs(300),
            max_cost_usd: None,
        }
    }

    fn backoff(&self, retries_used: u32) -> Duration {
        self.backoff_base
            .saturating_mul(1u32 << retries_used.min(16))
            .min(self.backoff_cap)
    }
}

/// Pause and cancel of the whole outcome.
#[derive(Clone, Default)]
pub struct Control {
    pause: Arc<AtomicBool>,
    pub cancel: CancelFlag,
}

impl Control {
    pub fn pause(&self) {
        self.pause.store(true, Ordering::SeqCst);
    }

    pub fn resume(&self) {
        self.pause.store(false, Ordering::SeqCst);
    }

    pub fn is_paused(&self) -> bool {
        self.pause.load(Ordering::SeqCst)
    }
}

/// Why a walk stopped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Stop {
    /// Every work item of the plan succeeded.
    Succeeded,
    /// Nothing can run any more: the reason names the items. The outcome is `blocked`.
    Blocked(String),
    Cancelled,
    /// Paused with no worker running.
    Paused,
    /// Items wait in `verifying` and the verifier has not decided.
    WaitingVerification,
    /// A retry waits for its backoff.
    Backoff,
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct TickReport {
    /// Keys leased this tick, in order.
    pub leased: Vec<String>,
    /// Keys whose worker ended this tick.
    pub settled: Vec<String>,
    pub stop: Option<Stop>,
    /// Failures of single items that did not stop the walk (a worker that would not start, a
    /// worker that failed to be supervised).
    pub errors: Vec<String>,
}

pub struct Scheduler<H: WorkerHost, V: Verifier> {
    pub host: H,
    pub verifier: V,
    pub cfg: SchedConfig,
    pub control: Control,
    running: Vec<(WorkItemId, H::Handle)>,
    not_before: BTreeMap<WorkItemId, OffsetDateTime>,
}

/// The first `slots` keys in the order the walk leases them (sorted, no repeats).
pub fn pick_in_order(ready: &[String], slots: usize) -> Vec<String> {
    let set: BTreeSet<&String> = ready.iter().collect();
    set.into_iter().take(slots).cloned().collect()
}

impl<H: WorkerHost, V: Verifier> Scheduler<H, V> {
    pub fn new(host: H, verifier: V, cfg: SchedConfig) -> Self {
        Self {
            host,
            verifier,
            cfg,
            control: Control::default(),
            running: vec![],
            not_before: BTreeMap::new(),
        }
    }

    pub fn running_count(&self) -> usize {
        self.running.len()
    }

    fn go(w: &mut OutcomeWriter, id: &WorkItemId, to: WorkItemState) -> Result<(), StoreError> {
        let rev = w.get_work_item(id)?.revision;
        w.transition_work_item(id, rev, to, &Effects::default())?;
        Ok(())
    }

    fn plan_items(w: &OutcomeWriter) -> Result<Vec<WorkItemRecord>, SchedError> {
        let o = w.get_outcome(w.id())?;
        let pv = o
            .plan_version
            .ok_or_else(|| SchedError::State("the outcome has no plan".into()))?;
        let mut v: Vec<_> = w
            .list_work_items(w.id())?
            .into_iter()
            .filter(|i| i.plan_version == pv)
            .collect();
        v.sort_by(|a, b| a.key.cmp(&b.key));
        Ok(v)
    }

    /// One step of the walk. The outcome must be `running`.
    pub fn tick(
        &mut self,
        w: &mut OutcomeWriter,
        now: OffsetDateTime,
    ) -> Result<TickReport, SchedError> {
        let mut rep = TickReport::default();
        let o = w.get_outcome(w.id())?;
        if o.state != OutcomeState::Running {
            return Err(SchedError::State(format!(
                "the outcome is {:?}, not running",
                o.state
            )));
        }
        let cancel = self.control.cancel.clone();

        // 1. workers
        let mut still = Vec::new();
        for (id, mut h) in std::mem::take(&mut self.running) {
            match self.host.poll(w, &mut h, &cancel) {
                Ok(None) => still.push((id, h)),
                Ok(Some(_)) => rep.settled.push(Self::key_of(w, &id)),
                Err(e) => rep.errors.push(format!("{}: {e}", Self::key_of(w, &id))),
            }
        }
        self.running = still;

        if cancel.is_set() {
            return self.finish_cancel(w, rep);
        }

        let items = Self::plan_items(w)?;
        let running_ids: BTreeSet<WorkItemId> =
            self.running.iter().map(|(i, _)| i.clone()).collect();

        // 2. items left Leased/Running by a worker we do not hold (a failed start, a failed
        // supervision, a lease taken just before a crash): free them for a new attempt
        for it in &items {
            if matches!(it.state, WorkItemState::Leased | WorkItemState::Running)
                && !running_ids.contains(&it.id)
                && !self.has_live_attempt(w, it)?
            {
                self.free(w, it)?;
            }
        }
        let items = Self::plan_items(w)?;

        // 3. verification of items that handed over
        self.verify_pass(w, &items, &mut rep)?;
        let items = Self::plan_items(w)?;

        // 4. retries whose backoff is over (sorted, deterministic)
        for it in items
            .iter()
            .filter(|i| i.state == WorkItemState::RetryableFailure)
        {
            let due = *self
                .not_before
                .entry(it.id.clone())
                .or_insert_with(|| now + self.cfg.backoff(it.retries_used));
            if now >= due {
                // the store refuses when the budget is used up: then it is final
                if Self::go(w, &it.id, WorkItemState::Ready).is_err() {
                    Self::go(w, &it.id, WorkItemState::TerminalFailure)?;
                }
                self.not_before.remove(&it.id);
            }
        }
        let items = Self::plan_items(w)?;

        // 5. items whose dependencies all succeeded
        let state_of: BTreeMap<&str, WorkItemState> =
            items.iter().map(|i| (i.key.as_str(), i.state)).collect();
        for it in items.iter().filter(|i| i.state == WorkItemState::Created) {
            let deps = w.work_item_deps(&it.id)?;
            if deps
                .iter()
                .all(|d| state_of.get(d.as_str()) == Some(&WorkItemState::Succeeded))
            {
                Self::go(w, &it.id, WorkItemState::Ready)?;
            }
        }
        let items = Self::plan_items(w)?;

        // 6. lease
        let spent = w.outcome_spend(w.id())?;
        let over_budget = self.cfg.max_cost_usd.is_some_and(|c| spent >= c);
        let live_unheld = items
            .iter()
            .filter(|i| matches!(i.state, WorkItemState::Leased | WorkItemState::Running))
            .count()
            .saturating_sub(self.running.len());
        let slots =
            (self.cfg.max_parallel as usize).saturating_sub(self.running.len() + live_unheld);
        if !self.control.is_paused() && !over_budget {
            let ready: Vec<String> = items
                .iter()
                .filter(|i| i.state == WorkItemState::Ready)
                .map(|i| i.key.clone())
                .collect();
            let mut agent_slots = slots;
            for key in pick_in_order(&ready, ready.len()) {
                let it = items.iter().find(|i| i.key == key).expect("listed");
                match it.kind {
                    WorkItemKind::Agent => {
                        if agent_slots == 0 {
                            continue;
                        }
                        agent_slots -= 1;
                        Self::go(w, &it.id, WorkItemState::Leased)?;
                        let ordinal = w.list_attempts(&it.id)?.len() as u32 + 1;
                        let leased = w
                            .list_work_items(w.id())?
                            .into_iter()
                            .find(|x| x.id == it.id)
                            .unwrap_or_else(|| it.clone());
                        match self.host.start(w, &leased, ordinal, spent) {
                            Ok(h) => {
                                rep.leased.push(it.key.clone());
                                self.running.push((it.id.clone(), h));
                            }
                            Err(e) => {
                                rep.errors.push(format!("{}: {e}", it.key));
                                self.block(w, it)?;
                            }
                        }
                    }
                    // a verify item has no worker: it goes straight to the verifier
                    WorkItemKind::Verify => {
                        for to in [
                            WorkItemState::Leased,
                            WorkItemState::Running,
                            WorkItemState::Verifying,
                        ] {
                            Self::go(w, &it.id, to)?;
                        }
                        rep.leased.push(it.key.clone());
                    }
                }
            }
        }

        // a verify item leased just now is judged in the same tick
        let items = Self::plan_items(w)?;
        self.verify_pass(w, &items, &mut rep)?;

        // 7. where does the walk stand?
        let items = Self::plan_items(w)?;
        rep.stop = self.stop_of(w, &items, over_budget)?;
        Ok(rep)
    }

    /// Asks the verifier about every item waiting in `verifying`: passed items succeed, failed
    /// ones retry within their budget (or fail for good).
    fn verify_pass(
        &mut self,
        w: &mut OutcomeWriter,
        items: &[WorkItemRecord],
        rep: &mut TickReport,
    ) -> Result<(), SchedError> {
        for it in items.iter().filter(|i| i.state == WorkItemState::Verifying) {
            match self.verifier.verify(w, it)? {
                Verdict::Passed => Self::go(w, &it.id, WorkItemState::Succeeded)?,
                Verdict::Pending => {}
                Verdict::Failed(why) => {
                    rep.errors
                        .push(format!("{}: verification failed: {why}", it.key));
                    self.retry_or_fail(w, it)?;
                }
            }
        }
        Ok(())
    }

    fn key_of(w: &OutcomeWriter, id: &WorkItemId) -> String {
        w.list_work_items(w.id())
            .ok()
            .and_then(|v| v.into_iter().find(|i| i.id == *id))
            .map_or_else(|| id.to_string(), |i| i.key)
    }

    fn has_live_attempt(&self, w: &OutcomeWriter, it: &WorkItemRecord) -> Result<bool, SchedError> {
        Ok(w.list_attempts(&it.id)?
            .iter()
            .any(|a| matches!(a.state, AttemptState::Starting | AttemptState::Running)))
    }

    /// Leased/Running without a worker: `recovering`, then `ready` while the attempt budget
    /// allows another attempt, else `terminal_failure`.
    fn free(&mut self, w: &mut OutcomeWriter, it: &WorkItemRecord) -> Result<(), SchedError> {
        let used = w.list_attempts(&it.id)?.len() as u32;
        Self::go(w, &it.id, WorkItemState::Recovering)?;
        let to = if used < self.cfg.max_attempts {
            WorkItemState::Ready
        } else {
            WorkItemState::TerminalFailure
        };
        Self::go(w, &it.id, to)?;
        Ok(())
    }

    /// A start that cannot work (no repository, no budget, no sandbox): the item is `blocked`
    /// with the error in the report, not retried in a loop.
    fn block(&mut self, w: &mut OutcomeWriter, it: &WorkItemRecord) -> Result<(), SchedError> {
        Self::go(w, &it.id, WorkItemState::Recovering)?;
        Self::go(w, &it.id, WorkItemState::Blocked)?;
        Ok(())
    }

    fn retry_or_fail(
        &mut self,
        w: &mut OutcomeWriter,
        it: &WorkItemRecord,
    ) -> Result<(), SchedError> {
        if Self::go(w, &it.id, WorkItemState::RetryableFailure).is_err() {
            Self::go(w, &it.id, WorkItemState::TerminalFailure)?;
        }
        Ok(())
    }

    fn finish_cancel(
        &mut self,
        w: &mut OutcomeWriter,
        mut rep: TickReport,
    ) -> Result<TickReport, SchedError> {
        if !self.running.is_empty() {
            return Ok(rep); // the stop requests are out; look again next tick
        }
        for it in Self::plan_items(w)? {
            if !it.state.is_terminal() && it.state != WorkItemState::CancelRequested {
                Self::go(w, &it.id, WorkItemState::CancelRequested)?;
            }
            if it.state != WorkItemState::Cancelled
                && !matches!(
                    it.state,
                    WorkItemState::Succeeded | WorkItemState::TerminalFailure
                )
            {
                Self::go(w, &it.id, WorkItemState::Cancelled)?;
            }
        }
        let rev = w.get_outcome(w.id())?.revision;
        w.transition_outcome(
            rev,
            OutcomeState::Cancelled,
            Some("cancelled"),
            &Effects::default(),
        )?;
        rep.stop = Some(Stop::Cancelled);
        Ok(rep)
    }

    fn stop_of(
        &mut self,
        w: &mut OutcomeWriter,
        items: &[WorkItemRecord],
        over_budget: bool,
    ) -> Result<Option<Stop>, SchedError> {
        if items.iter().all(|i| i.state == WorkItemState::Succeeded) {
            return Ok(Some(Stop::Succeeded));
        }
        if !self.running.is_empty() {
            return Ok(None);
        }
        let any = |s: WorkItemState| items.iter().any(|i| i.state == s);
        if any(WorkItemState::Ready) || any(WorkItemState::Leased) || any(WorkItemState::Running) {
            // runnable, or held by a worker we do not own: not stuck, just not leasable now
            if self.control.is_paused() {
                return Ok(Some(Stop::Paused));
            }
            if over_budget {
                return self
                    .block_outcome(w, "the outcome's cost cap is used up".into())
                    .map(Some);
            }
            return Ok(None);
        }
        if self.control.is_paused() {
            return Ok(Some(Stop::Paused));
        }
        if any(WorkItemState::Verifying) {
            return Ok(Some(Stop::WaitingVerification));
        }
        if any(WorkItemState::RetryableFailure) {
            return Ok(Some(Stop::Backoff));
        }
        // nothing can move: name what is stuck
        let stuck: Vec<String> = items
            .iter()
            .filter(|i| !matches!(i.state, WorkItemState::Succeeded))
            .map(|i| {
                format!(
                    "{} ({})",
                    i.key,
                    serde_json::to_string(&i.state)
                        .unwrap_or_default()
                        .trim_matches('"')
                )
            })
            .collect();
        self.block_outcome(w, format!("no work item can run: {}", stuck.join(", ")))
            .map(Some)
    }

    fn block_outcome(&mut self, w: &mut OutcomeWriter, reason: String) -> Result<Stop, SchedError> {
        let rev = w.get_outcome(w.id())?.revision;
        w.transition_outcome(
            rev,
            OutcomeState::Blocked,
            Some(&reason),
            &Effects::default(),
        )?;
        Ok(Stop::Blocked(reason))
    }

    /// Ticks until the walk stops for a reason that needs someone else (all succeeded, blocked,
    /// cancelled, paused, waiting for verification). Backoff waits are slept through.
    pub fn run(
        &mut self,
        w: &mut OutcomeWriter,
        clock: &dyn Fn() -> OffsetDateTime,
        pause_between: Duration,
    ) -> Result<(Stop, Vec<TickReport>), SchedError> {
        let mut reports = vec![];
        loop {
            let rep = self.tick(w, clock())?;
            let stop = rep.stop.clone();
            reports.push(rep);
            match stop {
                Some(Stop::Backoff) | None => std::thread::sleep(pause_between),
                Some(s) => return Ok((s, reports)),
            }
        }
    }
}
