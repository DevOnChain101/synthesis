//! The server side of the outcome methods of protocol v4 (ADR-021 section 1). It reads the store
//! and queues commands; it never applies one. The owner of an outcome applies them
//! (`synthesis_orchestrator::apply_queued`), so a call returns `queued` until an owner has run.
//!
//! Who may write: only the orchestrator endpoint, whose clients proved `run/orch.key` and attached
//! as a surface (`orch_endpoint`), gets a [`Service::new`]. The stdio engine gets
//! [`Service::read_only`]: an agent shell running `synt engine --stdio` can read what the store
//! allows but cannot answer a decision, approve a contract or start anything. This is an
//! ownership and correctness boundary, not a security one (ADR-017): any program of the same OS
//! user outside the sandbox can read the key.
//!
//! Off by default: while `synt.orchestrator.enabled` is false every method but
//! `orchestrator.status` is refused and nothing is opened, created or written.

use std::path::{Path, PathBuf};

use serde_json::{Value, json};
use synthesis_core::outcome::{DecisionId, DecisionState, OutcomeId, OutcomeState, WorkItemId};
use synthesis_session::outcome::{CommandRecord, DecisionRecord, NewCommand};
use synthesis_session::{
    Reader, SCHEMA_VERSION_OUTCOMES, SCHEMA_VERSION_SUPPORTED, Store, StoreOptions,
};

use crate::proto::{Call, CommandArgs, OutcomeCommand, PROTOCOL};

/// Events per `events.subscribe` page.
const EVENT_PAGE: usize = 200;
/// Longest `note` of a resolve.
const MAX_NOTE: usize = 1024;

/// One outcome call of the engine process: the switch is `synt.orchestrator.enabled` of the user and
/// managed settings (a project file can never turn it on), read per call so a change applies at once.
pub fn serve(env: &synthesis_config::Env, h: &crate::Homes, call: Call) -> Result<Value, String> {
    let st = settings_of(env, h);
    let on = crate::outcome_policy::outcome_policy(&st.synthesis.orchestrator).enabled;
    Service::read_only(&h.synthesis, on).handle(call)
}

/// `synt.orchestrator.enabled` of the user and managed settings (a project file can never turn it on).
pub fn settings_of(env: &synthesis_config::Env, h: &crate::Homes) -> synthesis_settings::Settings {
    let none = h.synthesis.join("no-project");
    synthesis_settings::load_settings(
        &synthesis_settings::LoadContext {
            cwd: &none,
            synthesis_home: &h.synthesis,
            user_home: &h.user,
            env,
        },
        synthesis_config::ProjectTrust::Untrusted,
    )
}

pub struct Service {
    home: PathBuf,
    enabled: bool,
    /// May queue commands (the orchestrator endpoint's authenticated clients only).
    may_queue: bool,
}

fn s<E: ToString>(e: E) -> String {
    e.to_string()
}

fn outcome_id(id: &str) -> Result<OutcomeId, String> {
    OutcomeId::parse(id).map_err(s)
}

impl Service {
    /// The orchestrator endpoint's service: reads, and queues commands. `enabled` is
    /// `synt.orchestrator.enabled` as the settings resolved it (managed layer first).
    pub fn new(home: &Path, enabled: bool) -> Self {
        Self {
            home: home.to_path_buf(),
            enabled,
            may_queue: true,
        }
    }

    /// Reads only: every call that would queue a command is refused (the stdio engine's).
    pub fn read_only(home: &Path, enabled: bool) -> Self {
        Self {
            may_queue: false,
            ..Self::new(home, enabled)
        }
    }

    pub fn handle(&self, call: Call) -> Result<Value, String> {
        if matches!(call, Call::OrchestratorStatus) {
            return Ok(self.status());
        }
        if !self.enabled {
            return Err("outcomes are off (synt.orchestrator.enabled is false)".into());
        }
        if !self.may_queue && call.surface().is_some() {
            return Err(
                "commands are accepted only on the orchestrator endpoint (this link is read-only)"
                    .into(),
            );
        }
        if !self.home.join("synt.db").is_file() {
            return Err("there is no session store yet".into());
        }
        match call {
            Call::OutcomeList { workspace } => {
                let r = Reader::open(&self.home).map_err(s)?;
                let mut rows = vec![];
                for o in r.list_outcomes().map_err(s)? {
                    if workspace.as_ref().is_none_or(|w| *w == o.workspace_id) {
                        rows.push(outcome_json(&r, &o)?);
                    }
                }
                Ok(json!({ "outcomes": rows }))
            }
            Call::OutcomeGet { id } => {
                let r = Reader::open(&self.home).map_err(s)?;
                outcome_json(&r, &r.get_outcome(&outcome_id(&id)?).map_err(s)?)
            }
            Call::OutcomeContractGet { id, version } => {
                let r = Reader::open(&self.home).map_err(s)?;
                let id = outcome_id(&id)?;
                let version = match version {
                    Some(v) => v,
                    None => r.get_outcome(&id).map_err(s)?.contract_version,
                };
                let (contract, hash, approved) = r.get_contract(&id, version).map_err(s)?;
                Ok(
                    json!({"version": version, "hash": hash, "approved": approved,
                          "contract": serde_json::to_value(contract).map_err(s)?}),
                )
            }
            Call::OutcomeContractDiff { id, version } => {
                let r = Reader::open(&self.home).map_err(s)?;
                let id = outcome_id(&id)?;
                let base = r.get_outcome(&id).map_err(s)?.contract_version;
                if version <= base {
                    return Err(format!(
                        "v{version} is not newer than the outcome's contract v{base}"
                    ));
                }
                let (old, old_hash, old_ok) = r.get_contract(&id, base).map_err(s)?;
                let (new, hash, approved) = r.get_contract(&id, version).map_err(s)?;
                let d = synthesis_core::outcome::diff_contracts(&old, &new);
                // `widens` and `weakens` are the server's reading of the two contracts, never a surface's
                Ok(
                    json!({"base": {"version": base, "hash": old_hash, "approved": old_ok},
                          "version": version, "hash": hash, "approved": approved,
                          "diff": serde_json::to_value(&d).map_err(s)?,
                          "widens_scope": !d.widens.is_empty()}),
                )
            }
            Call::PlanGet { id } => {
                let r = Reader::open(&self.home).map_err(s)?;
                let id = outcome_id(&id)?;
                let o = r.get_outcome(&id).map_err(s)?;
                let mut items = vec![];
                for i in r.list_work_items(&id).map_err(s)? {
                    if Some(i.plan_version) == o.plan_version {
                        items.push(item_json(&r, &i)?);
                    }
                }
                Ok(json!({"plan_version": o.plan_version, "items": items}))
            }
            Call::WorkItemGet { outcome, id } => {
                let r = Reader::open(&self.home).map_err(s)?;
                let item = r
                    .get_work_item(&WorkItemId::parse(&id).map_err(s)?)
                    .map_err(s)?;
                owned(&r, &outcome_id(&outcome)?, &item.id)?;
                let mut v = item_json(&r, &item)?;
                v["attempts"] = attempts_json(&r, &item.id)?;
                Ok(v)
            }
            Call::AttemptInspect { outcome, id } => {
                let r = Reader::open(&self.home).map_err(s)?;
                for i in r.list_work_items(&outcome_id(&outcome)?).map_err(s)? {
                    let found = attempts_json(&r, &i.id)?
                        .as_array()
                        .and_then(|a| a.iter().find(|a| a["id"] == id.as_str()).cloned());
                    if let Some(a) = found {
                        return Ok(json!({"work_item_id": i.id, "attempt": a}));
                    }
                }
                Err(format!("no attempt {id} in this outcome"))
            }
            Call::OutcomeBudget { id } => {
                let r = Reader::open(&self.home).map_err(s)?;
                let id = outcome_id(&id)?;
                let o = r.get_outcome(&id).map_err(s)?;
                let (contract, ..) = r.get_contract(&id, o.contract_version).map_err(s)?;
                let u = r.outcome_usage(&id).map_err(s)?;
                // `remaining` is not computed here: with reservations apart it would be a guess
                Ok(
                    json!({"cost_usd": {"spent": u.cost_usd, "limit": contract.envelope.budgets.max_cost_usd},
                          "wall_secs": {"spent": u.wall_ms as f64 / 1000.0},
                          "reserved_usd": u.reserved_usd}),
                )
            }
            Call::AttemptDiff { outcome, id } => {
                let r = Reader::open(&self.home).map_err(s)?;
                let oid = outcome_id(&outcome)?;
                let o = r.get_outcome(&oid).map_err(s)?;
                let mut commit = None;
                for i in r.list_work_items(&oid).map_err(s)? {
                    if let Some(a) = r
                        .list_attempts(&i.id)
                        .map_err(s)?
                        .into_iter()
                        .find(|a| a.id.as_str() == id)
                    {
                        commit = Some(a.subject_commit);
                    }
                }
                let Some(commit) = commit else {
                    return Err(format!("no attempt {id} in this outcome"));
                };
                let Some(commit) = commit else {
                    return Err("the attempt has no candidate commit yet".into());
                };
                // repository, base and commit are the owner's records, never a worker's or a caller's
                let d = synthesis_orchestrator::commit_diff(
                    Path::new(&o.repo_root),
                    synthesis_orchestrator::temp_scratch(),
                    &o.base_commit,
                    &commit,
                    &std::sync::atomic::AtomicBool::new(false),
                )
                .map_err(s)?;
                let files = d
                    .files
                    .iter()
                    .map(|f| {
                        json!({"path": f.path, "status": f.status.to_string(),
                    "binary": f.binary, "secret": f.secret, "added": f.added, "removed": f.removed,
                    "patch": f.patch, "patch_truncated": f.patch_truncated})
                    })
                    .collect::<Vec<_>>();
                Ok(
                    json!({"attempt": id, "base_commit": o.base_commit, "commit": commit,
                          "total_files": d.total_files, "truncated": d.total_files > files.len(),
                          "files": files}),
                )
            }
            Call::DecisionList { outcome } => {
                let r = Reader::open(&self.home).map_err(s)?;
                let list = r
                    .list_decisions(&outcome_id(&outcome)?)
                    .map_err(s)?
                    .iter()
                    .map(|d| decision_json(&r, d))
                    .collect::<Vec<_>>();
                Ok(json!({ "decisions": list }))
            }
            Call::DecisionGet { outcome, id } => {
                let r = Reader::open(&self.home).map_err(s)?;
                let id = DecisionId::parse(&id).map_err(s)?;
                r.list_decisions(&outcome_id(&outcome)?)
                    .map_err(s)?
                    .iter()
                    .find(|d| d.id == id)
                    .map(|d| decision_json(&r, d))
                    .ok_or_else(|| format!("no decision {id} in this outcome"))
            }
            Call::EvidenceGet { outcome } => {
                let r = Reader::open(&self.home).map_err(s)?;
                let rows = r
                    .list_evidence(&outcome_id(&outcome)?)
                    .map_err(s)?
                    .iter()
                    .map(|e| {
                        json!({"id": e.id, "work_item_id": e.work_item_id, "trust_class": e.trust_class,
                           "producer": e.producer, "subject_commit": e.subject_commit, "check_id": e.check_id,
                           "criteria_ids": e.criteria_ids, "result": e.result, "exit_code": e.exit_code,
                           "flags": e.flags, "captured_at": e.captured_at, "event_backed": e.event_backed})
                    })
                    .collect::<Vec<_>>();
                Ok(json!({ "evidence": rows }))
            }
            Call::CommandStatus {
                outcome,
                idempotency_key,
            } => {
                let r = Reader::open(&self.home).map_err(s)?;
                r.get_command(&outcome_id(&outcome)?, &idempotency_key)
                    .map_err(s)?
                    .map(|c| command_json(&c))
                    .ok_or_else(|| "no such command".to_string())
            }
            Call::EventsSubscribe {
                outcome,
                after_event_id,
            } => {
                let r = Reader::open(&self.home).map_err(s)?;
                let o = r.get_outcome(&outcome_id(&outcome)?).map_err(s)?;
                let page = r
                    .read_events(&o.control_session_id, after_event_id, EVENT_PAGE)
                    .map_err(s)?;
                let mut next = after_event_id;
                let mut events = vec![];
                for rec in page {
                    let v = match rec {
                        synthesis_events::Record::Known(e) => serde_json::to_value(e),
                        synthesis_events::Record::Unknown(e) => serde_json::to_value(e),
                    }
                    .map_err(s)?;
                    next = v["seq"].as_u64().unwrap_or(next);
                    events.push(v);
                }
                Ok(json!({"events": events, "next_event_id": next}))
            }
            Call::OutcomeContractApprove {
                id,
                version,
                hash,
                cmd,
            } => self.queue(
                &id,
                "outcome.contract.approve",
                json!({"version": version, "hash": hash}),
                None,
                &cmd,
            ),
            Call::OutcomeContractRevise {
                id,
                expected_revision,
                contract,
                cmd,
            } => self.queue(
                &id,
                "outcome.contract.revise",
                json!({"contract": serde_json::to_value(contract).map_err(s)?}),
                Some(expected_revision),
                &cmd,
            ),
            Call::OutcomeStart(c) => self.queue_outcome("outcome.start", &c),
            Call::OutcomePause(c) => self.queue_outcome("outcome.pause", &c),
            Call::OutcomeResume(c) => self.queue_outcome("outcome.resume", &c),
            Call::OutcomeCancel(c) => self.queue_outcome("outcome.cancel", &c),
            Call::OutcomeAck(c) => self.queue_outcome("outcome.ack", &c),
            Call::DecisionResolve {
                outcome,
                id,
                expected_revision,
                choice,
                brief_sha256,
                note,
                cmd,
            } => {
                if note.as_ref().is_some_and(|n| n.len() > MAX_NOTE) {
                    return Err(format!("the note is longer than {MAX_NOTE} bytes"));
                }
                let mut params =
                    json!({"decision_id": id, "choice": choice, "brief_sha256": brief_sha256});
                if let Some(n) = note {
                    params["note"] = json!(n);
                }
                self.queue(
                    &outcome,
                    "decision.resolve",
                    params,
                    Some(expected_revision),
                    &cmd,
                )
            }
            Call::OutcomeCreate { .. } | Call::OrchestratorEnable { .. } => {
                Err("this call is served by the orchestrator process only".into())
            }
            _ => Err("not an outcome method".into()),
        }
    }

    fn status(&self) -> Value {
        let schema = rusqlite::Connection::open_with_flags(
            self.home.join("synt.db"),
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .and_then(|c| c.query_row("PRAGMA user_version", [], |r| r.get::<_, i64>(0)))
        .ok();
        json!({"enabled": self.enabled, "protocol": PROTOCOL, "schema": schema,
               "ready": self.enabled && schema.is_some_and(|v| (SCHEMA_VERSION_OUTCOMES..=SCHEMA_VERSION_SUPPORTED).contains(&v))})
    }

    fn queue_outcome(&self, kind: &str, c: &OutcomeCommand) -> Result<Value, String> {
        self.queue(&c.id, kind, json!({}), c.expected_revision, &c.cmd)
    }

    /// The only write a surface makes: a command row (a retry of the same key returns the stored row).
    fn queue(
        &self,
        outcome: &str,
        kind: &str,
        params: Value,
        expected_revision: Option<i64>,
        cmd: &CommandArgs,
    ) -> Result<Value, String> {
        let store = Store::open(&self.home, StoreOptions::default()).map_err(s)?;
        let rec = store
            .enqueue_command(NewCommand {
                outcome_id: outcome_id(outcome)?,
                idempotency_key: cmd.idempotency_key.clone(),
                kind: kind.into(),
                params,
                expected_revision,
                from_surface: cmd.surface,
            })
            .map_err(s)?;
        Ok(command_json(&rec))
    }
}

/// The work item belongs to the outcome (an id of another outcome is refused, not looked up).
fn owned(r: &Reader, outcome: &OutcomeId, item: &WorkItemId) -> Result<(), String> {
    r.list_work_items(outcome)
        .map_err(s)?
        .iter()
        .any(|i| i.id == *item)
        .then_some(())
        .ok_or_else(|| format!("no work item {item} in this outcome"))
}

fn command_json(c: &CommandRecord) -> Value {
    json!({"command_id": c.id, "idempotency_key": c.idempotency_key, "kind": c.kind,
           "state": c.state, "result": c.result})
}

fn outcome_json(
    r: &Reader,
    o: &synthesis_session::outcome::OutcomeRecord,
) -> Result<Value, String> {
    let pending = r
        .list_decisions(&o.id)
        .map_err(s)?
        .iter()
        .filter(|d| r.decision_state(&d.id).ok() == Some(DecisionState::Pending))
        .count();
    // Derived from rows, so it survives restarts (ADR-021 section 4). There is no `critical` (dropped, ADR-021 B2 notes).
    Ok(
        json!({"id": o.id, "workspace_id": o.workspace_id, "goal": o.goal, "state": o.state,
        "revision": o.revision, "contract_version": o.contract_version, "plan_version": o.plan_version,
        "control_session_id": o.control_session_id, "created_at": o.created_at, "updated_at": o.updated_at,
        "needs_you": {"decisions_pending": pending, "blocked": o.state == OutcomeState::Blocked,
                      "verified_awaiting_ack": o.state == OutcomeState::Verified}}),
    )
}

fn item_json(r: &Reader, i: &synthesis_session::outcome::WorkItemRecord) -> Result<Value, String> {
    Ok(
        json!({"id": i.id, "key": i.key, "kind": i.kind, "state": i.state, "revision": i.revision,
        "retries_used": i.retries_used, "requires": r.work_item_deps(&i.id).map_err(s)?}),
    )
}

fn attempts_json(r: &Reader, item: &WorkItemId) -> Result<Value, String> {
    let rows = r
        .list_attempts(item)
        .map_err(s)?
        .iter()
        .map(|a| {
            let mut v = json!({"id": a.id, "ordinal": a.ordinal, "session_id": a.session_id, "state": a.state,
               "task_status": a.task_status, "subject_commit": a.subject_commit,
               "retained_reason": a.retained_reason, "started_at": a.started_at, "ended_at": a.ended_at});
            // what the owner recorded at launch; an older attempt has none, so the key is omitted
            if let (Some(b), Some(g)) = (&a.sandbox_backend, &a.sandbox_guarantee) {
                v["sandbox"] = json!({"backend": b, "guarantee": g});
            }
            v
        })
        .collect::<Vec<_>>();
    Ok(Value::Array(rows))
}

/// The state is the verified one (a resolved row without a valid approval is `unverified`, which
/// no surface can resolve); the brief is stored data, shown whole.
fn decision_json(r: &Reader, d: &DecisionRecord) -> Value {
    let state = r
        .decision_state(&d.id)
        .map_or_else(|_| json!("unverified"), |st| json!(st));
    json!({"id": d.id, "kind": d.kind, "state": state, "revision": d.revision, "brief": d.brief,
           "brief_sha256": d.brief_sha256, "binding_hash": d.binding_hash, "expires_at": d.expires_at,
           "created_at": d.created_at})
}
