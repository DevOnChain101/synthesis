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
use synthesis_session::outcome::{CommandRecord, DecisionRecord, NewCommand, check_message_text};
use synthesis_session::{
    Reader, SCHEMA_VERSION_OUTCOMES, SCHEMA_VERSION_SUPPORTED, Store, StoreOptions,
};

use synthesis_orchestrator::gateway::{audit, authorize};
use synthesis_orchestrator::{Action, Principal};

use crate::proto::{Call, CommandArgs, FEATURES, OutcomeCommand, PROTOCOL};

/// The gateway's verdict on a call at the link. A denial is audited here as one stderr line of
/// codes (no outcome row exists to chain it to; an allowed command is audited in the outcome's
/// control session when the owner applies it).
pub fn gateway_allows(p: Principal, a: Action) -> bool {
    let ok = authorize(p, Some(a)).allow;
    if !ok {
        eprintln!("synt orchestrator: policy.decision {}", audit(p, Some(a)));
    }
    ok
}

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
        // the gateway is the one policy point: the stdio engine is the read-only principal and is
        // denied every mutation; a surface principal is the one the endpoint link attached
        if let Some(a) = call.action() {
            let p = match call.surface() {
                Some(s) if self.may_queue => Principal::User(s),
                _ => Principal::Engine,
            };
            if !gateway_allows(p, a) {
                return Err(if self.may_queue {
                    "the owner's policy table does not allow this call".into()
                } else {
                    "commands are accepted only on the orchestrator endpoint (this link is read-only)"
                        .into()
                });
            }
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
                          "reserved_usd": u.reserved_usd, "planner_spent_usd": u.planner_usd}),
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
                // a read-only goal has a report, not a commit: git is never run for it
                if o.base_commit.is_empty() {
                    return Err("a read-only goal changes nothing: it has no diff".into());
                }
                // no home folder means no settings to read: refuse rather than diff with the built-in names alone
                let env = synthesis_config::Env::from_process();
                let h = crate::homes(&env).map_err(|e| {
                    format!("the diff is not shown: the secret-path settings cannot be read ({e})")
                })?;
                let (secrets, diag) =
                    crate::ctx::secret_policy_for(&env, &h, Path::new(&o.repo_root));
                if let Some(d) = diag.first() {
                    eprintln!("synt: warning: {d}");
                }
                // repository, base and commit are the owner's records, never a worker's or a caller's
                let d = synthesis_orchestrator::commit_diff(
                    Path::new(&o.repo_root),
                    synthesis_orchestrator::temp_scratch(),
                    &o.base_commit,
                    &commit,
                    &std::sync::atomic::AtomicBool::new(false),
                    &secrets,
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
            Call::AttemptMessage {
                outcome,
                work_item,
                text,
                brief_change,
                expected_revision,
                proposal_sha256,
                cmd,
            } => {
                // the same checks as the owner's, so a refusal comes back at once (the owner repeats them)
                if cfg!(windows) {
                    return Err(
                        "worker messages are not available on native Windows; open the folder in WSL2"
                            .into(),
                    );
                }
                check_message_text(&text).map_err(s)?;
                WorkItemId::parse(&work_item).map_err(s)?;
                let mut params =
                    json!({"work_item": work_item, "text": text, "brief_change": brief_change});
                if let Some(h) = proposal_sha256 {
                    params["proposal_sha256"] = json!(h);
                }
                self.queue(&outcome, "attempt.message", params, expected_revision, &cmd)
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
        // worker messages are refused on native Windows until a real Windows run (S1 Q5)
        let features: Vec<_> = FEATURES
            .iter()
            .filter(|f| !(cfg!(windows) && **f == "attempt_message"))
            .collect();
        json!({"enabled": self.enabled, "protocol": PROTOCOL, "features": features, "schema": schema,
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
        .filter(|d| d.row_state == DecisionState::Pending)
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
            // the `seq`s of the person's messages this worker acked (S1 4.6): "sent (live)" becomes "delivered"
            let delivered = r.owner_deliveries(&a.session_id).unwrap_or_default();
            if !delivered.is_empty() {
                v["messages_delivered"] = json!(delivered);
            }
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
    // An open row's state is the one read with its revision: a second read can see the next
    // revision's state (preparing -> pending) and show a card whose revision cannot be answered.
    let state = match d.row_state {
        DecisionState::Approved | DecisionState::Rejected | DecisionState::NeedsMoreInfo => r
            .decision_state(&d.id)
            .map_or_else(|_| json!("unverified"), |st| json!(st)),
        open => json!(open),
    };
    json!({"id": d.id, "kind": d.kind, "state": state, "revision": d.revision, "brief": d.brief,
           "brief_sha256": d.brief_sha256, "binding_hash": d.binding_hash, "expires_at": d.expires_at,
           "created_at": d.created_at})
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use synthesis_core::WorkspaceContext;
    use synthesis_core::outcome::*;
    use synthesis_session::outcome::{Effects, NewDecision, NewOutcome, OutcomePolicy};
    use synthesis_session::{SCHEMA_VERSION_OUTCOMES, Store, StoreOptions};

    /// A card is listed with the row and its state from the same read: a card read while the
    /// decision was still `preparing` (revision 1) must not say `pending` because the owner
    /// committed the next revision a moment later, or its revision could never be answered.
    #[test]
    fn a_card_listed_before_the_commit_to_pending_is_not_shown_pending_at_revision_1() {
        let tmp = tempfile::tempdir().unwrap();
        let base = synthesis_core::canonicalize(tmp.path()).unwrap();
        let repo = base.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        let home = base.join("home");
        let store = Store::open(
            &home,
            StoreOptions {
                schema_target: SCHEMA_VERSION_OUTCOMES,
                ..StoreOptions::default()
            },
        )
        .unwrap();
        let ws = WorkspaceContext::open("w", &repo, &repo, &[], base.join("cart"), "pg").unwrap();
        let mut caps = BTreeMap::new();
        caps.insert(Capability::RepoRead, CapabilityMode::Allow);
        let contract = Contract {
            kind: synthesis_core::outcome::ContractKind::Write,
            version: 1,
            goal: "goal".into(),
            criteria: vec![ContractCriterion {
                id: "c1".into(),
                text: "c1".into(),
                mandatory: true,
                manual: false,
                required_trust: TrustClass::IndependentCheck,
                check_ids: vec![],
            }],
            envelope: Envelope {
                allowed_paths: vec!["src/**".into()],
                denied_paths: vec![],
                capabilities: caps,
                budgets: Budgets::new(5.0),
                quality_gates: vec![],
            },
            checks: vec![],
        };
        let mut w = store
            .create_outcome(NewOutcome {
                workspace: &ws,
                base_commit: "c0ffee".into(),
                contract: &contract,
                policy: &OutcomePolicy::new(true, 16, 10, 24),
            })
            .unwrap();
        let (d, _) = w
            .new_decision(&NewDecision {
                kind: "ambiguity".into(),
                dedupe_key: "k".into(),
                brief: json!({"why": "w", "alternatives": [], "unknowns": [], "safe_default": "HOLD"}),
                binding: json!({}),
                expires_at: (time::OffsetDateTime::now_utc() + time::Duration::hours(1))
                    .format(time::macros::format_description!(
                        "[year]-[month]-[day]T[hour]:[minute]:[second].[subsecond digits:3]Z"
                    ))
                    .unwrap(),
                action: None,
            })
            .unwrap();
        let reader = Reader::open(&home).unwrap();
        // the listing a surface got just before the owner opened the decision
        let listed = reader.list_decisions(w.id()).unwrap()[0].clone();
        w.transition_decision(
            &d.id,
            1,
            DecisionState::Pending,
            &d.binding,
            &Effects::default(),
        )
        .unwrap();
        let stale = decision_json(&reader, &listed);
        assert_eq!(
            (stale["state"].as_str(), stale["revision"].as_i64()),
            (Some("preparing"), Some(1))
        );
        let fresh = decision_json(&reader, &reader.list_decisions(w.id()).unwrap()[0]);
        assert_eq!(
            (fresh["state"].as_str(), fresh["revision"].as_i64()),
            (Some("pending"), Some(2))
        );
    }
}
