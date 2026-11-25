//! The orchestrator process (`synt orchestrator --serve`, plan PR-12, ADR-017 L2).
//!
//! One process per user and `$SYNT_HOME` (`run/orch.lock`) serves protocol v4 on the local
//! endpoint ([`crate::orch_endpoint`]) and OWNS the outcomes that need it: the owner of an outcome
//! is whoever holds the lock of its control session, so a second process (or a test) that wants
//! one gets `Locked` and leaves it alone. Every cycle it adopts outcomes with work (running ones
//! and ones with queued commands), recovers the attempts of a dead owner, applies the commands
//! surfaces queued, walks the scheduler with the real worker host and the independent verifier,
//! notifies when a person is needed, and lets go of what needs nothing. Durable state is the
//! store; the process may die at any time and any client can start it again.
//!
//! Off by default: it refuses to start unless `synt.orchestrator.enabled`, and it stops (workers
//! included) when the setting is turned off. Ownership and correctness boundary, not a security
//! boundary (ADR-017).

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

use serde::Serialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use synthesis_config::Env;
use synthesis_core::WorkspaceContext;
use synthesis_core::outcome::{Contract, ContractKind, DecisionState, OutcomeId, OutcomeState};
use synthesis_orchestrator::{
    Allocator, Control, DecisionDriver, IndependentVerifier, ModelPlanner, PlanCfg, PlanRun,
    PlanWait, Planner, Platform, ProcessHost, ReasonCode, Runner, SandboxRunner, SchedConfig,
    Scheduler, Stop, StubPlanner, SyntSpawn, answered_plans, answered_waiting, apply_lifecycle,
    apply_queued_with, awaiting_rebrief, awaiting_replan, conclude, is_read_only, needs_model_plan,
    plan_wait, record_planner_failed, recover_attempts, recover_plan_runs, replan_after_revise,
    replan_open, restore_pause, start_plan_run, wall_secs,
};
use synthesis_peers::net::{Listener, prepare_run_dir};
use synthesis_peers::{platform_ok, run_dir};
use synthesis_session::outcome::{
    Effects, NewOutbox, NewOutcome, OutboxFilter, OutboxKind, OutcomePolicy, OutcomeRecord,
    OutcomeWriter,
};
use synthesis_session::{
    Error as StoreError, OsProbe, Reader, SCHEMA_VERSION_OUTCOMES, Store, StoreOptions,
};
use synthesis_settings::{PlannerMode, PlannerSettings};
use synthesis_workspace::WorktreeManager;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use crate::notify::{Notifier, outcome_payload};
use crate::orch_endpoint::{self as ep, Ctx, Job};
use crate::outcome_policy::{DISCLOSURE_ID, INTEGRITY_DISCLOSURE, outcome_policy};
use crate::outcome_service::{gateway_allows, settings_of};
use crate::proto::{Call, DecisionHint};
use synthesis_orchestrator::gateway::audit;
use synthesis_orchestrator::{Action, Principal};

/// Pause between cycles of the owner loop (a job wakes it earlier).
const TICK: Duration = Duration::from_millis(500);
/// How long an attempt's worker may be silent before a new owner treats it as dead.
const LEASE: Duration = Duration::from_secs(120);
/// Longest one independent check may run.
const CHECK_TIMEOUT: Duration = Duration::from_secs(600);
/// Most `outcome.create` replies kept for a retry with the same key (in memory).
const KEPT_CREATES: usize = 256;
/// The command kinds an owner applies; anything else does not make an outcome worth owning.
const KINDS: [&str; 8] = [
    "decision.resolve",
    "outcome.contract.approve",
    "outcome.contract.revise",
    "outcome.start",
    "outcome.pause",
    "outcome.resume",
    "outcome.cancel",
    "outcome.ack",
];

/// Failed replans of one revised contract after which the owner lets the outcome go (about 5 steps).
const MAX_REPLAN_FAILURES: u32 = 5;

fn s<E: ToString>(e: E) -> String {
    e.to_string()
}

/// Registers the key of every configured provider (the planner and the workers resolve theirs from
/// the same config, by reference: `{env:}`/`{file:}` are already substituted at load) with the
/// redactor, so a worker's report cannot carry one into a review. Returns the providers whose key
/// is NOT protected (unresolved, rejected or no config); pattern redaction stays on for them.
/// Project-level keys (a repo's own config) are out of scope by design: the owner reads the user
/// and managed config only.
fn learn_secrets(r: &synthesis_events::Redactor, env: &Env, h: &crate::Homes) -> Vec<String> {
    let cfg = match crate::load_config(env, h, None, synthesis_config::ProjectTrust::Untrusted) {
        Ok(c) => c,
        Err(_) => return vec!["(config)".into()],
    };
    let mut open = vec![];
    for (id, p) in &cfg.providers {
        if p.is_no_auth() {
            continue;
        }
        // everything the config or the env holds as this provider's key, and its header values
        let mut keys: Vec<String> = vec![];
        let cred = synthesis_config::resolve_credential(id, Some(p), &h.synthesis, env);
        keys.extend(cred.iter().map(|c| c.secret.clone()));
        if let Some(synthesis_config::ApiKey::Value(k)) = &p.api_key {
            keys.push(k.clone());
        }
        keys.extend(p.auth_token.clone());
        let configured = cred.is_some() || p.api_key.is_some() || p.auth_token.is_some();
        // a key that is set but cannot be protected (too short, reserved, no room) is named; a
        // header value is registered when it can be (short ones are not secrets by themselves)
        let mut ok = keys.iter().all(|k| r.add_secret_min(k, 8).is_ok());
        for v in p.headers.iter().flat_map(|h| h.values()) {
            let _ = r.add_secret_min(v, 8);
        }
        // nothing configured: a built-in nobody set up has nothing to leak; a reference that
        // failed to resolve is named by the config warnings, as `resolve_provider` reads them
        if !configured {
            ok = !cfg
                .warnings
                .iter()
                .any(|w| w.contains(&format!("provider \"{id}\"")));
        }
        if !ok {
            open.push(id.clone());
        }
    }
    open
}

fn log(msg: &str) {
    eprintln!(
        "synt orchestrator: {}",
        synthesis_harness::sanitize_for_display(msg)
    );
}

/// A full lowercase commit id (the recorded base of every attempt).
fn commit_id(c: &str) -> bool {
    matches!(c.len(), 40 | 64) && c.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

fn word<T: Serialize>(v: &T) -> String {
    serde_json::to_value(v)
        .ok()
        .and_then(|v| v.as_str().map(String::from))
        .unwrap_or_default()
}

/// Test-only switches of the owner process, set from `SYNT_TEST_ORCH_*` (feature `test-provider`:
/// debug builds only, see the `compile_error` in `lib.rs`). They are not settings.
#[cfg(feature = "test-provider")]
#[derive(Default)]
pub struct TestHooks {
    /// Worker options (`--provider fake:<script>`) every worker is started with.
    pub worker_args: Vec<String>,
    /// While this file exists no scheduler ticks (kills at a lifecycle point).
    pub hold: Option<PathBuf>,
}

type Sched<'a> = Scheduler<ProcessHost<'a>, IndependentVerifier>;

struct Owned<'a> {
    w: OutcomeWriter,
    control: Control,
    sched: Option<Sched<'a>>,
    /// Why no scheduler could be built, said once.
    refused: Option<String>,
    /// Answered decisions this owner already moved a blocked outcome for: each one gets one
    /// unblock, so an answer that changes nothing cannot flip the outcome blocked/running forever.
    unblocked_for: BTreeSet<String>,
    /// `decision id -> (revision, state)` as last published to push links.
    pushed: BTreeMap<String, (i64, String)>,
    /// The model planning run of this outcome that this process is watching.
    planning: Option<PlanRun>,
    /// Keeps what the driver already refused (said once) and the user's `max_items`.
    driver: DecisionDriver,
}

/// What the owner needs to plan with a model (`synt.orchestrator.planner.mode = model`): the
/// settings of this cycle, and where a model is resolved and priced.
pub struct PlannerCtx {
    pub settings: PlannerSettings,
    pub env: Env,
    pub homes: crate::Homes,
}

pub struct Owner<'a> {
    home: PathBuf,
    store: &'a Store,
    wm: &'a WorktreeManager,
    spawn: &'a SyntSpawn,
    program: PathBuf,
    owned: BTreeMap<String, Owned<'a>>,
    sent: BTreeSet<String>,
    notifier: Option<Arc<Notifier>>,
    /// Push links of the endpoint (decision hints), when it serves any.
    push: Option<tokio::sync::broadcast::Sender<DecisionHint>>,
    /// `(idempotency key, request fingerprint, reply)` of the latest creates, oldest first.
    created: VecDeque<(String, String, Value)>,
    /// `(outcome, contract version)` -> failed replans of a revised contract (this process only).
    replan_failed: BTreeMap<(String, u32), u32>,
    /// Set (by the loop, every cycle) only while `planner.mode` is `model`; `None` plans with the
    /// deterministic stub.
    pub planner: Option<PlannerCtx>,
    /// `synt.orchestrator.planner.max_items` of this cycle: a model plan is installed within it.
    pub max_items: usize,
    /// Masks session secrets in a worker's report shown for review; shared with the runner and verifier.
    pub redactor: synthesis_events::Redactor,
    /// The enable gate (build-time); a field so tests can close it in a debug build.
    #[doc(hidden)]
    pub enable_gate_open: bool,
    /// `(outcome, contract version, plan version)` the model planner could not even start for (said once).
    plan_failed: BTreeSet<(String, u32, Option<u32>)>,
    #[cfg(feature = "test-provider")]
    pub test: TestHooks,
}

/// The planner model: its label and the options to start it with, or why it cannot be used. An
/// unpriced model is refused HERE, before anything is reserved (a cost cap needs a price).
fn planner_model(
    ctx: &PlannerCtx,
    provider: Option<String>,
) -> Result<(String, Vec<String>), ReasonCode> {
    let cfg = crate::load_config(
        &ctx.env,
        &ctx.homes,
        None,
        synthesis_config::ProjectTrust::Untrusted,
    )
    .map_err(|_| ReasonCode::PlannerFailed)?;
    let res = crate::run::resolve_provider(
        provider.as_deref(),
        ctx.settings.model.as_deref(),
        &cfg,
        &ctx.homes.synthesis,
        &ctx.env,
    )
    .map_err(|_| ReasonCode::PlannerFailed)?;
    crate::run::pricing(
        &ctx.homes.synthesis.join("settings.json"),
        &res.label,
        &res.model,
    )
    .ok_or(ReasonCode::UnpricedModel)?;
    let mut extra = vec![];
    if let Some(p) = provider {
        extra.extend(["--provider".to_string(), p]);
    }
    if let Some(m) = &ctx.settings.model {
        extra.extend(["--model".to_string(), m.clone()]);
    }
    Ok((res.label, extra))
}

/// `decision id -> (revision, verified state word)`; a row whose state cannot be verified is left
/// out (nothing is hinted about it).
fn decision_rows(w: &OutcomeWriter) -> BTreeMap<String, (i64, String)> {
    w.list_decisions(w.id())
        .unwrap_or_default()
        .iter()
        .filter_map(|d| {
            let st = w.decision_state(&d.id).ok()?;
            Some((d.id.to_string(), (d.revision, word(&st))))
        })
        .collect()
}

fn has_work(r: &Reader, id: &OutcomeId) -> Result<bool, StoreError> {
    Ok(r.queued_commands(id)?
        .iter()
        .any(|c| KINDS.contains(&c.kind.as_str())))
}

impl<'a> Owner<'a> {
    pub fn new(
        home: &Path,
        store: &'a Store,
        wm: &'a WorktreeManager,
        spawn: &'a SyntSpawn,
        program: PathBuf,
        notifier: Option<Arc<Notifier>>,
    ) -> Self {
        Self {
            home: home.to_path_buf(),
            store,
            wm,
            spawn,
            program,
            owned: BTreeMap::new(),
            sent: BTreeSet::new(),
            notifier,
            push: None,
            created: VecDeque::new(),
            replan_failed: BTreeMap::new(),
            planner: None,
            max_items: synthesis_core::outcome::MAX_PLAN_ITEMS,
            redactor: synthesis_events::Redactor::new([]),
            enable_gate_open: synthesis_orchestrator::enable_gate_open(),
            plan_failed: BTreeSet::new(),
            #[cfg(feature = "test-provider")]
            test: TestHooks::default(),
        }
    }

    /// Publishes decision changes of the outcomes this owner drives to the endpoint's push links.
    pub fn set_push(&mut self, tx: tokio::sync::broadcast::Sender<DecisionHint>) {
        self.push = Some(tx);
    }

    pub fn owned_count(&self) -> usize {
        self.owned.len()
    }

    /// One pass: adopt, then drive every owned outcome, then let go of what needs nothing.
    pub fn cycle(&mut self, pol: &OutcomePolicy) -> Result<(), String> {
        let r = Reader::open(&self.home).map_err(s)?;
        for o in r.list_outcomes().map_err(s)? {
            let id = o.id.to_string();
            let active = matches!(o.state, OutcomeState::Running | OutcomeState::Verifying);
            // a blocked outcome with an answered request_evidence card is also taken (an owner restart must re-brief it)
            if self.owned.contains_key(&id)
                || !(active
                    || has_work(&r, &o.id).map_err(s)?
                    || awaiting_rebrief(&r, &o.id).unwrap_or(false)
                    || (awaiting_replan(&r, &o.id).unwrap_or(false)
                        && !self.replan_given_up(&id, o.contract_version))
                    // a model replan is wanted or waits for the person's answer
                    || (self.planner.is_some() && replan_open(&r, &o.id).unwrap_or(false)))
            {
                continue;
            }
            match self.store.open_outcome(&o.id, pol) {
                Ok(mut w) => {
                    let control = Control::default();
                    self.adopt(&mut w, &control, &o, pol);
                    // what exists now is not news: only later changes are pushed
                    let pushed = decision_rows(&w);
                    self.owned.insert(
                        id,
                        Owned {
                            w,
                            control,
                            sched: None,
                            refused: None,
                            unblocked_for: BTreeSet::new(),
                            pushed,
                            planning: None,
                            driver: DecisionDriver::with_max_items(self.max_items),
                        },
                    );
                }
                // another process owns it: leave it to them
                Err(StoreError::Locked(_)) => {}
                Err(e) => log(&format!("cannot own outcome {id}: {e}")),
            }
        }
        for id in self.owned.keys().cloned().collect::<Vec<_>>() {
            if let Some(mut ow) = self.owned.remove(&id)
                && self.step(&mut ow, pol)
            {
                self.owned.insert(id, ow);
            }
        }
        Ok(())
    }

    /// Takes over an outcome: attempts of a dead owner are reconciled (nothing is rerun, a lost
    /// attempt only frees its work item) and a pause a person made is kept.
    fn adopt(
        &self,
        w: &mut OutcomeWriter,
        control: &Control,
        o: &OutcomeRecord,
        pol: &OutcomePolicy,
    ) {
        if let Err(e) = restore_pause(w, control) {
            log(&format!("outcome {}: cannot read its pause: {e}", o.id));
            // fail closed: unknown means paused
            control.pause();
        }
        if o.state != OutcomeState::Running {
            return;
        }
        match recover_attempts(
            self.store,
            self.wm,
            &OsProbe,
            w,
            OffsetDateTime::now_utc(),
            LEASE,
            pol.max_attempts_per_work_item,
            Platform::current(),
        ) {
            Ok(rep) => {
                for n in &rep.notes {
                    log(&format!("outcome {}: recovery: {n}", o.id));
                }
                let (a, l, st, x, u) = (
                    rep.alive.len(),
                    rep.lost.len(),
                    rep.settled.len(),
                    rep.expired.len(),
                    rep.unknown.len(),
                );
                if a + l + st + x + u > 0 {
                    let fact =
                        json!({"alive": a, "lost": l, "settled": st, "expired": x, "unknown": u});
                    if let Err(e) = w.atomically(|tx| tx.emit("outcome.recovered", fact.clone())) {
                        log(&format!("outcome {}: cannot record recovery: {e}", o.id));
                    }
                }
            }
            Err(e) => {
                log(&format!("outcome {}: recovery failed: {e}", o.id));
                control.pause();
            }
        }
    }

    fn replan_given_up(&self, id: &str, version: u32) -> bool {
        self.replan_failed
            .get(&(id.to_string(), version))
            .is_some_and(|n| *n >= MAX_REPLAN_FAILURES)
    }

    /// Stores the plan of a rebound contract. A failure is logged and audited once per (outcome,
    /// contract version) as `outcome.replan_failed`; after `MAX_REPLAN_FAILURES` tries in a row the
    /// owner lets the outcome go: it stays `blocked` (a new `outcome revise` is queued as any command
    /// and is adopted again), and this process does not adopt it again for that version (a restart
    /// tries again).
    fn replan(&mut self, ow: &mut Owned<'a>, id: &str, planner: &dyn Planner) {
        let key = (
            id.to_string(),
            ow.w.get_outcome(ow.w.id())
                .map_or(0, |o| o.contract_version),
        );
        if self.replan_given_up(id, key.1) {
            return;
        }
        match replan_after_revise(&mut ow.w, planner) {
            Ok(_) => {
                self.replan_failed.remove(&key);
            }
            Err(e) => {
                let n = self.replan_failed.entry(key.clone()).or_insert(0);
                *n += 1;
                if *n == 1 || *n == MAX_REPLAN_FAILURES {
                    log(&format!(
                        "outcome {id}: replan of contract v{} failed (try {n}): {e}",
                        key.1
                    ));
                    let fact = json!({"contract_version": key.1, "tries": *n,
                        "gave_up": *n >= MAX_REPLAN_FAILURES, "reason": e.to_string()});
                    if let Err(x) =
                        ow.w.atomically(|tx| tx.emit("outcome.replan_failed", fact.clone()))
                    {
                        log(&format!(
                            "outcome {id}: cannot record the failed replan: {x}"
                        ));
                    }
                }
            }
        }
    }

    /// The model planning run (B7): watches the one this process started, settles one a dead owner
    /// left (`lost`, never rerun, the stub plan offered), and starts one when the outcome needs a
    /// plan and `planner.mode` is `model`. Every failure ends in a decision that offers only the
    /// stub plan; the owner never switches to it by itself.
    fn plan_step(&mut self, ow: &mut Owned<'a>, id: &str, now: OffsetDateTime) {
        if let Some(run) = ow.planning.as_mut() {
            let cancel = ow.control.cancel.is_set()
                || ow
                    .w
                    .get_outcome(ow.w.id())
                    .is_ok_and(|o| o.state.is_terminal());
            match run.poll(&mut ow.w, self.store, self.spawn, cancel, now) {
                Ok(true) => ow.planning = None,
                Ok(false) => {}
                Err(e) => {
                    // dropping the run kills its process; the row stays `running` and the next step settles it `lost`
                    log(&format!("outcome {id}: planning run: {e}"));
                    ow.planning = None;
                }
            }
            return;
        }
        match recover_plan_runs(&mut ow.w, &self.home, now) {
            Ok(done) => done
                .iter()
                .for_each(|r| log(&format!("outcome {id}: planning run {r} was lost with its owner; the stub plan is offered"))),
            Err(e) => log(&format!("outcome {id}: planning run recovery: {e}")),
        }
        let Some(ctx) = &self.planner else {
            return;
        };
        let Ok(o) = ow.w.get_outcome(ow.w.id()) else {
            return;
        };
        if !needs_model_plan(&ow.w).unwrap_or(false)
            || self
                .plan_failed
                .contains(&(id.to_string(), o.contract_version, o.plan_version))
        {
            return;
        }
        let (model, extra) = match planner_model(ctx, self.provider_arg()) {
            Ok((label, extra)) => (Ok(label), extra),
            Err(why) => (Err(why), vec![]),
        };
        let cfg = PlanCfg {
            program: self.program.clone(),
            home: self.home.clone(),
            extra,
            max_cost_usd: ctx.settings.max_cost_usd,
            max_items: ctx.settings.max_items as usize,
        };
        match start_plan_run(&mut ow.w, self.store, self.spawn, &cfg, model, now) {
            Ok(run) => {
                ow.planning = run;
                // nothing was started and nothing was offered (not even the stub plan is valid):
                // it is said once, not asked again every cycle
                if ow.planning.is_none()
                    && plan_wait(&ow.w).map_or(true, |w| w == PlanWait::NotStarted)
                {
                    self.plan_failed
                        .insert((id.to_string(), o.contract_version, o.plan_version));
                    if let Err(x) = record_planner_failed(&mut ow.w, ReasonCode::PlannerFailed) {
                        log(&format!(
                            "outcome {id}: cannot record the planner failure: {x}"
                        ));
                    }
                }
            }
            Err(e) => {
                log(&format!("outcome {id}: planning run not started: {e}"));
                self.plan_failed
                    .insert((id.to_string(), o.contract_version, o.plan_version));
                // persisted, not only remembered: a queued `outcome.start` ends with a message
                // (revise the contract) instead of waiting for ever, and a restart does not retry
                if let Err(x) = record_planner_failed(&mut ow.w, ReasonCode::PlannerFailed) {
                    log(&format!(
                        "outcome {id}: cannot record the planner failure: {x}"
                    ));
                }
            }
        }
    }

    /// The `--provider` a test build starts every model process with (`fake:<script>`); none in a release.
    fn provider_arg(&self) -> Option<String> {
        #[cfg(feature = "test-provider")]
        let found = {
            let a = &self.test.worker_args;
            a.iter()
                .position(|x| x == "--provider")
                .and_then(|i| a.get(i + 1))
                .cloned()
        };
        #[cfg(not(feature = "test-provider"))]
        let found = None;
        found
    }

    /// Returns whether the outcome stays owned.
    fn step(&mut self, ow: &mut Owned<'a>, pol: &OutcomePolicy) -> bool {
        let now = OffsetDateTime::now_utc();
        let id = ow.w.id().to_string();
        ow.driver.set_max_items(self.max_items);
        if let Err(e) = apply_queued_with(&mut ow.w, now, &mut ow.driver) {
            log(&format!("outcome {id}: commands: {e}"));
        }
        // with a model planner the plan comes from a planning run and the person's approval
        // the kind decides who plans; if it cannot be read, nothing is planned this step
        let read_only = match is_read_only(&ow.w) {
            Ok(r) => r,
            Err(e) => {
                log(&format!("outcome {id}: cannot read its contract: {e}"));
                return true;
            }
        };
        let planner: &dyn Planner = if self.planner.is_some() && !read_only {
            &ModelPlanner
        } else {
            &StubPlanner
        };
        if let Err(e) = apply_lifecycle(&mut ow.w, &ow.control, planner) {
            log(&format!("outcome {id}: commands: {e}"));
        }
        self.plan_step(ow, &id, now);
        // a revised contract was approved: its plan is stored and the outcome runs again (retried each step until it works)
        self.replan(ow, &id, planner);
        let Ok(mut o) = ow.w.get_outcome(ow.w.id()) else {
            return true;
        };
        // A person's answer to a decision moves a `blocked` outcome by itself: it goes back to
        // `running` once per answer and the scheduler re-checks everything, so anything else that
        // still blocks (a cap, another item) blocks it again with its own reason (fail closed). A
        // paused or cancelled outcome is left alone: only `resume` undoes a person's pause.
        if matches!(
            o.state,
            OutcomeState::Blocked | OutcomeState::ReworkRequired
        ) && !ow.control.is_paused()
            && !ow.control.cancel.is_set()
        {
            let fresh: Vec<String> = answered_waiting(&mut ow.w)
                .unwrap_or_default()
                .into_iter()
                .chain(answered_plans(&ow.w).unwrap_or_default())
                .filter(|d| !ow.unblocked_for.contains(d))
                .collect();
            if !fresh.is_empty() {
                match ow.w.transition_outcome(
                    o.revision,
                    OutcomeState::Running,
                    Some("a decision was answered"),
                    &Effects::default(),
                ) {
                    Ok(_) => {
                        // only a successful flip uses the answer up; a failed one retries next tick
                        ow.unblocked_for.extend(fresh);
                        if let Ok(n) = ow.w.get_outcome(ow.w.id()) {
                            o = n;
                        }
                    }
                    Err(e) => log(&format!("outcome {id}: unblock: {e}")),
                }
            }
        }
        #[cfg(feature = "test-provider")]
        if o.state == OutcomeState::Running && self.test.hold.as_ref().is_some_and(|f| f.exists()) {
            return true;
        }
        match o.state {
            OutcomeState::Running => {
                if ow.sched.is_none() && ow.refused.is_none() {
                    match self.build(&ow.w, &o, pol) {
                        Ok(mut sc) => {
                            sc.control = ow.control.clone();
                            sc.decisions =
                                Some(Box::new(DecisionDriver::with_max_items(self.max_items)));
                            ow.sched = Some(sc);
                        }
                        Err(e) => {
                            log(&format!("outcome {id}: not run: {e}"));
                            ow.refused = Some(e);
                        }
                    }
                }
                if ow.sched.is_none() && ow.control.cancel.is_set() {
                    // nothing runs it, so nothing else would end it
                    let end = ow.w.transition_outcome(
                        o.revision,
                        OutcomeState::Cancelled,
                        Some("cancelled"),
                        &Effects::default(),
                    );
                    if let Err(e) = end {
                        log(&format!("outcome {id}: cancel: {e}"));
                    }
                }
                if let Some(sc) = ow.sched.as_mut() {
                    match sc.tick(&mut ow.w, now) {
                        Ok(rep) => {
                            for e in rep.errors {
                                log(&format!("outcome {id}: {e}"));
                            }
                            if rep.stop == Some(Stop::Succeeded) {
                                sc.shutdown(&mut ow.w, "every work item succeeded");
                                if let Err(e) = conclude(&mut ow.w) {
                                    log(&format!("outcome {id}: verification step: {e}"));
                                }
                            }
                        }
                        Err(e) => log(&format!("outcome {id}: scheduler: {e}")),
                    }
                }
            }
            OutcomeState::Verifying => {
                if let Err(e) = conclude(&mut ow.w) {
                    log(&format!("outcome {id}: verification step: {e}"));
                }
            }
            _ => {}
        }
        let Ok(o) = ow.w.get_outcome(ow.w.id()) else {
            return true;
        };
        self.notices(&mut ow.w, &o);
        self.publish(ow);
        let keep = matches!(o.state, OutcomeState::Running | OutcomeState::Verifying)
            || ow.planning.is_some()
            || has_work(&ow.w, ow.w.id()).unwrap_or(true)
            // evidence was requested: the next step re-briefs (or the decision expires); an error here releases (fail closed)
            || awaiting_rebrief(&ow.w, ow.w.id()).unwrap_or(false)
            || (awaiting_replan(&ow.w, ow.w.id()).unwrap_or(false)
                && !self.replan_given_up(&id, o.contract_version))
            || (self.planner.is_some() && replan_open(&ow.w, ow.w.id()).unwrap_or(false));
        if !keep {
            if let Some(sc) = ow.sched.as_mut() {
                sc.shutdown(&mut ow.w, "the outcome is no longer running");
            }
            ow.sched = None;
        }
        keep
    }

    /// The scheduler of a running outcome: the real worker host and the independent verifier. An
    /// outcome whose contract is not approved, or whose checks cannot be run in an enforcing
    /// sandbox, is not run (fail closed).
    fn build(
        &self,
        w: &OutcomeWriter,
        o: &OutcomeRecord,
        pol: &OutcomePolicy,
    ) -> Result<Sched<'a>, String> {
        let (contract, _, approved): (Contract, String, bool) =
            w.get_contract(w.id(), o.contract_version).map_err(s)?;
        if !approved {
            return Err("its contract is not approved".into());
        }
        let mut verifier = IndependentVerifier::for_outcome(
            w,
            PathBuf::from(&o.repo_root),
            self.home.clone(),
            Arc::new(SandboxRunner::detect(self.home.clone())),
            CHECK_TIMEOUT,
        )
        .map_err(s)?;
        verifier.redactor = self.redactor.clone();
        let b = &contract.envelope.budgets;
        let mut cfg = SchedConfig::new(
            b.max_parallel_workers.min(pol.max_parallel_workers),
            b.max_attempts_per_work_item
                .min(pol.max_attempts_per_work_item),
        );
        cfg.max_cost_usd = Some(
            pol.max_cost_usd
                .map_or(b.max_cost_usd, |c| c.min(b.max_cost_usd)),
        );
        let grace = Duration::from_secs(10);
        cfg.unknown_after = Some(Duration::from_secs(wall_secs(&contract, pol)) + grace);
        let mut runner = Runner::new(
            self.store,
            self.wm,
            self.spawn,
            &OsProbe,
            Duration::from_millis(250),
            grace,
        );
        // what the owner itself resolves (the worker enforces with the same `detect()`); the
        // worker is never asked
        let sb = synthesis_sandbox::detect();
        use synthesis_sandbox::Guarantee;
        runner.redactor = self.redactor.clone();
        runner.sandbox = Some((
            sb.name().to_string(),
            match sb.capabilities().filesystem {
                Guarantee::None => "none",
                Guarantee::Weak => "weak",
                Guarantee::Strong => "strong",
            }
            .to_string(),
        ));
        let host = ProcessHost {
            allocator: Allocator {
                store: self.store,
                worktrees: self.wm,
                platform: Platform::current(),
                artifact_root: self.home.join("artifacts"),
            },
            runner,
            contract,
            policy: pol.clone(),
            program: self.program.clone(),
            extra: vec![],
            source: PathBuf::from(&o.repo_root),
            base_commit: o.base_commit.clone(),
            worker_kind: "local-engine",
        };
        #[cfg(feature = "test-provider")]
        let host = ProcessHost {
            extra: self.test.worker_args.clone(),
            ..host
        };
        Ok(Scheduler::new(host, verifier, cfg))
    }

    /// Tells the person's notify hook that a decision, a block or a verified outcome waits.
    fn notices(&mut self, w: &mut OutcomeWriter, o: &OutcomeRecord) {
        let Some(n) = self.notifier.clone() else {
            return;
        };
        for p in due_notices(w, o, &mut self.sent) {
            n.send_outcome(p);
        }
    }

    /// Hints the endpoint's push links about every decision that is new or changed since the last
    /// look: ids and the state word, never text. A hint nobody listens to is dropped.
    fn publish(&self, ow: &mut Owned<'a>) {
        let now = decision_rows(&ow.w);
        if let Some(tx) = &self.push {
            for (id, (revision, state)) in &now {
                if ow.pushed.get(id) != Some(&(*revision, state.clone())) {
                    let _ = tx.send(DecisionHint {
                        outcome: ow.w.id().to_string(),
                        id: id.clone(),
                        revision: *revision,
                        state: state.clone(),
                    });
                }
            }
        }
        ow.pushed = now;
    }

    /// Stops every worker and lets go of every outcome (the locks are released with the writers).
    pub fn release_all(&mut self, why: &str) {
        for (_, mut ow) in std::mem::take(&mut self.owned) {
            if let Some(sc) = ow.sched.as_mut() {
                sc.shutdown(&mut ow.w, why);
            }
        }
    }

    /// The calls that need the owner itself: they take control-session locks.
    pub fn job(&mut self, call: Call, pol: &OutcomePolicy) -> Result<Value, String> {
        // the endpoint verified the call's surface is the link's; the gateway decides here again
        // (defence in depth) and writes the allowed create into the new outcome's chain
        let by = call
            .surface()
            .map(Principal::User)
            .unwrap_or(Principal::Engine);
        if !call.action().is_some_and(|a| gateway_allows(by, a)) {
            return Err("the owner's policy table does not allow this call".into());
        }
        match call {
            Call::OutcomeCreate {
                workspace,
                base_commit,
                contract,
                idempotency_key,
                ..
            } => self.create(&workspace, base_commit, &contract, idempotency_key, pol, by),
            Call::OrchestratorEnable { .. } => self.enable(pol),
            _ => Err("not an owner call".into()),
        }
    }

    /// A retry of a create (same key, same request) returns the first reply instead of a second
    /// draft. The record is in memory: an owner that restarted in between forgets it, and the retry
    /// makes another draft that only waits for its approval (documented residual).
    fn create(
        &mut self,
        workspace: &str,
        base_commit: String,
        contract: &Contract,
        key: Option<String>,
        pol: &OutcomePolicy,
        by: Principal,
    ) -> Result<Value, String> {
        let Some(key) = key else {
            return self.make(workspace, base_commit, contract, pol, by);
        };
        if !(8..=128).contains(&key.len())
            || !key
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"-_.:".contains(&b))
        {
            return Err(
                "idempotency_key must be 8 to 128 characters of A-Z a-z 0-9 - _ . :".into(),
            );
        }
        let fp = format!(
            "{:x}",
            Sha256::digest(json!([workspace, base_commit, contract]).to_string())
        );
        if let Some((_, f, reply)) = self.created.iter().find(|c| c.0 == key) {
            return if *f == fp {
                Ok(reply.clone())
            } else {
                Err("the idempotency key was already used by a different request".into())
            };
        }
        let reply = self.make(workspace, base_commit, contract, pol, by)?;
        if self.created.len() >= KEPT_CREATES {
            self.created.pop_front();
        }
        self.created.push_back((key, fp, reply.clone()));
        Ok(reply)
    }

    fn make(
        &mut self,
        workspace: &str,
        base_commit: String,
        contract: &Contract,
        pol: &OutcomePolicy,
        by: Principal,
    ) -> Result<Value, String> {
        // a read-only goal runs in place in any folder: no commit, no git (S1 section 2.6); a
        // write goal keeps the commit rule
        let read_only = contract.kind == ContractKind::ReadOnly;
        if read_only {
            if !base_commit.is_empty() {
                return Err("a read-only goal has no base commit".into());
            }
        } else if !commit_id(&base_commit) {
            return Err("base_commit must be a full lowercase commit id".into());
        }
        let root = Path::new(workspace);
        if !root.is_absolute() || !root.is_dir() {
            return Err("workspace must be an existing absolute directory".into());
        }
        if read_only {
            // an unknown home gives an empty path, which the check refuses
            let user = crate::homes(&Env::from_process())
                .map(|h| h.user)
                .unwrap_or_default();
            if let Some(why) = read_only_folder_refusal(root, &user, &self.home) {
                return Err(why);
            }
        } else {
            self.wm.verify_base(root, &base_commit).map_err(s)?;
        }
        let art = self.home.join("artifacts");
        let open = |id: String| {
            WorkspaceContext::open(id, root, root, &[], art.join("outcome"), "orchestrator")
        };
        // the workspace id is the folder's key, as `outcome.list {workspace}` filters by it
        let key = open("probe".into()).map_err(s)?.workspace_key;
        let ws = open(key).map_err(s)?;
        let mut w = self
            .store
            .create_outcome(NewOutcome {
                workspace: &ws,
                base_commit,
                contract,
                policy: pol,
            })
            .map_err(s)?;
        // the contract waits for the person's approval
        let rev = w.get_outcome(w.id()).map_err(s)?.revision;
        w.transition_outcome(
            rev,
            OutcomeState::ContractPending,
            None,
            &Effects::default(),
        )
        .map_err(s)?;
        let a = audit(by, Some(Action::CreateOutcome));
        w.audit_policy("outcome.create", "allow", a["detail"].clone())
            .map_err(s)?;
        let (_, hash, _) = w.get_contract(w.id(), contract.version).map_err(s)?;
        Ok(
            json!({"id": w.id(), "state": "contract_pending", "contract_version": contract.version,
                  "contract_hash": hash}),
        )
    }

    /// The explicit migration of ADR-016 item 9: the store goes to schema v5 (a consistent backup
    /// first, `backups/pre-v5-*`). It does not change settings: `synt.orchestrator.enabled` must
    /// already be true, which is the person's own step.
    fn enable(&self, pol: &OutcomePolicy) -> Result<Value, String> {
        if !self.enable_gate_open {
            return Err(synthesis_orchestrator::GATE_REFUSAL.into());
        }
        if !pol.enabled {
            return Err("set synt.orchestrator.enabled to true in the user settings first".into());
        }
        Store::open(
            &self.home,
            StoreOptions {
                schema_target: SCHEMA_VERSION_OUTCOMES,
                write_node_transcripts: false,
                ..StoreOptions::default()
            },
        )
        .map_err(s)?;
        // surfaces show this text before they say "enabled" and never word it themselves
        Ok(
            json!({"schema": SCHEMA_VERSION_OUTCOMES, "backups": "backups/pre-v5-*",
                  "disclosure": {"id": DISCLOSURE_ID, "text": INTEGRITY_DISCLOSURE}}),
        )
    }
}

/// A read-only worker reads its whole folder, so the folder must not be, hold or sit above the
/// user's home, Synthesis's own home (state, keys) or the filesystem root, and must not be, hold or
/// sit inside a credential store (`~/.config`, `~/.docker`, `~/Library`, `~/.kube` ...: the
/// `--sandbox-read` check). An unknown home fails closed. `None`: allowed. Used when the goal is
/// created and again when an attempt starts (the folder may have been swapped for a symlink).
pub(crate) fn read_only_folder_refusal(root: &Path, user: &Path, synt: &Path) -> Option<String> {
    if user.as_os_str().is_empty() {
        return Some("cannot tell where your home folder is (HOME or USERPROFILE is not set), so no folder can be checked".into());
    }
    let real = |p: &Path| synthesis_core::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    let root = match synthesis_core::canonicalize(root) {
        Ok(r) => r,
        Err(e) => return Some(format!("{}: {e}", root.display())),
    };
    let (user, synt) = (real(user), real(synt));
    let mut deny = synthesis_sandbox::credential_dirs(&user);
    deny.extend(synthesis_sandbox::synthesis_state_paths(&synt));
    let policy = synthesis_sandbox::SandboxPolicy {
        deny_paths: deny,
        ..Default::default()
    };
    let credential = policy.check_read_root(&root).err();
    let holds = |h: &Path| !h.as_os_str().is_empty() && h.starts_with(&root);
    let wide = root.parent().is_none()
        || holds(&user)
        || holds(&synt)
        || (!synt.as_os_str().is_empty() && root.starts_with(&synt));
    (wide || credential.is_some()).then(|| {
        format!(
            "a read-only goal needs a project folder: not your home folder, a folder that holds it, the filesystem root, a credential folder or Synthesis's own folder{}",
            credential.map_or(String::new(), |c| format!(" ({c})"))
        )
    })
}

/// The notifications an outcome owes the person now (ADR-021 section 4): ids and codes only, once
/// per `(decision, revision)` and per `(outcome, state, revision)`. `sent` is the in-memory first
/// filter; the durable one is an `outbox` row of kind `notify` keyed by the same facts, written
/// and marked `done` BEFORE the notice is returned, so a restart (or a crash after the mark and
/// before the hook ran) loses at most that one notice and never tells twice. `needs_you` is
/// derived from rows, so a lost notice does not hide the decision.
pub fn due_notices(
    w: &mut OutcomeWriter,
    o: &OutcomeRecord,
    sent: &mut BTreeSet<String>,
) -> Vec<Value> {
    let ts = OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .unwrap_or_default();
    let oid = o.id.to_string();
    let mut out = vec![];
    let mut tell = |w: &mut OutcomeWriter, key: String, mut p: Value| {
        if sent.insert(key.clone()) && told_once(w, &key, &p) {
            p["ts"] = json!(ts);
            out.push(p);
        }
    };
    for d in w.list_decisions(w.id()).unwrap_or_default() {
        if w.decision_state(&d.id).ok() == Some(DecisionState::Pending) {
            let did = d.id.to_string();
            let p = outcome_payload(
                "decision.created",
                &oid,
                Some((&did, &word(&d.kind))),
                "decision_pending",
                "",
            );
            tell(w, format!("decision.created:{did}:{}", d.revision), p);
        }
    }
    let (event, code) = match o.state {
        OutcomeState::Blocked => ("outcome.blocked", "blocked"),
        OutcomeState::Verified => ("outcome.verified", "verified"),
        _ => return out,
    };
    tell(
        w,
        format!("{event}:{oid}:{}", o.revision),
        outcome_payload(event, &oid, None, code, ""),
    );
    out
}

/// Records that the notice `key` is being told, as a `done` outbox row: `true` only for the call
/// that created it. Any failure answers `false` (the notice is not told: fail toward silence).
fn told_once(w: &mut OutcomeWriter, key: &str, payload: &Value) -> bool {
    let row = NewOutbox {
        idempotency_key: format!("notify:{key}"),
        kind: OutboxKind::Notify,
        payload: payload.clone(),
    };
    let first = w.atomically(|tx| tx.enqueue(&row)).unwrap_or(false);
    let only = OutboxFilter::one(&[OutboxKind::Notify], &row.idempotency_key);
    first
        && matches!(w.claim_outbox(OffsetDateTime::now_utc(), &only), Ok(Some(r))
            if w.complete_outbox(&r.id, "told").is_ok())
}

// ------------------------------------------------------------------ process

/// The environment of a worker: `child_env` (allowlist) plus only what its provider needs, the
/// `SYNT_API_KEY` / `SYNT_AUTH_TOKEN` pair (ADR-011) and the variables the user's own provider
/// entries name as `{env:NAME}`. The rest of the environment of whichever surface started the
/// orchestrator (cloud tokens, ...) never reaches a worker.
pub fn worker_env(env: &Env, home: &Path) -> Vec<(OsString, OsString)> {
    let mut out: BTreeMap<String, String> = env
        .child_env()
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    let named = ["SYNT_API_KEY".to_string(), "SYNT_AUTH_TOKEN".to_string()]
        .into_iter()
        .chain(provider_env_names(home));
    for n in named {
        if let Some(v) = env.get(&n) {
            out.insert(n, v.to_string());
        }
    }
    out.into_iter().map(|(k, v)| (k.into(), v.into())).collect()
}

/// The `NAME`s of every whole-string `{env:NAME}` in the `providers` of the user's `synt.json`.
fn provider_env_names(home: &Path) -> Vec<String> {
    fn walk(v: &Value, out: &mut Vec<String>) {
        match v {
            Value::String(s) => {
                if let Some(n) = s.strip_prefix("{env:").and_then(|r| r.strip_suffix('}'))
                    && !n.is_empty()
                    && n.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
                {
                    out.push(n.to_string());
                }
            }
            Value::Array(a) => a.iter().for_each(|x| walk(x, out)),
            Value::Object(o) => o.values().for_each(|x| walk(x, out)),
            _ => {}
        }
    }
    let mut out = vec![];
    let doc = std::fs::read(home.join("synt.json"))
        .ok()
        .filter(|b| b.len() <= 1 << 20)
        .and_then(|b| serde_json::from_slice::<Value>(&b).ok());
    if let Some(p) = doc.as_ref().and_then(|d| d.get("providers")) {
        walk(p, &mut out);
    }
    out
}

/// Removes the socket a dead orchestrator left, and only that: a direct child of `run/` named
/// like the listener names its sockets (16 hex digits + `.sock`), a socket (not a link or a file),
/// owned by this user with mode 0600, that nobody listens on. A registry entry that names anything
/// else is left alone. (A Windows pipe goes away with its owner.)
#[cfg(unix)]
fn remove_stale_socket(run: &Path, old: &str) {
    use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
    let p = Path::new(old);
    let hex16 = |n: &str| {
        n.strip_suffix(".sock").is_some_and(|h| {
            h.len() == 16 && h.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
        })
    };
    let (Ok(m), Ok(dir)) = (std::fs::symlink_metadata(p), std::fs::metadata(run)) else {
        return;
    };
    if p.parent() == Some(run)
        && p.file_name().and_then(|n| n.to_str()).is_some_and(hex16)
        && m.file_type().is_socket()
        && m.uid() == dir.uid()
        && m.permissions().mode() & 0o777 == 0o600
        && std::os::unix::net::UnixStream::connect(p).is_err()
    {
        let _ = std::fs::remove_file(p);
    }
}

#[cfg(not(unix))]
fn remove_stale_socket(_run: &Path, _old: &str) {}

/// The notify hook, when the person configured one and a confining sandbox exists; otherwise
/// notifications are off (said once), never run unconfined.
fn notifier(env: &Env, h: &crate::Homes) -> Option<Arc<Notifier>> {
    let cfg = match crate::notify::read_config(
        &h.synthesis,
        &synthesis_settings::managed_settings_path(),
    ) {
        Ok(Some(c)) => c,
        Ok(None) => return None,
        Err(e) => {
            log(&format!("notifications are off: {e}"));
            return None;
        }
    };
    let started = (|| {
        let dir = h.synthesis.join("orch-notify");
        std::fs::create_dir_all(&dir).map_err(s)?;
        let ws = Arc::new(
            WorkspaceContext::open("orchestrator", &dir, &dir, &[], dir.clone(), "orchestrator")
                .map_err(s)?,
        );
        let mut policy =
            synthesis_tools::standard_policy_with(&ws, synthesis_sandbox::NetworkPolicy::Deny)
                .ok_or("no sandbox policy")?;
        policy
            .deny_paths
            .extend(synthesis_sandbox::synthesis_state_paths(&h.synthesis));
        let rt = Arc::new(synthesis_terminal::TerminalRuntime::new(
            synthesis_terminal::SandboxMode::Enforce,
        ));
        let child: BTreeMap<String, String> = env
            .child_env()
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        Notifier::start_for_outcomes(&cfg, rt, "orchestrator", ws, &child, Some(policy))
    })();
    match started {
        Ok(n) => Some(n),
        Err(e) => {
            log(&format!("notifications are off: {e}"));
            None
        }
    }
}

/// `synt orchestrator --serve`. Returns when it was idle for `idle_exit_minutes`, when outcomes
/// were turned off, or on SIGINT/SIGTERM. `Ok` is also returned when another orchestrator already
/// serves this home (the lock decides).
pub fn serve(env: Env) -> Result<(), String> {
    serve_with(env, Arc::new(AtomicBool::new(false)), None)
}

/// [`serve`] with an outside stop flag and, for tests, the program workers run (default: this
/// binary).
pub fn serve_with(env: Env, stop: Arc<AtomicBool>, program: Option<PathBuf>) -> Result<(), String> {
    let h = crate::homes(&env)?;
    let home = h.synthesis.clone();
    let st = settings_of(&env, &h);
    if !outcome_policy(&st.synthesis.orchestrator).enabled {
        return Err("outcomes are off (synt.orchestrator.enabled is false): not starting".into());
    }
    // Windows: run/ and orch.key are protected by the ACL they inherit from the user profile
    let profile = env.get_nonempty("USERPROFILE").map(PathBuf::from);
    if !platform_ok(&home, profile.as_deref()) {
        return Err("SYNT_HOME must be under your user profile on Windows".into());
    }
    let run = run_dir(&home);
    prepare_run_dir(&run).map_err(s)?;
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(run.join(ep::LOCK_FILE))
        .map_err(s)?;
    match lock.try_lock() {
        Ok(()) => {}
        Err(std::fs::TryLockError::WouldBlock) => {
            log("another orchestrator already serves this home");
            return Ok(());
        }
        Err(std::fs::TryLockError::Error(e)) => return Err(s(e)),
    }
    let key = ep::load_key(&home).map_err(s)?;
    // the lock is ours, so a socket the registry still names belongs to a dead owner
    if let Some(old) = ep::read_registry(&home) {
        remove_stale_socket(&run, &old);
    }
    // everything that can fail is built before the endpoint is published
    let store = Store::open(
        &home,
        StoreOptions {
            write_node_transcripts: false,
            ..StoreOptions::default()
        },
    )
    .map_err(s)?;
    let wm = crate::wt::manager(&env, &home)?;
    let spawn = SyntSpawn {
        log: run.join("orch-worker.log"),
        env: Some(worker_env(&env, &home)),
    };
    let program = match program {
        Some(p) => p,
        None => std::env::current_exe().map_err(s)?,
    };
    let mut owner = Owner::new(&home, &store, &wm, &spawn, program, notifier(&env, &h));
    #[cfg(feature = "test-provider")]
    {
        if let Some(script) = env.get_nonempty("SYNT_TEST_ORCH_SCRIPT") {
            owner.test.worker_args = vec!["--provider".into(), format!("fake:{script}")];
        }
        owner.test.hold = env.get_nonempty("SYNT_TEST_ORCH_HOLD").map(PathBuf::from);
    }
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .map_err(s)?;
    let (tx, rx) = std::sync::mpsc::sync_channel::<Job>(ep::JOB_QUEUE);
    let (push, _) = tokio::sync::broadcast::channel(ep::PUSH_QUEUE);
    owner.set_push(push.clone());
    let ctx = Arc::new(Ctx {
        env: env.clone(),
        push,
        home: home.clone(),
        jobs: tx,
        link_idle: ep::LINK_IDLE,
        clients: AtomicUsize::new(0),
        last_call: std::sync::atomic::AtomicU64::new(ep::now_secs()),
    });
    let listener = {
        let _g = rt.enter();
        Listener::bind(&run).map_err(s)?
    };
    let endpoint = listener.endpoint();
    ep::write_registry(&home, &endpoint).map_err(s)?;
    rt.spawn(ep::serve(listener, key, ctx.clone()));
    let flag = stop.clone();
    rt.spawn(async move {
        wait_for_signal().await;
        flag.store(true, Ordering::SeqCst);
    });

    let result = run_loop(&mut owner, &env, &h, &rx, &ctx, &stop);
    owner.release_all("the orchestrator is stopping");
    if let Some(n) = &owner.notifier {
        n.flush();
        n.close();
    }
    ep::clear_registry(&home, &endpoint);
    drop(rt);
    drop(lock);
    result
}

fn run_loop(
    owner: &mut Owner<'_>,
    env: &Env,
    h: &crate::Homes,
    rx: &std::sync::mpsc::Receiver<Job>,
    ctx: &Ctx,
    stop: &AtomicBool,
) -> Result<(), String> {
    let mut said: Option<String> = None;
    let mut unprotected: Vec<String> = vec![];
    while !stop.load(Ordering::SeqCst) {
        let st = settings_of(env, h);
        let orch = &st.synthesis.orchestrator;
        let pol = outcome_policy(orch);
        if !pol.enabled {
            log("outcomes were turned off: stopping");
            return Ok(());
        }
        // the planner settings of this cycle (user or managed only); `None` keeps the stub plan
        owner.max_items = orch.planner.max_items as usize;
        owner.planner = (orch.planner.mode == PlannerMode::Model).then(|| PlannerCtx {
            settings: orch.planner.clone(),
            env: env.clone(),
            homes: crate::Homes {
                user: h.user.clone(),
                synthesis: h.synthesis.clone(),
            },
        });
        // names only, said when the set changes
        let open = learn_secrets(&owner.redactor, env, h);
        if open != unprotected {
            if !open.is_empty() {
                log(&format!(
                    "the key of provider {} is not resolved: only pattern redaction applies to it in reports",
                    open.join(", ")
                ));
            }
            unprotected = open;
        }
        // a job wakes the loop early; the rest of the cycle is the same
        match rx.recv_timeout(TICK) {
            Ok((call, reply)) => {
                let _ = reply.send(owner.job(call, &pol));
                // the rest of the queue too: one job per cycle would delay the later ones
                while let Ok((call, reply)) = rx.try_recv() {
                    let _ = reply.send(owner.job(call, &pol));
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => return Ok(()),
        }
        // a store that is not migrated yet fails every cycle the same way: said once
        match owner.cycle(&pol) {
            Ok(()) => said = None,
            Err(e) if said.as_deref() != Some(&e) => {
                log(&format!("cycle: {e}"));
                said = Some(e);
            }
            Err(_) => {}
        }
        if idle_over(
            owner.owned_count(),
            ctx.clients.load(Ordering::SeqCst),
            ctx.last_call.load(Ordering::SeqCst),
            ep::now_secs(),
            orch.idle_exit_minutes,
        ) {
            return Ok(());
        }
    }
    Ok(())
}

/// D20: no owned outcome, no client and no call for `minutes`.
fn idle_over(owned: usize, clients: usize, last_call: u64, now: u64, minutes: u32) -> bool {
    owned == 0 && clients == 0 && now.saturating_sub(last_call) >= u64::from(minutes) * 60
}

async fn wait_for_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        match signal(SignalKind::terminate()) {
            Ok(mut t) => {
                tokio::select! {
                    _ = tokio::signal::ctrl_c() => {}
                    _ = t.recv() => {}
                }
            }
            Err(_) => {
                let _ = tokio::signal::ctrl_c().await;
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `ro-folder-refused`: a read-only goal cannot name the home folder, a folder that holds it,
    /// the filesystem root, or Synthesis's own folder (or anything in it).
    #[test]
    fn a_read_only_folder_is_never_home_above_home_root_or_synthesis_state() {
        let t = tempfile::tempdir().unwrap();
        let base = synthesis_core::canonicalize(t.path()).unwrap();
        let (home, synt) = (base.join("u"), base.join("u/.synt"));
        let proj = home.join("code/proj");
        std::fs::create_dir_all(&proj).unwrap();
        std::fs::create_dir_all(&synt).unwrap();
        let refused = |p: &Path| read_only_folder_refusal(p, &home, &synt).is_some();
        assert!(!refused(&proj), "a project below home is fine");
        for bad in [&home, &base, &synt, &synt.join("sessions")] {
            if bad.exists() {
                assert!(refused(bad), "{bad:?}");
            }
        }
        assert!(refused(Path::new("/")));
        // credential stores chosen directly, and a folder that holds one
        for d in [".config", ".docker", ".kube", "Library", ".config/gh"] {
            std::fs::create_dir_all(home.join(d)).unwrap();
        }
        std::fs::create_dir_all(home.join("Library/Keychains")).unwrap();
        for d in [".config", ".docker", ".kube", "Library", ".config/gh"] {
            assert!(refused(&home.join(d)), "{d}");
        }
        // an unknown home fails closed
        assert!(
            read_only_folder_refusal(&proj, Path::new(""), &synt)
                .is_some_and(|m| m.contains("home folder"))
        );
        // a project folder reached through a symlink to home is home
        #[cfg(unix)]
        {
            let link = base.join("link");
            std::os::unix::fs::symlink(&home, &link).unwrap();
            assert!(refused(&link));
        }
    }

    #[test]
    fn idle_exit_needs_no_outcome_no_client_and_quiet_minutes() {
        assert!(idle_over(0, 0, 1000, 1000 + 600, 10));
        assert!(
            !idle_over(0, 0, 1000, 1000 + 599, 10),
            "not quiet long enough"
        );
        assert!(!idle_over(1, 0, 0, 10_000, 10), "an outcome is owned");
        assert!(!idle_over(0, 1, 0, 10_000, 10), "a client is attached");
        assert!(
            !idle_over(0, 0, 2000, 1000, 10),
            "a clock set back is not idle"
        );
    }

    #[test]
    fn a_commit_id_is_full_lowercase_hex() {
        assert!(commit_id(&"a".repeat(40)) && commit_id(&"0".repeat(64)));
        for bad in ["", "abc", &"A".repeat(40), &"g".repeat(40), &"a".repeat(41)] {
            assert!(!commit_id(bad), "{bad}");
        }
    }

    #[test]
    fn provider_keys_are_masked_and_an_unresolved_one_keeps_pattern_redaction() {
        let home = tempfile::tempdir().unwrap();
        std::fs::write(
            home.path().join("synt.json"),
            r#"{"$schema":"config.json/v1","providers":{"gw":{"type":"openai","baseURL":"http://x/v1","apiKey":"{env:GW_KEY}"},"gone":{"type":"openai","baseURL":"http://x/v1","apiKey":"{env:GONE_KEY}"}}}"#,
        )
        .unwrap();
        let h = crate::Homes {
            user: home.path().to_path_buf(),
            synthesis: home.path().to_path_buf(),
        };
        // a test-only value that no token pattern matches
        let key = "zq-plain-owner-key-77";
        let env = Env::from_pairs([("PATH", "/usr/bin"), ("GW_KEY", key)]);
        let r = synthesis_events::Redactor::new([]);
        let ghp = format!("{}_{}", "ghp", "Z".repeat(36));
        let text = format!("k {key} t {ghp}");
        assert!(r.redact_str(&text).contains(key));
        let open = learn_secrets(&r, &env, &h);
        let shown = r.redact_str(&text);
        assert!(
            !shown.contains(key) && !shown.contains(&ghp),
            "{shown} {open:?}"
        );
        // the unresolved reference is named (never a value) and does not switch anything off
        assert_eq!(open, ["gone"]);
    }

    #[test]
    fn a_set_but_unprotectable_key_is_named_without_any_warning_and_a_header_is_masked() {
        let home = tempfile::tempdir().unwrap();
        std::fs::write(
            home.path().join("synt.json"),
            r#"{"$schema":"config.json/v1","providers":{"short":{"type":"openai","baseURL":"http://x/v1","apiKey":"abc"},"hdr":{"type":"openai","baseURL":"http://x/v1","apiKey":"fine-enough-key-1","headers":{"x-org":"zq-header-only-secret-5"}}}}"#,
        )
        .unwrap();
        let h = crate::Homes {
            user: home.path().to_path_buf(),
            synthesis: home.path().to_path_buf(),
        };
        let r = synthesis_events::Redactor::new([]);
        let open = learn_secrets(&r, &Env::from_pairs([("PATH", "/usr/bin")]), &h);
        assert_eq!(open, ["short"]);
        assert!(
            !r.redact_str("h zq-header-only-secret-5")
                .contains("zq-header")
        );
    }

    #[test]
    fn the_runner_and_the_verifier_share_the_owner_redactor() {
        // shortcut: `build` needs a store, an approved contract and a worker host, so the wiring is
        // pinned at the source (as `nothing_bypasses_csp` does); upgrade with an owner-level rig
        let src = include_str!("orch_owner.rs");
        for line in [
            "verifier.redactor = self.redactor.clone();",
            "runner.redactor = self.redactor.clone();",
        ] {
            assert_eq!(src.matches(line).count(), 2, "{line}"); // the code and this list
        }
        let owner = synthesis_events::Redactor::new([]);
        let (runner, verifier) = (owner.clone(), owner.clone());
        assert!(owner.add_secret_min("zq-late-owner-secret-3", 8).is_ok());
        for r in [&runner, &verifier] {
            assert!(!r.redact_str("zq-late-owner-secret-3").contains("zq-late"));
        }
    }

    #[test]
    fn a_worker_gets_the_filtered_env_and_only_the_provider_keys() {
        let home = tempfile::tempdir().unwrap();
        std::fs::write(
            home.path().join("synt.json"),
            r#"{"providers":{"gw":{"apiKey":"{env:GW_KEY}","headers":{"x":"{env:GW_HDR}"}}},"other":"{env:NOT_A_PROVIDER}"}"#,
        )
        .unwrap();
        let env = Env::from_pairs([
            ("PATH", "/usr/bin"),
            ("FAKE_CLOUD_TOKEN", "cloud"),
            ("AWS_SECRET_ACCESS_KEY", "aws"),
            ("NOT_A_PROVIDER", "x"),
            ("SYNT_API_KEY", "key1"),
            ("SYNT_PROVIDER", "gw"),
            ("GW_KEY", "key2"),
            ("GW_HDR", "hdr"),
        ]);
        let got: BTreeMap<_, _> = worker_env(&env, home.path())
            .into_iter()
            .map(|(k, v)| {
                (
                    k.to_string_lossy().into_owned(),
                    v.to_string_lossy().into_owned(),
                )
            })
            .collect();
        for gone in [
            "FAKE_CLOUD_TOKEN",
            "AWS_SECRET_ACCESS_KEY",
            "NOT_A_PROVIDER",
        ] {
            assert!(!got.contains_key(gone), "{gone} reached the worker");
        }
        for (k, v) in [
            ("PATH", "/usr/bin"),
            ("SYNT_API_KEY", "key1"),
            ("SYNT_PROVIDER", "gw"),
            ("GW_KEY", "key2"),
            ("GW_HDR", "hdr"),
        ] {
            assert_eq!(got.get(k).map(String::as_str), Some(v), "{k}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn only_the_orchestrators_own_stale_socket_is_removed() {
        use std::os::unix::fs::PermissionsExt;
        use std::os::unix::net::UnixListener;
        let d = tempfile::Builder::new()
            .prefix("o")
            .tempdir_in("/tmp")
            .unwrap();
        let run = d.path().join("run");
        std::fs::create_dir_all(run.join("sub")).unwrap();
        let sock = |p: &Path, mode: u32| {
            drop(UnixListener::bind(p).unwrap()); // the file stays, nobody listens
            std::fs::set_permissions(p, std::fs::Permissions::from_mode(mode)).unwrap();
        };
        let ours = run.join("0123456789abcdef.sock");
        sock(&ours, 0o600);
        let odd_name = run.join("peer.sock");
        sock(&odd_name, 0o600);
        let wide = run.join("fedcba9876543210.sock");
        sock(&wide, 0o666);
        let nested = run.join("sub/0123456789abcdef.sock");
        sock(&nested, 0o600);
        let file = run.join("aaaaaaaaaaaaaaaa.sock");
        std::fs::write(&file, "x").unwrap();
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600)).unwrap();
        let live = run.join("bbbbbbbbbbbbbbbb.sock");
        let _listening = UnixListener::bind(&live).unwrap();
        std::fs::set_permissions(&live, std::fs::Permissions::from_mode(0o600)).unwrap();
        for kept in [&odd_name, &wide, &nested, &file, &live] {
            remove_stale_socket(&run, &kept.to_string_lossy());
            assert!(kept.exists(), "{kept:?} must stay");
        }
        remove_stale_socket(&run, &ours.to_string_lossy());
        assert!(!ours.exists());
    }
}
