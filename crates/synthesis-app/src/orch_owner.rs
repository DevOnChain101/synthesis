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
use synthesis_core::outcome::{Contract, DecisionState, OutcomeId, OutcomeState};
use synthesis_orchestrator::{
    Allocator, Control, DecisionDriver, IndependentVerifier, Platform, ProcessHost, Runner,
    SandboxRunner, SchedConfig, Scheduler, Stop, StubPlanner, SyntSpawn, answered_waiting,
    apply_lifecycle, apply_queued, conclude, recover_attempts, restore_pause,
};
use synthesis_peers::net::{Listener, prepare_run_dir};
use synthesis_peers::{platform_ok, run_dir};
use synthesis_session::outcome::{
    Effects, NewOutcome, OutcomePolicy, OutcomeRecord, OutcomeWriter,
};
use synthesis_session::{
    Error as StoreError, OsProbe, Reader, SCHEMA_VERSION_OUTCOMES, Store, StoreOptions,
};
use synthesis_workspace::WorktreeManager;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use crate::notify::{Notifier, outcome_payload};
use crate::orch_endpoint::{self as ep, Ctx, Job};
use crate::outcome_policy::outcome_policy;
use crate::outcome_service::settings_of;
use crate::proto::Call;

/// Pause between cycles of the owner loop (a job wakes it earlier).
const TICK: Duration = Duration::from_millis(500);
/// How long an attempt's worker may be silent before a new owner treats it as dead.
const LEASE: Duration = Duration::from_secs(120);
/// Longest one independent check may run.
const CHECK_TIMEOUT: Duration = Duration::from_secs(600);
/// Most `outcome.create` replies kept for a retry with the same key (in memory).
const KEPT_CREATES: usize = 256;
/// The command kinds an owner applies; anything else does not make an outcome worth owning.
const KINDS: [&str; 7] = [
    "decision.resolve",
    "outcome.contract.approve",
    "outcome.start",
    "outcome.pause",
    "outcome.resume",
    "outcome.cancel",
    "outcome.ack",
];

fn s<E: ToString>(e: E) -> String {
    e.to_string()
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
    /// `(idempotency key, request fingerprint, reply)` of the latest creates, oldest first.
    created: VecDeque<(String, String, Value)>,
    #[cfg(feature = "test-provider")]
    pub test: TestHooks,
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
            created: VecDeque::new(),
            #[cfg(feature = "test-provider")]
            test: TestHooks::default(),
        }
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
            if self.owned.contains_key(&id) || !(active || has_work(&r, &o.id).map_err(s)?) {
                continue;
            }
            match self.store.open_outcome(&o.id, pol) {
                Ok(mut w) => {
                    let control = Control::default();
                    self.adopt(&mut w, &control, &o, pol);
                    self.owned.insert(
                        id,
                        Owned {
                            w,
                            control,
                            sched: None,
                            refused: None,
                            unblocked_for: BTreeSet::new(),
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

    /// Returns whether the outcome stays owned.
    fn step(&mut self, ow: &mut Owned<'a>, pol: &OutcomePolicy) -> bool {
        let now = OffsetDateTime::now_utc();
        let id = ow.w.id().to_string();
        if let Err(e) = apply_queued(&mut ow.w, now) {
            log(&format!("outcome {id}: commands: {e}"));
        }
        if let Err(e) = apply_lifecycle(&mut ow.w, &ow.control, &StubPlanner) {
            log(&format!("outcome {id}: commands: {e}"));
        }
        let Ok(mut o) = ow.w.get_outcome(ow.w.id()) else {
            return true;
        };
        // A person's answer to a decision moves a `blocked` outcome by itself: it goes back to
        // `running` once per answer and the scheduler re-checks everything, so anything else that
        // still blocks (a cap, another item) blocks it again with its own reason (fail closed). A
        // paused or cancelled outcome is left alone: only `resume` undoes a person's pause.
        if o.state == OutcomeState::Blocked
            && !ow.control.is_paused()
            && !ow.control.cancel.is_set()
        {
            let fresh: Vec<String> = answered_waiting(&mut ow.w)
                .unwrap_or_default()
                .into_iter()
                .filter(|d| ow.unblocked_for.insert(d.clone()))
                .collect();
            if !fresh.is_empty() {
                match ow.w.transition_outcome(
                    o.revision,
                    OutcomeState::Running,
                    Some("a decision was answered"),
                    &Effects::default(),
                ) {
                    Ok(_) => {
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
                            sc.decisions = Some(Box::new(DecisionDriver::default()));
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
        self.notices(&ow.w, &o);
        let keep = matches!(o.state, OutcomeState::Running | OutcomeState::Verifying)
            || has_work(&ow.w, ow.w.id()).unwrap_or(true);
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
        let verifier = IndependentVerifier::for_outcome(
            w,
            PathBuf::from(&o.repo_root),
            self.home.clone(),
            Arc::new(SandboxRunner::detect(self.home.clone())),
            CHECK_TIMEOUT,
        )
        .map_err(s)?;
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
        let host = ProcessHost {
            allocator: Allocator {
                store: self.store,
                worktrees: self.wm,
                platform: Platform::current(),
                artifact_root: self.home.join("artifacts"),
            },
            runner: Runner::new(
                self.store,
                self.wm,
                self.spawn,
                &OsProbe,
                Duration::from_millis(250),
                Duration::from_secs(10),
            ),
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
    fn notices(&mut self, w: &OutcomeWriter, o: &OutcomeRecord) {
        let Some(n) = self.notifier.clone() else {
            return;
        };
        for p in due_notices(w, o, &mut self.sent) {
            n.send_outcome(p);
        }
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
        match call {
            Call::OutcomeCreate {
                workspace,
                base_commit,
                contract,
                idempotency_key,
                ..
            } => self.create(&workspace, base_commit, &contract, idempotency_key, pol),
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
    ) -> Result<Value, String> {
        let Some(key) = key else {
            return self.make(workspace, base_commit, contract, pol);
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
        let reply = self.make(workspace, base_commit, contract, pol)?;
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
    ) -> Result<Value, String> {
        if !commit_id(&base_commit) {
            return Err("base_commit must be a full lowercase commit id".into());
        }
        let root = Path::new(workspace);
        if !root.is_absolute() || !root.is_dir() {
            return Err("workspace must be an existing absolute directory".into());
        }
        self.wm.verify_base(root, &base_commit).map_err(s)?;
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
        Ok(json!({"schema": SCHEMA_VERSION_OUTCOMES, "backups": "backups/pre-v5-*"}))
    }
}

/// The notifications an outcome owes the person now (ADR-021 section 4): ids and codes only, once
/// per `(decision, revision)` and per `(outcome, state, revision)` in `sent`. Dedupe is in memory:
/// a restarted orchestrator may tell again.
pub fn due_notices(
    w: &OutcomeWriter,
    o: &OutcomeRecord,
    sent: &mut BTreeSet<String>,
) -> Vec<Value> {
    let ts = OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .unwrap_or_default();
    let oid = o.id.to_string();
    let mut out = vec![];
    for d in w.list_decisions(w.id()).unwrap_or_default() {
        if w.decision_state(&d.id).ok() == Some(DecisionState::Pending)
            && sent.insert(format!("{}:{}", d.id, d.revision))
        {
            let did = d.id.to_string();
            out.push(outcome_payload(
                "decision.created",
                &oid,
                Some((&did, &word(&d.kind))),
                "decision_pending",
                &ts,
            ));
        }
    }
    let (event, code) = match o.state {
        OutcomeState::Blocked => ("outcome.blocked", "blocked"),
        OutcomeState::Verified => ("outcome.verified", "verified"),
        _ => return out,
    };
    if sent.insert(format!("{oid}:{code}:{}", o.revision)) {
        out.push(outcome_payload(event, &oid, None, code, &ts));
    }
    out
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
    let ctx = Arc::new(Ctx {
        env: env.clone(),
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
    while !stop.load(Ordering::SeqCst) {
        let st = settings_of(env, h);
        let orch = &st.synthesis.orchestrator;
        let pol = outcome_policy(orch);
        if !pol.enabled {
            log("outcomes were turned off: stopping");
            return Ok(());
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
