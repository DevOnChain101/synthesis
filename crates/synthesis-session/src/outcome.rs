//! Outcome persistence (ADR-016): aggregates with optimistic revisions, durable commands, the
//! outbox, decisions and evidence rows, all in `synt.db` schema v5.
//!
//! Rules this module keeps:
//! - Only the holder of the outcome's control-session lock writes aggregate rows
//!   ([`OutcomeWriter`]). A surface only inserts a command ([`Store::enqueue_command`]).
//! - Every change is one IMMEDIATE transaction: state row, event in the control session (hash
//!   chained), outbox rows. A crash leaves all of it or none of it.
//! - A state change is a compare-and-swap on `revision` and must be in the transition tables of
//!   `synthesis_core::outcome`; the guards read what they can from the rows, not from the caller.
//! - Nothing here exists below v5: every entry point answers [`Error::OutcomesNotEnabled`].

use std::ops::Deref;

use rusqlite::{Connection, OptionalExtension, Row, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use synthesis_core::outcome::{
    AttemptId, AttemptState, CommandId, Contract, DecisionGuards, DecisionId, DecisionState,
    EvidenceId, OutcomeGuards, OutcomeId, OutcomeState, Plan, TrustClass, WorkItemGuards,
    WorkItemId, WorkItemKind, WorkItemState, validate_contract, validate_plan,
};
use synthesis_core::{PermissionMode, TaskStatus, WorkspaceContext};
use synthesis_events::{Budget, outcome as ev};
use time::OffsetDateTime;

use crate::clean::ts;
use crate::error::{Error, Result};
use crate::schema;
use crate::store::{
    EventTx, NewSession, Reader, SessionWriter, Store, bad, connect, enum_from, enum_str, now,
    sha_hex, utf8,
};

/// Backoff cap of the outbox: after this many tries a row stays `failed` for a person to look at.
pub const OUTBOX_MAX_TRIES: u32 = 5;

fn need_v5(conn: &Connection) -> Result<()> {
    if schema::user_version(conn)? >= schema::OUTCOMES {
        Ok(())
    } else {
        Err(Error::OutcomesNotEnabled)
    }
}

fn new_uuid() -> String {
    uuid::Uuid::new_v4().to_string()
}

fn conflict(aggregate: &'static str, id: &str, expected: i64, actual: i64) -> Error {
    Error::RevisionConflict {
        aggregate,
        id: id.to_string(),
        expected,
        actual,
    }
}

fn refuse(e: impl std::fmt::Display) -> Error {
    Error::Invalid(e.to_string())
}

// ------------------------------------------------------------------- records

#[derive(Debug, Clone, PartialEq)]
pub struct OutcomeRecord {
    pub id: OutcomeId,
    pub workspace_id: String,
    pub repo_root: String,
    pub base_commit: String,
    pub control_session_id: String,
    pub goal: String,
    pub state: OutcomeState,
    pub revision: i64,
    pub contract_version: u32,
    pub plan_version: Option<u32>,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct WorkItemRecord {
    pub id: WorkItemId,
    pub plan_version: u32,
    pub key: String,
    pub kind: WorkItemKind,
    pub spec: Value,
    pub state: WorkItemState,
    pub revision: i64,
    pub retries_used: u32,
}

#[derive(Debug, Clone, PartialEq)]
pub struct AttemptRecord {
    pub id: AttemptId,
    pub work_item_id: WorkItemId,
    pub ordinal: u32,
    pub session_id: String,
    pub worker_kind: String,
    pub state: AttemptState,
    pub revision: i64,
    pub fence: i64,
    pub task_status: Option<String>,
    pub subject_commit: Option<String>,
    pub retained_reason: Option<String>,
    pub started_at: String,
    pub ended_at: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct DecisionRecord {
    pub id: DecisionId,
    pub kind: String,
    pub dedupe_key: String,
    pub state: DecisionState,
    pub revision: i64,
    pub brief: Value,
    pub binding: Value,
    pub expires_at: String,
    pub created_at: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct EvidenceRecord {
    pub id: EvidenceId,
    pub work_item_id: WorkItemId,
    pub attempt_id: Option<AttemptId>,
    pub trust_class: TrustClass,
    pub producer: String,
    pub subject_commit: String,
    pub check_id: String,
    pub criteria_ids: Vec<String>,
    pub result: String,
    pub exit_code: Option<i64>,
    pub sha256: Option<String>,
    pub fence: i64,
    pub captured_at: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Surface {
    Cli,
    Tui,
    Superagent,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CommandState {
    Queued,
    Applied,
    Stale,
    AlreadyResolved,
    Denied,
}

#[derive(Debug, Clone, PartialEq)]
pub struct CommandRecord {
    pub id: CommandId,
    pub outcome_id: OutcomeId,
    pub idempotency_key: String,
    pub kind: String,
    pub params: Value,
    pub expected_revision: Option<i64>,
    pub from_surface: Surface,
    pub state: CommandState,
    pub result: Option<Value>,
    pub created_at: String,
    pub applied_at: Option<String>,
}

/// Effects of the MVP outbox (ADR-016 item 5): local only, idempotent on their own key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum OutboxKind {
    #[serde(rename = "worktree.create")]
    WorktreeCreate,
    #[serde(rename = "worktree.cleanup")]
    WorktreeCleanup,
    #[serde(rename = "verifier.run")]
    VerifierRun,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OutboxState {
    Pending,
    Dispatching,
    Done,
    Failed,
}

#[derive(Debug, Clone, PartialEq)]
pub struct OutboxRecord {
    pub id: String,
    pub idempotency_key: String,
    pub kind: OutboxKind,
    pub payload: Value,
    pub state: OutboxState,
    pub tries: u32,
    pub next_at: Option<String>,
    pub receipt: Option<String>,
}

// ---------------------------------------------------------------- row mapping

fn col<T: serde::de::DeserializeOwned>(r: &Row<'_>, i: usize) -> rusqlite::Result<T> {
    enum_from(&r.get::<_, String>(i)?).map_err(bad)
}
fn json_col(r: &Row<'_>, i: usize) -> rusqlite::Result<Value> {
    serde_json::from_str(&r.get::<_, String>(i)?).map_err(bad)
}
fn id_col<T: TryFrom<String, Error = synthesis_core::outcome::InvalidId>>(
    r: &Row<'_>,
    i: usize,
) -> rusqlite::Result<T> {
    T::try_from(r.get::<_, String>(i)?).map_err(bad)
}

const OUTCOME_COLS: &str = "id,workspace_id,repo_root,base_commit,control_session_id,goal,state,revision,contract_version,plan_version,created_at,updated_at";
fn outcome_row(r: &Row<'_>) -> rusqlite::Result<OutcomeRecord> {
    Ok(OutcomeRecord {
        id: id_col(r, 0)?,
        workspace_id: r.get(1)?,
        repo_root: r.get(2)?,
        base_commit: r.get(3)?,
        control_session_id: r.get(4)?,
        goal: r.get(5)?,
        state: col(r, 6)?,
        revision: r.get(7)?,
        contract_version: r.get(8)?,
        plan_version: r.get(9)?,
        created_at: r.get(10)?,
        updated_at: r.get(11)?,
    })
}

const ITEM_COLS: &str = "id,plan_version,key,kind,spec,state,revision,retries_used";
fn item_row(r: &Row<'_>) -> rusqlite::Result<WorkItemRecord> {
    Ok(WorkItemRecord {
        id: id_col(r, 0)?,
        plan_version: r.get(1)?,
        key: r.get(2)?,
        kind: col(r, 3)?,
        spec: json_col(r, 4)?,
        state: col(r, 5)?,
        revision: r.get(6)?,
        retries_used: r.get(7)?,
    })
}

const ATTEMPT_COLS: &str = "a.id,a.work_item_id,a.ordinal,a.session_id,a.worker_kind,a.state,a.revision,a.fence,a.task_status,a.subject_commit,a.retained_reason,a.started_at,a.ended_at";
fn attempt_row(r: &Row<'_>) -> rusqlite::Result<AttemptRecord> {
    Ok(AttemptRecord {
        id: id_col(r, 0)?,
        work_item_id: id_col(r, 1)?,
        ordinal: r.get(2)?,
        session_id: r.get(3)?,
        worker_kind: r.get(4)?,
        state: col(r, 5)?,
        revision: r.get(6)?,
        fence: r.get(7)?,
        task_status: r.get(8)?,
        subject_commit: r.get(9)?,
        retained_reason: r.get(10)?,
        started_at: r.get(11)?,
        ended_at: r.get(12)?,
    })
}

const DECISION_COLS: &str = "id,kind,dedupe_key,state,revision,brief,binding,expires_at,created_at";
fn decision_row(r: &Row<'_>) -> rusqlite::Result<DecisionRecord> {
    Ok(DecisionRecord {
        id: id_col(r, 0)?,
        kind: r.get(1)?,
        dedupe_key: r.get(2)?,
        state: col(r, 3)?,
        revision: r.get(4)?,
        brief: json_col(r, 5)?,
        binding: json_col(r, 6)?,
        expires_at: r.get(7)?,
        created_at: r.get(8)?,
    })
}

const EVIDENCE_COLS: &str = "id,work_item_id,attempt_id,trust_class,producer,subject_commit,check_id,criteria_ids,result,exit_code,sha256,fence,captured_at";
fn evidence_row(r: &Row<'_>) -> rusqlite::Result<EvidenceRecord> {
    let attempt: Option<String> = r.get(2)?;
    Ok(EvidenceRecord {
        id: id_col(r, 0)?,
        work_item_id: id_col(r, 1)?,
        attempt_id: attempt.map(AttemptId::try_from).transpose().map_err(bad)?,
        trust_class: col(r, 3)?,
        producer: r.get(4)?,
        subject_commit: r.get(5)?,
        check_id: r.get(6)?,
        criteria_ids: serde_json::from_value(json_col(r, 7)?).map_err(bad)?,
        result: r.get(8)?,
        exit_code: r.get(9)?,
        sha256: r.get(10)?,
        fence: r.get(11)?,
        captured_at: r.get(12)?,
    })
}

const COMMAND_COLS: &str = "id,outcome_id,idempotency_key,kind,params,expected_revision,from_surface,state,result,created_at,applied_at";
fn command_row(r: &Row<'_>) -> rusqlite::Result<CommandRecord> {
    let result: Option<String> = r.get(8)?;
    Ok(CommandRecord {
        id: id_col(r, 0)?,
        outcome_id: id_col(r, 1)?,
        idempotency_key: r.get(2)?,
        kind: r.get(3)?,
        params: json_col(r, 4)?,
        expected_revision: r.get(5)?,
        from_surface: col(r, 6)?,
        state: col(r, 7)?,
        result: result
            .map(|s| serde_json::from_str(&s))
            .transpose()
            .map_err(bad)?,
        created_at: r.get(9)?,
        applied_at: r.get(10)?,
    })
}

const OUTBOX_COLS: &str = "id,idempotency_key,kind,payload,state,tries,next_at,receipt";
fn outbox_row(r: &Row<'_>) -> rusqlite::Result<OutboxRecord> {
    Ok(OutboxRecord {
        id: r.get(0)?,
        idempotency_key: r.get(1)?,
        kind: col(r, 2)?,
        payload: json_col(r, 3)?,
        state: col(r, 4)?,
        tries: r.get(5)?,
        next_at: r.get(6)?,
        receipt: r.get(7)?,
    })
}

// -------------------------------------------------------------------- reading

impl Reader {
    pub fn get_outcome(&self, id: &OutcomeId) -> Result<OutcomeRecord> {
        need_v5(&self.conn)?;
        self.conn
            .query_row(
                &format!("SELECT {OUTCOME_COLS} FROM outcomes WHERE id=?1"),
                [id.as_str()],
                outcome_row,
            )
            .optional()?
            .ok_or_else(|| Error::NotFound(format!("outcome {id}")))
    }

    /// Newest first.
    pub fn list_outcomes(&self) -> Result<Vec<OutcomeRecord>> {
        need_v5(&self.conn)?;
        let mut st = self.conn.prepare(&format!(
            "SELECT {OUTCOME_COLS} FROM outcomes ORDER BY created_at DESC, id"
        ))?;
        Ok(st
            .query_map([], outcome_row)?
            .collect::<rusqlite::Result<_>>()?)
    }

    /// The stored contract of a version and whether a person approved it.
    pub fn get_contract(&self, id: &OutcomeId, version: u32) -> Result<(Contract, String, bool)> {
        need_v5(&self.conn)?;
        let (body, hash, approved): (String, String, Option<String>) = self
            .conn
            .query_row(
                "SELECT body,hash,approved_at FROM contracts WHERE outcome_id=?1 AND version=?2",
                params![id.as_str(), version],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?
            .ok_or_else(|| Error::NotFound(format!("contract {id} v{version}")))?;
        Ok((serde_json::from_str(&body)?, hash, approved.is_some()))
    }

    pub fn list_work_items(&self, id: &OutcomeId) -> Result<Vec<WorkItemRecord>> {
        need_v5(&self.conn)?;
        let mut st = self.conn.prepare(&format!(
            "SELECT {ITEM_COLS} FROM work_items WHERE outcome_id=?1 ORDER BY plan_version, key"
        ))?;
        Ok(st
            .query_map([id.as_str()], item_row)?
            .collect::<rusqlite::Result<_>>()?)
    }

    /// Keys of the items a work item requires.
    pub fn work_item_deps(&self, id: &WorkItemId) -> Result<Vec<String>> {
        need_v5(&self.conn)?;
        let mut st = self.conn.prepare(
            "SELECT r.key FROM work_item_deps d JOIN work_items r ON r.id=d.requires_id \
             WHERE d.work_item_id=?1 ORDER BY r.key",
        )?;
        Ok(st
            .query_map([id.as_str()], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?)
    }

    pub fn list_attempts(&self, item: &WorkItemId) -> Result<Vec<AttemptRecord>> {
        need_v5(&self.conn)?;
        let mut st = self.conn.prepare(&format!(
            "SELECT {ATTEMPT_COLS} FROM attempts a WHERE a.work_item_id=?1 ORDER BY a.ordinal"
        ))?;
        Ok(st
            .query_map([item.as_str()], attempt_row)?
            .collect::<rusqlite::Result<_>>()?)
    }

    pub fn list_decisions(&self, id: &OutcomeId) -> Result<Vec<DecisionRecord>> {
        need_v5(&self.conn)?;
        let mut st = self.conn.prepare(&format!(
            "SELECT {DECISION_COLS} FROM decisions WHERE outcome_id=?1 ORDER BY created_at, id"
        ))?;
        Ok(st
            .query_map([id.as_str()], decision_row)?
            .collect::<rusqlite::Result<_>>()?)
    }

    /// Approvals of a decision: `(decision_revision, approver, choice)`.
    pub fn decision_approvals(&self, id: &DecisionId) -> Result<Vec<(i64, String, String)>> {
        need_v5(&self.conn)?;
        let mut st = self.conn.prepare(
            "SELECT decision_revision,approver,choice FROM decision_approvals \
             WHERE decision_id=?1 ORDER BY rowid",
        )?;
        Ok(st
            .query_map([id.as_str()], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
            .collect::<rusqlite::Result<_>>()?)
    }

    pub fn list_evidence(&self, id: &OutcomeId) -> Result<Vec<EvidenceRecord>> {
        need_v5(&self.conn)?;
        let mut st = self.conn.prepare(&format!(
            "SELECT {EVIDENCE_COLS} FROM evidence WHERE outcome_id=?1 ORDER BY captured_at, id"
        ))?;
        Ok(st
            .query_map([id.as_str()], evidence_row)?
            .collect::<rusqlite::Result<_>>()?)
    }

    pub fn list_outbox(&self, id: &OutcomeId) -> Result<Vec<OutboxRecord>> {
        need_v5(&self.conn)?;
        let mut st = self.conn.prepare(&format!(
            "SELECT {OUTBOX_COLS} FROM outbox WHERE outcome_id=?1 ORDER BY rowid"
        ))?;
        Ok(st
            .query_map([id.as_str()], outbox_row)?
            .collect::<rusqlite::Result<_>>()?)
    }

    pub fn get_command(&self, idempotency_key: &str) -> Result<Option<CommandRecord>> {
        need_v5(&self.conn)?;
        Ok(self
            .conn
            .query_row(
                &format!("SELECT {COMMAND_COLS} FROM outcome_commands WHERE idempotency_key=?1"),
                [idempotency_key],
                command_row,
            )
            .optional()?)
    }

    /// Commands waiting for an owner, oldest first.
    pub fn queued_commands(&self, id: &OutcomeId) -> Result<Vec<CommandRecord>> {
        need_v5(&self.conn)?;
        let mut st = self.conn.prepare(&format!(
            "SELECT {COMMAND_COLS} FROM outcome_commands WHERE outcome_id=?1 AND state='queued' \
             ORDER BY created_at, rowid"
        ))?;
        Ok(st
            .query_map([id.as_str()], command_row)?
            .collect::<rusqlite::Result<_>>()?)
    }
}

// ------------------------------------------------------------------- commands

#[derive(Debug, Clone)]
pub struct NewCommand {
    pub outcome_id: OutcomeId,
    /// The caller's retry key: the same key returns the stored result and has no second effect.
    pub idempotency_key: String,
    /// `decision.resolve`, `outcome.start`, ...
    pub kind: String,
    pub params: Value,
    pub expected_revision: Option<i64>,
    pub from_surface: Surface,
}

fn valid_command_text(s: &str, max: usize) -> bool {
    !s.is_empty() && s.len() <= max && s.bytes().all(|b| b.is_ascii_graphic())
}

impl Store {
    /// The only write a surface makes (ADR-016 item 11): a plain insert, safe under WAL while an
    /// owner runs or while none does. Retrying with the same key returns the stored row; reusing a
    /// key for a different request is refused.
    pub fn enqueue_command(&self, c: NewCommand) -> Result<CommandRecord> {
        if !valid_command_text(&c.idempotency_key, 128) || !valid_command_text(&c.kind, 64) {
            return Err(Error::Invalid(
                "command key and kind must be 1 to 128 and 1 to 64 printable characters".into(),
            ));
        }
        let conn = connect(self.home(), false)?;
        need_v5(&conn)?;
        let mut params_json = c.params.clone();
        // the store never keeps a secret a surface pasted into a command
        self.redact_value(&mut params_json);
        conn.execute(
            "INSERT INTO outcome_commands(id,outcome_id,idempotency_key,kind,params,expected_revision,from_surface,state,created_at) \
             VALUES (?1,?2,?3,?4,?5,?6,?7,'queued',?8) ON CONFLICT(idempotency_key) DO NOTHING",
            params![
                new_uuid(),
                c.outcome_id.as_str(),
                c.idempotency_key,
                c.kind,
                params_json.to_string(),
                c.expected_revision,
                enum_str(&c.from_surface),
                now(),
            ],
        )?;
        let rec = self
            .reader()?
            .get_command(&c.idempotency_key)?
            .ok_or_else(|| Error::NotFound("command".into()))?;
        if rec.outcome_id != c.outcome_id
            || rec.kind != c.kind
            || rec.params != params_json
            || rec.expected_revision != c.expected_revision
            || rec.from_surface != c.from_surface
        {
            return Err(Error::Invalid(
                "idempotency key was already used by a different request".into(),
            ));
        }
        Ok(rec)
    }
}

// ------------------------------------------------------------------- creating

pub struct NewOutcome<'a> {
    /// The workspace the control session is bound to (the repository's root).
    pub workspace: &'a WorkspaceContext,
    pub base_commit: String,
    /// Validated here; the stored body is what a person approves.
    pub contract: &'a Contract,
}

/// `sha256` of the stored contract body (redacted JSON, fields in declaration order). Approval
/// records this hash, and a stored contract can never change (trigger in the v5 migration).
fn body_hash(body: &Value) -> String {
    sha_hex(body.to_string().as_bytes())
}

impl Store {
    /// Creates the outcome, its hidden control session (`kind = 'outcome'`) and contract v1 in one
    /// transaction with the `outcome.created` event, and returns the writer that owns the
    /// control-session lock.
    pub fn create_outcome(&self, new: NewOutcome<'_>) -> Result<OutcomeWriter> {
        need_v5(&connect(self.home(), true)?)?;
        validate_contract(new.contract).map_err(refuse)?;
        let id = OutcomeId::parse(&new_uuid()).map_err(refuse)?;
        // no Node transcript mirror: a control session holds events only
        let mut w = self.create_inner(
            NewSession {
                id: None,
                workspace: new.workspace,
                model_config_id: "orchestrator".into(),
                permission_mode: PermissionMode::Default,
                permission_policy_id: "orchestrator".into(),
                budget: Budget::default(),
                parent: None,
                title: Some("outcome".into()),
            },
            |_, _| None,
        )?;
        let control = w.id().to_string();
        let (root, ws_id) = (
            utf8(&new.workspace.root)?,
            new.workspace.workspace_id.clone(),
        );
        let contract_json = serde_json::to_value(new.contract)?;
        w.event_tx(|et| {
            let mut body = contract_json.clone();
            et.redact_value(&mut body);
            let goal = et.redact_str(&new.contract.goal);
            let ts = now();
            et.tx
                .execute("UPDATE sessions SET kind='outcome' WHERE id=?1", [&control])?;
            et.tx.execute(
                "INSERT INTO outcomes(id,workspace_id,repo_root,base_commit,control_session_id,goal,state,revision,contract_version,created_at,updated_at) \
                 VALUES (?1,?2,?3,?4,?5,?6,'draft',1,?7,?8,?8)",
                params![
                    id.as_str(),
                    ws_id,
                    root,
                    new.base_commit,
                    control,
                    goal,
                    new.contract.version,
                    ts
                ],
            )?;
            et.tx.execute(
                "INSERT INTO contracts(outcome_id,version,hash,body) VALUES (?1,?2,?3,?4)",
                params![id.as_str(), new.contract.version, body_hash(&body), body.to_string()],
            )?;
            et.append(
                ev::OUTCOME_CREATED,
                json!({"outcome_id": id, "contract_version": new.contract.version,
                       "contract_hash": body_hash(&body), "revision": 1}),
            )?;
            Ok(())
        })?;
        Ok(OutcomeWriter { w, id })
    }

    /// Takes the control-session lock of an existing outcome. [`Error::Locked`] means another
    /// process owns it: talk to that owner (or queue a command) instead.
    pub fn open_outcome(&self, id: &OutcomeId) -> Result<OutcomeWriter> {
        let rec = self.reader()?.get_outcome(id)?;
        let w = self.open_locked(&rec.control_session_id)?;
        Ok(OutcomeWriter { w, id: id.clone() })
    }
}

// --------------------------------------------------------------------- writer

/// The single writer of one outcome: holds its control session's OS lock for its whole life.
pub struct OutcomeWriter {
    w: SessionWriter,
    id: OutcomeId,
}

impl Deref for OutcomeWriter {
    type Target = Reader;
    fn deref(&self) -> &Reader {
        &self.w
    }
}

/// What a transition adds to its transaction.
#[derive(Debug, Clone, Default)]
pub struct Effects {
    /// Extra events `(kind, payload)` of the control session.
    pub events: Vec<(String, Value)>,
    pub outbox: Vec<NewOutbox>,
}

#[derive(Debug, Clone)]
pub struct NewOutbox {
    pub idempotency_key: String,
    pub kind: OutboxKind,
    pub payload: Value,
}

/// Facts about an outcome the rows cannot show (the guards read the rest from the rows).
#[derive(Debug, Clone, Copy, Default)]
pub struct OutcomeFacts {
    pub verdict_verified: bool,
    pub delivery_ready: bool,
}

#[derive(Debug, Clone, Default)]
pub struct AttemptEnd {
    pub task_status: Option<TaskStatus>,
    pub subject_commit: Option<String>,
    pub retained_reason: Option<String>,
}

#[derive(Debug, Clone)]
pub struct NewDecision {
    pub kind: String,
    /// One open decision per key (ADR-016 item 4): a second insert returns the first.
    pub dedupe_key: String,
    pub brief: Value,
    /// Contract and plan versions, subject commit, policy hash, action hash, attempt ordinal.
    pub binding: Value,
    pub expires_at: String,
}

#[derive(Debug, Clone)]
pub struct NewEvidence {
    pub work_item_id: WorkItemId,
    pub attempt_id: Option<AttemptId>,
    pub trust_class: TrustClass,
    /// `gate`, `independent_runner`, `human`, `model` or `worker`.
    pub producer: String,
    pub subject_commit: String,
    pub check_id: String,
    pub command_hash: Option<String>,
    pub criteria_ids: Vec<String>,
    /// `passed`, `failed`, `error` or `skipped`.
    pub result: String,
    pub exit_code: Option<i64>,
    pub env_fingerprint: String,
    pub artifact: Option<(String, String, String)>,
    /// The fence of the attempt that produced it; an older one is rejected (ADR-017).
    pub fence: i64,
}

impl OutcomeWriter {
    pub fn id(&self) -> &OutcomeId {
        &self.id
    }

    pub fn control_session_id(&self) -> &str {
        self.w.id()
    }

    /// Runs `f` as one transaction. A busy database re-runs it, so it must be free of effects
    /// outside the transaction.
    pub fn atomically<T>(
        &mut self,
        mut f: impl FnMut(&mut OutcomeTx<'_, '_>) -> Result<T>,
    ) -> Result<T> {
        let id = &self.id;
        self.w.event_tx(|et| f(&mut OutcomeTx { et, id }))
    }

    pub fn approve_contract(&mut self, version: u32, hash: &str, approver: &str) -> Result<()> {
        self.atomically(|tx| tx.approve_contract(version, hash, approver))
    }

    pub fn insert_plan(&mut self, plan: &Plan) -> Result<Vec<(String, WorkItemId)>> {
        self.atomically(|tx| tx.insert_plan(plan))
    }

    pub fn transition_outcome(
        &mut self,
        expected_revision: i64,
        to: OutcomeState,
        facts: OutcomeFacts,
        reason: Option<&str>,
        fx: &Effects,
    ) -> Result<i64> {
        self.atomically(|tx| tx.transition_outcome(expected_revision, to, facts, reason, fx))
    }

    pub fn transition_work_item(
        &mut self,
        id: &WorkItemId,
        expected_revision: i64,
        to: WorkItemState,
        max_retries: u32,
        independent_evidence: bool,
        fx: &Effects,
    ) -> Result<i64> {
        self.atomically(|tx| {
            tx.transition_work_item(
                id,
                expected_revision,
                to,
                max_retries,
                independent_evidence,
                fx,
            )
        })
    }

    pub fn new_attempt(
        &mut self,
        item: &WorkItemId,
        ordinal: u32,
        session_id: &str,
        worker_kind: &str,
        fx: &Effects,
    ) -> Result<AttemptRecord> {
        self.atomically(|tx| tx.new_attempt(item, ordinal, session_id, worker_kind, fx))
    }

    pub fn transition_attempt(
        &mut self,
        id: &AttemptId,
        expected_revision: i64,
        to: AttemptState,
        end: &AttemptEnd,
        fx: &Effects,
    ) -> Result<i64> {
        self.atomically(|tx| tx.transition_attempt(id, expected_revision, to, end, fx))
    }

    pub fn new_decision(&mut self, d: &NewDecision) -> Result<(DecisionRecord, bool)> {
        self.atomically(|tx| tx.new_decision(d))
    }

    pub fn transition_decision(
        &mut self,
        id: &DecisionId,
        expected_revision: i64,
        to: DecisionState,
        guards: &DecisionGuards,
        fx: &Effects,
    ) -> Result<i64> {
        self.atomically(|tx| tx.transition_decision(id, expected_revision, to, guards, fx))
    }

    pub fn record_decision_approval(
        &mut self,
        id: &DecisionId,
        decision_revision: i64,
        approver: &str,
        choice: &str,
        brief_sha256: &str,
    ) -> Result<()> {
        self.atomically(|tx| {
            tx.record_decision_approval(id, decision_revision, approver, choice, brief_sha256)
        })
    }

    pub fn record_evidence(&mut self, e: &NewEvidence) -> Result<(EvidenceId, bool)> {
        self.atomically(|tx| tx.record_evidence(e))
    }

    /// Applies one queued command exactly once. `f` does the work through the transaction and
    /// returns the result state and body; a lost revision race inside `f` becomes `stale`. A
    /// command that is no longer queued is returned as stored, with no second effect.
    pub fn apply_command(
        &mut self,
        idempotency_key: &str,
        mut f: impl FnMut(&mut OutcomeTx<'_, '_>, &CommandRecord) -> Result<(CommandState, Value)>,
    ) -> Result<CommandRecord> {
        let run = self.atomically(|tx| {
            let cmd = tx.command(idempotency_key)?;
            if cmd.state != CommandState::Queued {
                return Ok(cmd);
            }
            let (state, result) = f(tx, &cmd)?;
            tx.finish_command(&cmd, state, result)
        });
        match run {
            Err(Error::RevisionConflict {
                aggregate,
                id,
                expected,
                actual,
            }) => self.atomically(|tx| {
                let cmd = tx.command(idempotency_key)?;
                if cmd.state != CommandState::Queued {
                    return Ok(cmd);
                }
                let result = json!({"aggregate": aggregate, "id": id, "expected": expected, "actual": actual});
                tx.finish_command(&cmd, CommandState::Stale, result)
            }),
            other => other,
        }
    }

    // ---- outbox

    /// Moves the next due `pending` row to `dispatching` (one more try) and returns it.
    pub fn claim_outbox(&mut self, now: OffsetDateTime) -> Result<Option<OutboxRecord>> {
        let at = ts(now);
        self.atomically(|tx| {
            let row = tx
                .et
                .tx
                .query_row(
                    &format!(
                        "SELECT {OUTBOX_COLS} FROM outbox WHERE outcome_id=?1 AND state='pending' \
                         AND (next_at IS NULL OR next_at<=?2) ORDER BY rowid LIMIT 1"
                    ),
                    params![tx.id.as_str(), at],
                    outbox_row,
                )
                .optional()?;
            let Some(mut row) = row else { return Ok(None) };
            tx.et.tx.execute(
                "UPDATE outbox SET state='dispatching', tries=tries+1 WHERE id=?1 AND state='pending'",
                [&row.id],
            )?;
            row.state = OutboxState::Dispatching;
            row.tries += 1;
            Ok(Some(row))
        })
    }

    pub fn complete_outbox(&mut self, id: &str, receipt: &str) -> Result<()> {
        self.atomically(|tx| {
            let mut receipt = Value::String(receipt.to_string());
            tx.et.redact_value(&mut receipt);
            tx.et.tx.execute(
                "UPDATE outbox SET state='done', receipt=?2, next_at=NULL WHERE id=?1 AND outcome_id=?3",
                params![id, receipt.as_str().unwrap_or_default(), tx.id.as_str()],
            )?;
            Ok(())
        })
    }

    /// Back to `pending` after a growing delay, or `failed` once [`OUTBOX_MAX_TRIES`] tries are
    /// used; the reason is kept in `receipt` (redacted).
    pub fn fail_outbox(&mut self, id: &str, now: OffsetDateTime, why: &str) -> Result<OutboxState> {
        self.atomically(|tx| {
            let tries: u32 = tx.et.tx.query_row(
                "SELECT tries FROM outbox WHERE id=?1 AND outcome_id=?2",
                params![id, tx.id.as_str()],
                |r| r.get(0),
            )?;
            let mut why = Value::String(why.to_string());
            tx.et.redact_value(&mut why);
            let (state, next) = if tries >= OUTBOX_MAX_TRIES {
                (OutboxState::Failed, None)
            } else {
                // 2, 4, 8, 16 seconds
                let delay = time::Duration::seconds(1_i64 << tries.min(10));
                (OutboxState::Pending, Some(ts(now + delay)))
            };
            tx.et.tx.execute(
                "UPDATE outbox SET state=?2, next_at=?3, receipt=?4 WHERE id=?1",
                params![id, enum_str(&state), next, why.as_str().unwrap_or_default()],
            )?;
            Ok(state)
        })
    }

    /// Restart path (ADR-016 item 5): rows left `dispatching` by a dead writer are asked
    /// [`OutboxEffect::probe`]; an effect that already happened is completed with its receipt,
    /// anything else goes back to `pending`. Then every due `pending` row runs once. At-least-once,
    /// so receivers must be idempotent on the row's key; this never runs a probed-done effect twice.
    pub fn run_outbox(
        &mut self,
        now: OffsetDateTime,
        effect: &mut dyn OutboxEffect,
    ) -> Result<OutboxRun> {
        let mut run = OutboxRun::default();
        let stuck: Vec<OutboxRecord> = self
            .list_outbox(&self.id.clone())?
            .into_iter()
            .filter(|r| r.state == OutboxState::Dispatching)
            .collect();
        for row in stuck {
            match effect.probe(&row)? {
                Some(receipt) => {
                    self.complete_outbox(&row.id, &receipt)?;
                    run.reconciled += 1;
                }
                None => {
                    self.atomically(|tx| {
                        tx.et.tx.execute(
                            "UPDATE outbox SET state='pending' WHERE id=?1 AND state='dispatching'",
                            [&row.id],
                        )?;
                        Ok(())
                    })?;
                }
            }
        }
        while let Some(row) = self.claim_outbox(now)? {
            match effect.run(&row) {
                Ok(receipt) => {
                    self.complete_outbox(&row.id, &receipt)?;
                    run.done += 1;
                }
                Err(e) => {
                    // the retry is due later (`next_at`), so this loop does not spin on it
                    if self.fail_outbox(&row.id, now, &e)? == OutboxState::Failed {
                        run.failed += 1;
                    }
                    run.retry_later += 1;
                }
            }
        }
        Ok(run)
    }
}

/// The side of an outbox effect that knows whether it already happened.
pub trait OutboxEffect {
    /// `Some(receipt)` when the effect of `row` is already in place (worktree exists, verifier
    /// artifact present), found without running it.
    fn probe(&mut self, row: &OutboxRecord) -> Result<Option<String>>;
    /// Runs the effect and returns its receipt. Must be idempotent on `row.idempotency_key`.
    fn run(&mut self, row: &OutboxRecord) -> std::result::Result<String, String>;
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OutboxRun {
    pub done: u32,
    pub reconciled: u32,
    /// Rows that failed and were put back (or failed for good).
    pub retry_later: u32,
    pub failed: u32,
}

// ---------------------------------------------------------------- transaction

/// The primitives of one outcome transaction. Every method that changes a row appends its event
/// in the same transaction.
pub struct OutcomeTx<'a, 'e> {
    et: &'a mut EventTx<'e>,
    id: &'a OutcomeId,
}

impl OutcomeTx<'_, '_> {
    pub fn emit(&mut self, kind: &str, payload: Value) -> Result<u64> {
        self.et.append(kind, payload)
    }

    /// The outcome's current revision (what a command without `expected_revision` swaps on).
    pub fn outcome_revision(&self) -> Result<i64> {
        Ok(self.et.tx.query_row(
            "SELECT revision FROM outcomes WHERE id=?1",
            [self.id.as_str()],
            |r| r.get(0),
        )?)
    }

    fn effects(&mut self, fx: &Effects) -> Result<()> {
        for (kind, payload) in &fx.events {
            self.et.append(kind, payload.clone())?;
        }
        for o in &fx.outbox {
            self.enqueue(o)?;
        }
        Ok(())
    }

    /// Inserts an outbox row; `false` when the key already exists (nothing changes).
    pub fn enqueue(&mut self, o: &NewOutbox) -> Result<bool> {
        let mut payload = o.payload.clone();
        self.et.redact_value(&mut payload);
        Ok(self.et.tx.execute(
            "INSERT INTO outbox(id,outcome_id,idempotency_key,kind,payload,state) \
             VALUES (?1,?2,?3,?4,?5,'pending') ON CONFLICT(idempotency_key) DO NOTHING",
            params![
                new_uuid(),
                self.id.as_str(),
                o.idempotency_key,
                enum_str(&o.kind),
                payload.to_string()
            ],
        )? == 1)
    }

    fn changed(
        &mut self,
        agg: ev::Aggregate,
        id: &str,
        revision: i64,
        from: &str,
        to: &str,
        reason: Option<&str>,
    ) -> Result<()> {
        let payload = serde_json::to_value(ev::StateChanged {
            aggregate: agg,
            aggregate_id: id.to_string(),
            revision,
            from: from.to_string(),
            to: to.to_string(),
            reason: reason.map(str::to_string),
        })?;
        self.et.append(&agg.state_changed_kind(), payload)?;
        Ok(())
    }

    // ---- contract and plan

    pub fn approve_contract(&mut self, version: u32, hash: &str, approver: &str) -> Result<()> {
        let stored: Option<(String, Option<String>)> = self
            .et
            .tx
            .query_row(
                "SELECT hash,approved_at FROM contracts WHERE outcome_id=?1 AND version=?2",
                params![self.id.as_str(), version],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        let Some((stored, approved)) = stored else {
            return Err(Error::NotFound(format!("contract v{version}")));
        };
        if stored != hash {
            // the person approved something other than what is stored
            return Err(Error::Invalid(
                "contract hash does not match the stored contract".into(),
            ));
        }
        if approved.is_some() {
            return Ok(());
        }
        let at = now();
        self.et.tx.execute(
            "UPDATE contracts SET approved_by=?3, approved_at=?4 WHERE outcome_id=?1 AND version=?2",
            params![self.id.as_str(), version, approver, at],
        )?;
        self.et.append(
            "outcome.contract_approved",
            json!({"outcome_id": self.id, "version": version, "hash": hash, "approver": approver}),
        )?;
        Ok(())
    }

    /// Validates `plan` against the stored contract of its version, stores it with its work
    /// items (state `created`) and dependencies, and makes it the outcome's current plan.
    /// Returns `(key, id)` of every work item.
    pub fn insert_plan(&mut self, plan: &Plan) -> Result<Vec<(String, WorkItemId)>> {
        let (body, chash): (String, String) = self.et.tx.query_row(
            "SELECT body,hash FROM contracts WHERE outcome_id=?1 AND version=?2",
            params![self.id.as_str(), plan.contract_version],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        let contract: Contract = serde_json::from_str(&body)?;
        validate_plan(plan, &contract).map_err(refuse)?;
        let dag = serde_json::to_value(plan)?;
        let hash = body_hash(&dag);
        self.et.tx.execute(
            "INSERT INTO plans(outcome_id,version,contract_version,hash,dag) VALUES (?1,?2,?3,?4,?5)",
            params![self.id.as_str(), plan.version, plan.contract_version, hash, dag.to_string()],
        )?;
        let mut ids = Vec::with_capacity(plan.items.len());
        for it in &plan.items {
            let id = WorkItemId::parse(&new_uuid()).map_err(refuse)?;
            self.et.tx.execute(
                "INSERT INTO work_items(id,outcome_id,plan_version,key,kind,spec,state,revision) \
                 VALUES (?1,?2,?3,?4,?5,?6,'created',1)",
                params![
                    id.as_str(),
                    self.id.as_str(),
                    plan.version,
                    it.key,
                    enum_str(&it.kind),
                    serde_json::to_string(it)?
                ],
            )?;
            ids.push((it.key.clone(), id));
        }
        for it in &plan.items {
            let id = &ids.iter().find(|(k, _)| *k == it.key).expect("inserted").1;
            for dep in &it.requires {
                let dep = &ids.iter().find(|(k, _)| k == dep).expect("validated").1;
                self.et.tx.execute(
                    "INSERT INTO work_item_deps(work_item_id,requires_id) VALUES (?1,?2)",
                    params![id.as_str(), dep.as_str()],
                )?;
            }
        }
        self.et.tx.execute(
            "UPDATE outcomes SET plan_version=?2, revision=revision+1, updated_at=?3 WHERE id=?1",
            params![self.id.as_str(), plan.version, now()],
        )?;
        self.et.append(
            ev::PLAN_CREATED,
            json!({"outcome_id": self.id, "version": plan.version,
                   "contract_version": plan.contract_version, "contract_hash": chash,
                   "plan_hash": hash, "items": plan.items.len()}),
        )?;
        Ok(ids)
    }

    // ---- transitions

    fn conflict_or(&self, agg: ev::Aggregate, id: &str, expected: i64, actual: i64) -> Result<()> {
        if expected == actual {
            Ok(())
        } else {
            Err(conflict(agg.as_str(), id, expected, actual))
        }
    }

    pub fn transition_outcome(
        &mut self,
        expected_revision: i64,
        to: OutcomeState,
        facts: OutcomeFacts,
        reason: Option<&str>,
        fx: &Effects,
    ) -> Result<i64> {
        let (state, rev, approved, plan_ok): (String, i64, bool, bool) = self
            .et
            .tx
            .query_row(
                "SELECT o.state,o.revision, \
                   EXISTS(SELECT 1 FROM contracts c WHERE c.outcome_id=o.id AND c.version=o.contract_version AND c.approved_at IS NOT NULL), \
                   EXISTS(SELECT 1 FROM plans p WHERE p.outcome_id=o.id AND p.version=o.plan_version AND p.contract_version=o.contract_version) \
                 FROM outcomes o WHERE o.id=?1",
                [self.id.as_str()],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .optional()?
            .ok_or_else(|| Error::NotFound(format!("outcome {}", self.id)))?;
        self.conflict_or(
            ev::Aggregate::Outcome,
            self.id.as_str(),
            expected_revision,
            rev,
        )?;
        let from: OutcomeState = enum_from(&state)?;
        let guards = OutcomeGuards {
            contract_approved: approved,
            plan_valid: plan_ok,
            verdict_verified: facts.verdict_verified,
            delivery_ready: facts.delivery_ready,
        };
        from.transition(to, &guards).map_err(refuse)?;
        let changed = self.et.tx.execute(
            "UPDATE outcomes SET state=?2, revision=revision+1, updated_at=?4 WHERE id=?1 AND revision=?3",
            params![self.id.as_str(), enum_str(&to), rev, now()],
        )?;
        if changed != 1 {
            return Err(conflict("outcome", self.id.as_str(), rev, rev + 1));
        }
        self.changed(
            ev::Aggregate::Outcome,
            &self.id.to_string(),
            rev + 1,
            &state,
            &enum_str(&to),
            reason,
        )?;
        self.effects(fx)?;
        Ok(rev + 1)
    }

    fn load_item(&self, id: &WorkItemId) -> Result<(String, i64, u32)> {
        self.et
            .tx
            .query_row(
                "SELECT state,revision,retries_used FROM work_items WHERE id=?1 AND outcome_id=?2",
                params![id.as_str(), self.id.as_str()],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?
            .ok_or_else(|| Error::NotFound(format!("work item {id}")))
    }

    /// `max_retries` comes from the contract's budgets; `retries_used` is read from the row.
    /// Leaving `retryable_failure` for `ready` starts a retry and counts it.
    pub fn transition_work_item(
        &mut self,
        id: &WorkItemId,
        expected_revision: i64,
        to: WorkItemState,
        max_retries: u32,
        independent_evidence: bool,
        fx: &Effects,
    ) -> Result<i64> {
        let (state, rev, used) = self.load_item(id)?;
        self.conflict_or(ev::Aggregate::WorkItem, id.as_str(), expected_revision, rev)?;
        let from: WorkItemState = enum_from(&state)?;
        let guards = WorkItemGuards {
            retries_used: used,
            max_retries,
            independent_evidence,
        };
        from.transition(to, &guards).map_err(refuse)?;
        let retry =
            u32::from(from == WorkItemState::RetryableFailure && to == WorkItemState::Ready);
        let changed = self.et.tx.execute(
            "UPDATE work_items SET state=?2, revision=revision+1, retries_used=retries_used+?4 WHERE id=?1 AND revision=?3",
            params![id.as_str(), enum_str(&to), rev, retry],
        )?;
        if changed != 1 {
            return Err(conflict("work_item", id.as_str(), rev, rev + 1));
        }
        self.changed(
            ev::Aggregate::WorkItem,
            id.as_str(),
            rev + 1,
            &state,
            &enum_str(&to),
            None,
        )?;
        self.effects(fx)?;
        Ok(rev + 1)
    }

    // ---- attempts

    /// Inserts attempt `ordinal` of a work item in state `starting`. The pair
    /// `(work_item, ordinal)` is unique, so a replayed allocation is refused instead of doubled
    /// ([`Error::AlreadyExists`]); the caller adopts the existing row. The fence equals the ordinal.
    pub fn new_attempt(
        &mut self,
        item: &WorkItemId,
        ordinal: u32,
        session_id: &str,
        worker_kind: &str,
        fx: &Effects,
    ) -> Result<AttemptRecord> {
        if ordinal == 0 || !matches!(worker_kind, "local-inproc" | "local-engine") {
            return Err(Error::Invalid(
                "attempt ordinal or worker kind is invalid".into(),
            ));
        }
        self.load_item(item)?;
        let id = AttemptId::parse(&new_uuid()).map_err(refuse)?;
        let r = self.et.tx.execute(
            "INSERT INTO attempts(id,work_item_id,ordinal,session_id,worker_kind,state,revision,fence,started_at) \
             VALUES (?1,?2,?3,?4,?5,'starting',1,?3,?6)",
            params![id.as_str(), item.as_str(), ordinal, session_id, worker_kind, now()],
        );
        match r {
            Err(rusqlite::Error::SqliteFailure(e, _))
                if e.extended_code == rusqlite::ffi::SQLITE_CONSTRAINT_UNIQUE =>
            {
                return Err(Error::AlreadyExists(format!("attempt {ordinal} of {item}")));
            }
            r => r?,
        };
        self.et.append(
            ev::ATTEMPT_STARTED,
            json!({"attempt_id": id, "work_item_id": item, "ordinal": ordinal,
                   "session_id": session_id, "worker_kind": worker_kind, "fence": ordinal}),
        )?;
        self.effects(fx)?;
        Ok(self.et.tx.query_row(
            &format!("SELECT {ATTEMPT_COLS} FROM attempts a WHERE a.id=?1"),
            [id.as_str()],
            attempt_row,
        )?)
    }

    pub fn transition_attempt(
        &mut self,
        id: &AttemptId,
        expected_revision: i64,
        to: AttemptState,
        end: &AttemptEnd,
        fx: &Effects,
    ) -> Result<i64> {
        let (state, rev): (String, i64) = self
            .et
            .tx
            .query_row(
                "SELECT a.state,a.revision FROM attempts a JOIN work_items w ON w.id=a.work_item_id \
                 WHERE a.id=?1 AND w.outcome_id=?2",
                params![id.as_str(), self.id.as_str()],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?
            .ok_or_else(|| Error::NotFound(format!("attempt {id}")))?;
        self.conflict_or(ev::Aggregate::Attempt, id.as_str(), expected_revision, rev)?;
        let from: AttemptState = enum_from(&state)?;
        from.transition(to).map_err(refuse)?;
        let ended = to.is_terminal().then(now);
        let changed = self.et.tx.execute(
            "UPDATE attempts SET state=?2, revision=revision+1, ended_at=COALESCE(?4,ended_at), \
               task_status=COALESCE(?5,task_status), subject_commit=COALESCE(?6,subject_commit), \
               retained_reason=COALESCE(?7,retained_reason) WHERE id=?1 AND revision=?3",
            params![
                id.as_str(),
                enum_str(&to),
                rev,
                ended,
                end.task_status.as_ref().map(enum_str),
                end.subject_commit,
                end.retained_reason
            ],
        )?;
        if changed != 1 {
            return Err(conflict("attempt", id.as_str(), rev, rev + 1));
        }
        self.changed(
            ev::Aggregate::Attempt,
            id.as_str(),
            rev + 1,
            &state,
            &enum_str(&to),
            None,
        )?;
        self.effects(fx)?;
        Ok(rev + 1)
    }

    // ---- decisions

    /// Creates a decision in `preparing`, or returns the existing one of the same dedupe key
    /// (`false`: nothing was created).
    pub fn new_decision(&mut self, d: &NewDecision) -> Result<(DecisionRecord, bool)> {
        if !d.brief.is_object()
            || !d.binding.is_object()
            || d.kind.is_empty()
            || d.dedupe_key.is_empty()
        {
            return Err(Error::Invalid(
                "a decision needs a kind, a key, a brief and a binding".into(),
            ));
        }
        let (mut brief, mut binding) = (d.brief.clone(), d.binding.clone());
        self.et.redact_value(&mut brief);
        self.et.redact_value(&mut binding);
        let id = DecisionId::parse(&new_uuid()).map_err(refuse)?;
        let inserted = self.et.tx.execute(
            "INSERT INTO decisions(id,outcome_id,kind,dedupe_key,state,revision,brief,binding,expires_at,created_at) \
             VALUES (?1,?2,?3,?4,'preparing',1,?5,?6,?7,?8) ON CONFLICT(outcome_id,dedupe_key) DO NOTHING",
            params![
                id.as_str(),
                self.id.as_str(),
                d.kind,
                d.dedupe_key,
                brief.to_string(),
                binding.to_string(),
                d.expires_at,
                now()
            ],
        )? == 1;
        let rec = self.et.tx.query_row(
            &format!("SELECT {DECISION_COLS} FROM decisions WHERE outcome_id=?1 AND dedupe_key=?2"),
            params![self.id.as_str(), d.dedupe_key],
            decision_row,
        )?;
        if inserted {
            self.et.append(
                ev::DECISION_CREATED,
                json!({"decision_id": rec.id, "kind": rec.kind, "binding_hash": body_hash(&rec.binding),
                       "revision": 1}),
            )?;
        }
        Ok((rec, inserted))
    }

    /// Transition with the ADR-019 guards. An approval or rejection by a person also needs its
    /// audit row: [`Self::record_decision_approval`] in the same transaction.
    pub fn transition_decision(
        &mut self,
        id: &DecisionId,
        expected_revision: i64,
        to: DecisionState,
        guards: &DecisionGuards,
        fx: &Effects,
    ) -> Result<i64> {
        let (state, rev): (String, i64) = self
            .et
            .tx
            .query_row(
                "SELECT state,revision FROM decisions WHERE id=?1 AND outcome_id=?2",
                params![id.as_str(), self.id.as_str()],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?
            .ok_or_else(|| Error::NotFound(format!("decision {id}")))?;
        self.conflict_or(ev::Aggregate::Decision, id.as_str(), expected_revision, rev)?;
        let from: DecisionState = enum_from(&state)?;
        from.transition(to, guards).map_err(refuse)?;
        let changed = self.et.tx.execute(
            "UPDATE decisions SET state=?2, revision=revision+1 WHERE id=?1 AND revision=?3",
            params![id.as_str(), enum_str(&to), rev],
        )?;
        if changed != 1 {
            return Err(conflict("decision", id.as_str(), rev, rev + 1));
        }
        self.changed(
            ev::Aggregate::Decision,
            id.as_str(),
            rev + 1,
            &state,
            &enum_str(&to),
            None,
        )?;
        self.effects(fx)?;
        Ok(rev + 1)
    }

    /// What the person was shown and chose. `brief_sha256` is the hash of the brief as displayed.
    pub fn record_decision_approval(
        &mut self,
        id: &DecisionId,
        decision_revision: i64,
        approver: &str,
        choice: &str,
        brief_sha256: &str,
    ) -> Result<()> {
        self.et
            .tx
            .query_row(
                "SELECT 1 FROM decisions WHERE id=?1 AND outcome_id=?2",
                params![id.as_str(), self.id.as_str()],
                |_| Ok(()),
            )
            .optional()?
            .ok_or_else(|| Error::NotFound(format!("decision {id}")))?;
        self.et.tx.execute(
            "INSERT INTO decision_approvals(id,decision_id,decision_revision,approver,choice,brief_sha256,decided_at) \
             VALUES (?1,?2,?3,?4,?5,?6,?7)",
            params![new_uuid(), id.as_str(), decision_revision, approver, choice, brief_sha256, now()],
        )?;
        Ok(())
    }

    // ---- evidence

    /// Inserts evidence. Refused: a trust class its producer cannot honestly hold (only the
    /// independent runner writes `independent_check`, only a person `human_review`), an attempt
    /// that is not this outcome's, and a stale fence (a newer attempt of the item exists).
    /// Idempotent on `(attempt, check, commit)`: returns the stored id and `false`.
    pub fn record_evidence(&mut self, e: &NewEvidence) -> Result<(EvidenceId, bool)> {
        let producer_ok = match e.trust_class {
            TrustClass::IndependentCheck => e.producer == "independent_runner",
            TrustClass::HumanReview => e.producer == "human",
            TrustClass::ObservedInAttempt => e.producer == "gate",
            TrustClass::ModelAssessment => e.producer == "model",
            TrustClass::ClaimedByWorker => e.producer == "worker",
        };
        if !producer_ok {
            return Err(Error::Invalid(format!(
                "producer {:?} cannot write {} evidence",
                e.producer,
                enum_str(&e.trust_class)
            )));
        }
        if !matches!(e.result.as_str(), "passed" | "failed" | "error" | "skipped")
            || e.check_id.is_empty()
            || e.subject_commit.is_empty()
        {
            return Err(Error::Invalid(
                "evidence needs a result, a check and a commit".into(),
            ));
        }
        self.load_item(&e.work_item_id)?;
        let newest: Option<i64> = self.et.tx.query_row(
            "SELECT MAX(fence) FROM attempts WHERE work_item_id=?1",
            [e.work_item_id.as_str()],
            |r| r.get(0),
        )?;
        if newest.is_some_and(|n| e.fence < n) {
            return Err(Error::Invalid(format!(
                "stale evidence: fence {} is older than the item's newest attempt",
                e.fence
            )));
        }
        if let Some(a) = &e.attempt_id {
            let (item, fence): (String, i64) = self
                .et
                .tx
                .query_row(
                    "SELECT work_item_id,fence FROM attempts WHERE id=?1",
                    [a.as_str()],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .optional()?
                .ok_or_else(|| Error::NotFound(format!("attempt {a}")))?;
            if item != e.work_item_id.as_str() || fence != e.fence {
                return Err(Error::Invalid(
                    "attempt does not match the item or the fence".into(),
                ));
            }
        }
        let id = EvidenceId::parse(&new_uuid()).map_err(refuse)?;
        let (art_session, art_id, sha) = match &e.artifact {
            Some((s, a, h)) => (Some(s.as_str()), Some(a.as_str()), Some(h.as_str())),
            None => (None, None, None),
        };
        let inserted = self.et.tx.execute(
            "INSERT INTO evidence(id,outcome_id,work_item_id,attempt_id,trust_class,producer,subject_commit,check_id,command_hash,\
               criteria_ids,result,exit_code,env_fingerprint,artifact_session_id,artifact_id,sha256,fence,captured_at) \
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18) \
             ON CONFLICT(attempt_id,check_id,subject_commit) DO NOTHING",
            params![
                id.as_str(),
                self.id.as_str(),
                e.work_item_id.as_str(),
                e.attempt_id.as_ref().map(AttemptId::as_str),
                enum_str(&e.trust_class),
                e.producer,
                e.subject_commit,
                e.check_id,
                e.command_hash,
                serde_json::to_string(&e.criteria_ids)?,
                e.result,
                e.exit_code,
                e.env_fingerprint,
                art_session,
                art_id,
                sha,
                e.fence,
                now()
            ],
        )? == 1;
        if !inserted {
            let existing: String = self.et.tx.query_row(
                "SELECT id FROM evidence WHERE attempt_id IS ?1 AND check_id=?2 AND subject_commit=?3",
                params![e.attempt_id.as_ref().map(AttemptId::as_str), e.check_id, e.subject_commit],
                |r| r.get(0),
            )?;
            return Ok((EvidenceId::parse(&existing).map_err(refuse)?, false));
        }
        self.et.append(
            ev::EVIDENCE_RECORDED,
            json!({"evidence_id": id, "work_item_id": e.work_item_id, "check_id": e.check_id,
                   "trust_class": e.trust_class, "result": e.result, "sha256": sha,
                   "subject_commit": e.subject_commit, "fence": e.fence}),
        )?;
        Ok((id, true))
    }

    // ---- commands

    fn command(&self, key: &str) -> Result<CommandRecord> {
        self.et
            .tx
            .query_row(
                &format!(
                    "SELECT {COMMAND_COLS} FROM outcome_commands WHERE idempotency_key=?1 AND outcome_id=?2"
                ),
                params![key, self.id.as_str()],
                command_row,
            )
            .optional()?
            .ok_or_else(|| Error::NotFound(format!("command {key}")))
    }

    /// Writes the result back and stamps the audit event. Only a `queued` command changes.
    pub fn finish_command(
        &mut self,
        cmd: &CommandRecord,
        state: CommandState,
        mut result: Value,
    ) -> Result<CommandRecord> {
        if state == CommandState::Queued {
            return Err(Error::Invalid("a finished command cannot be queued".into()));
        }
        self.et.redact_value(&mut result);
        let changed = self.et.tx.execute(
            "UPDATE outcome_commands SET state=?2, result=?3, applied_at=?4 WHERE id=?1 AND state='queued'",
            params![cmd.id.as_str(), enum_str(&state), result.to_string(), now()],
        )?;
        if changed == 1 {
            self.et.append(
                ev::COMMAND_FINISHED,
                json!({"command_id": cmd.id, "idempotency_key": cmd.idempotency_key,
                       "kind": cmd.kind, "from_surface": cmd.from_surface, "state": state}),
            )?;
        }
        self.command(&cmd.idempotency_key)
    }
}
