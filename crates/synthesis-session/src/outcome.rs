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

use hmac::{Hmac, Mac as _};
use rusqlite::{Connection, OptionalExtension, Row, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::Sha256;
use synthesis_core::outcome::{
    Actor, AttemptId, AttemptState, CommandId, Contract, ContractKind, DecisionGuards, DecisionId,
    DecisionState, EvidenceId, OutcomeGuards, OutcomeId, OutcomeState, Plan, TrustClass,
    WorkItemGuards, WorkItemId, WorkItemKind, WorkItemState, diff_contracts, validate_contract,
    validate_plan,
};
use synthesis_core::{PermissionMode, TaskStatus, WorkspaceContext};
use synthesis_events::{Budget, outcome as ev};
use time::OffsetDateTime;

use crate::clean::ts;
use crate::error::{Error, Result};
use crate::schema;
use crate::store::{
    EventTx, NewSession, Reader, SessionWriter, Store, bad, connect, enum_from, enum_str, now,
    sha_hex, utf8, valid_id,
};

/// Every list is bounded: a hostile or runaway writer cannot make a reader allocate without limit.
pub const LIST_LIMIT: usize = 1000;
/// A command's params, and how many commands may wait for an owner per outcome.
pub const MAX_COMMAND_PARAMS_BYTES: usize = 64 * 1024;
pub const MAX_QUEUED_COMMANDS: i64 = 1000;
/// An outbox payload is bounded like a command's params.
pub const MAX_OUTBOX_PAYLOAD_BYTES: usize = 64 * 1024;

/// Backoff cap of the outbox: after this many tries a row stays `failed` for a person to look at.
pub const OUTBOX_MAX_TRIES: u32 = 5;

fn need_v5(conn: &Connection) -> Result<()> {
    if schema::user_version(conn)? < schema::OUTCOMES {
        return Err(Error::OutcomesNotEnabled);
    }
    // a v5 made by an earlier draft of the DDL lacks the triggers and MAC columns this code relies on
    let marked: bool = conn.query_row(
        "SELECT EXISTS (SELECT 1 FROM schema_migrations WHERE version = ?1)",
        [schema::OUTCOME_DDL_MARKER],
        |r| r.get(0),
    )?;
    if marked {
        Ok(())
    } else {
        Err(Error::OutcomeSchemaOutdated)
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
    /// The worker process (`synt run`) and the start-time token that tells it from a pid reuser.
    pub worker_pid: Option<u32>,
    pub worker_start: Option<String>,
    /// Last sign of life of the worker; a lease that is older than the owner's limit is expired.
    pub heartbeat_at: Option<String>,
    /// The cost cap allocated to this attempt when its worker started. It counts as spent until
    /// the session records a final usage (then the recorded cost counts, never less).
    pub reserved_usd: Option<f64>,
    /// The contract version and the hash of the compiled policy the attempt started under: a
    /// Decision of the attempt binds to these, not to whatever the outcome says later.
    pub bound_contract_version: Option<u32>,
    pub bound_policy_hash: Option<String>,
    /// The sandbox backend and its filesystem guarantee (`none`/`weak`/`strong`) the OWNER resolved
    /// when it launched the worker. Never reported by the worker; absent for older attempts.
    pub sandbox_backend: Option<String>,
    pub sandbox_guarantee: Option<String>,
    pub started_at: String,
    pub ended_at: Option<String>,
}

/// See [`Reader::outcome_usage`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct OutcomeUsage {
    pub cost_usd: f64,
    pub wall_ms: u64,
    pub reserved_usd: f64,
    /// What the planning runs cost (a run not settled with a cost counts its reservation): already
    /// part of `cost_usd` / `reserved_usd`, reported apart so a surface can say what planning took.
    pub planner_usd: f64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct DecisionRecord {
    pub id: DecisionId,
    pub kind: String,
    pub dedupe_key: String,
    /// The raw `state` column. NOT authoritative for a resolved decision (anyone who can write
    /// the file can change it): use [`Reader::decision_state`] or [`Reader::verified_resolution`].
    pub row_state: DecisionState,
    pub revision: i64,
    pub brief: Value,
    /// Hash of the stored brief as shown: the resolve command must echo it back.
    pub brief_sha256: String,
    pub binding: Value,
    pub binding_hash: String,
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
    /// Disqualifying flags written with the row (`definitions_changed`, `tests_weakened`,
    /// `outside_envelope`, `mode_changed`).
    pub flags: Vec<String>,
    /// The control session has a chained `evidence.recorded` event with the same content (necessary
    /// for the row to count, not sufficient: the MAC is the other half). Computed by the store.
    pub event_backed: bool,
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
    /// A told-once record of an owner notification (ids and codes): written and marked `done`
    /// before the hook runs, so a restart never tells twice (at-most-once, ADR-021 section 4).
    #[serde(rename = "notify")]
    Notify,
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

/// The outcome's spend, attempts and planning runs together (see [`Reader::outcome_spend`]). A
/// planning run counts its settled cost, or its reservation until it is settled (a lost run keeps
/// it).
fn spend(conn: &Connection, id: &OutcomeId) -> Result<f64> {
    Ok(conn.query_row(
        "SELECT COALESCE(SUM(CASE \
           WHEN s.status IN ('completed','unverified','blocked','failed','cancelled') \
            AND (COALESCE(json_extract(s.usage,'$.turns'),0) > 0 \
              OR COALESCE(json_extract(s.usage,'$.wall_ms'),0) > 0 \
              OR COALESCE(json_extract(s.usage,'$.cost_usd'),0) > 0) \
             THEN COALESCE(json_extract(s.usage,'$.cost_usd'),0) \
           ELSE MAX(COALESCE(a.reserved_usd,0), COALESCE(json_extract(s.usage,'$.cost_usd'),0)) \
         END),0.0) \
         + (SELECT COALESCE(SUM(COALESCE(cost_usd, reserved_usd)),0.0) FROM plan_runs WHERE outcome_id=?1) \
         FROM attempts a JOIN work_items w ON w.id=a.work_item_id \
         JOIN sessions s ON s.id=a.session_id WHERE w.outcome_id=?1",
        [id.as_str()],
        |r| r.get(0),
    )?)
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

const ATTEMPT_COLS: &str = "a.id,a.work_item_id,a.ordinal,a.session_id,a.worker_kind,a.state,a.revision,a.fence,a.task_status,a.subject_commit,a.retained_reason,a.worker_pid,a.worker_start,a.heartbeat_at,a.reserved_usd,a.bound_contract_version,a.bound_policy_hash,a.started_at,a.ended_at,a.sandbox_backend,a.sandbox_guarantee";
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
        worker_pid: r.get(11)?,
        worker_start: r.get(12)?,
        heartbeat_at: r.get(13)?,
        reserved_usd: r.get(14)?,
        bound_contract_version: r.get(15)?,
        bound_policy_hash: r.get(16)?,
        started_at: r.get(17)?,
        ended_at: r.get(18)?,
        sandbox_backend: r.get(19)?,
        sandbox_guarantee: r.get(20)?,
    })
}

const DECISION_COLS: &str =
    "id,kind,dedupe_key,state,revision,brief,binding,expires_at,created_at,binding_hash";
fn decision_row(r: &Row<'_>) -> rusqlite::Result<DecisionRecord> {
    Ok(DecisionRecord {
        id: id_col(r, 0)?,
        kind: r.get(1)?,
        dedupe_key: r.get(2)?,
        row_state: col(r, 3)?,
        revision: r.get(4)?,
        brief_sha256: sha_hex(r.get::<_, String>(5)?.as_bytes()),
        brief: json_col(r, 5)?,
        binding: json_col(r, 6)?,
        binding_hash: r.get(9)?,
        expires_at: r.get(7)?,
        created_at: r.get(8)?,
    })
}

const EVIDENCE_COLS: &str = "id,work_item_id,attempt_id,trust_class,producer,subject_commit,check_id,criteria_ids,result,exit_code,sha256,fence,captured_at,flags";
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
        flags: serde_json::from_value(json_col(r, 13)?).map_err(bad)?,
        event_backed: r.get(14)?,
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

/// Most unapproved revisions of one outcome (each `outcome.contract.revise` stores a row).
const MAX_CONTRACT_DRAFTS: i64 = 20;

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
            "SELECT {OUTCOME_COLS} FROM outcomes ORDER BY created_at DESC, id LIMIT {LIST_LIMIT}"
        ))?;
        Ok(st
            .query_map([], outcome_row)?
            .collect::<rusqlite::Result<_>>()?)
    }

    /// The stored contract of a version and whether a person approved it.
    /// The stored contract, its hash, and whether its approval **verifies** (MAC and body hash,
    /// [`Self::contract_is_approved`]); an `approved_at` written by anything else is `false`.
    pub fn get_contract(&self, id: &OutcomeId, version: u32) -> Result<(Contract, String, bool)> {
        need_v5(&self.conn)?;
        let (body, hash): (String, String) = self
            .conn
            .query_row(
                "SELECT body,hash FROM contracts WHERE outcome_id=?1 AND version=?2",
                params![id.as_str(), version],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?
            .ok_or_else(|| Error::NotFound(format!("contract {id} v{version}")))?;
        let key = crate::trust::existing_chain_key(&self.home);
        let approved = contract_authentic(&self.conn, key.as_ref(), id, version)?;
        Ok((serde_json::from_str(&body)?, hash, approved))
    }

    /// The stored plan of `version`.
    pub fn get_plan(&self, id: &OutcomeId, version: u32) -> Result<Plan> {
        need_v5(&self.conn)?;
        let dag: String = self
            .conn
            .query_row(
                "SELECT dag FROM plans WHERE outcome_id=?1 AND version=?2",
                params![id.as_str(), version],
                |r| r.get(0),
            )
            .optional()?
            .ok_or_else(|| Error::NotFound(format!("plan v{version}")))?;
        Ok(serde_json::from_str(&dag)?)
    }

    pub fn list_work_items(&self, id: &OutcomeId) -> Result<Vec<WorkItemRecord>> {
        need_v5(&self.conn)?;
        let mut st = self.conn.prepare(&format!(
            "SELECT {ITEM_COLS} FROM work_items WHERE outcome_id=?1 ORDER BY plan_version, key LIMIT {LIST_LIMIT}"
        ))?;
        Ok(st
            .query_map([id.as_str()], item_row)?
            .collect::<rusqlite::Result<_>>()?)
    }

    pub fn get_work_item(&self, id: &WorkItemId) -> Result<WorkItemRecord> {
        need_v5(&self.conn)?;
        self.conn
            .query_row(
                &format!("SELECT {ITEM_COLS} FROM work_items WHERE id=?1"),
                [id.as_str()],
                item_row,
            )
            .optional()?
            .ok_or_else(|| Error::NotFound(format!("work item {id}")))
    }

    /// Keys of the items a work item requires.
    pub fn work_item_deps(&self, id: &WorkItemId) -> Result<Vec<String>> {
        need_v5(&self.conn)?;
        let mut st = self.conn.prepare(
            "SELECT r.key FROM work_item_deps d JOIN work_items r ON r.id=d.requires_id \
             WHERE d.work_item_id=?1 ORDER BY r.key LIMIT 1000",
        )?;
        Ok(st
            .query_map([id.as_str()], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?)
    }

    pub fn list_attempts(&self, item: &WorkItemId) -> Result<Vec<AttemptRecord>> {
        need_v5(&self.conn)?;
        let mut st = self.conn.prepare(&format!(
            "SELECT {ATTEMPT_COLS} FROM attempts a WHERE a.work_item_id=?1 ORDER BY a.ordinal LIMIT {LIST_LIMIT}"
        ))?;
        Ok(st
            .query_map([item.as_str()], attempt_row)?
            .collect::<rusqlite::Result<_>>()?)
    }

    /// What the outcome's attempts have cost, as an UPPER BOUND: for every attempt whose session
    /// ended with a final usage recorded (the session's usage is not the all-zero default), that
    /// cost; for every other attempt (running, killed,
    /// crashed, abandoned, wall-killed) the larger of its reservation (the cap allocated when its
    /// worker started) and whatever usage was written. A cancelled or lost attempt with a
    /// final usage counts only that usage. Used to lower the cap of the next attempt: N attempts
    /// must not spend N times the outcome cap, and parallel starts must not over-commit it.
    pub fn outcome_spend(&self, id: &OutcomeId) -> Result<f64> {
        need_v5(&self.conn)?;
        spend(&self.conn, id)
    }

    /// What the outcome's attempts used, as the store has it: the recorded cost and wall time of
    /// the attempts' sessions, and the reservations not yet replaced by a final usage (the same
    /// rule as [`Self::outcome_spend`]), reported apart: a reservation is a cap, not a spend. The
    /// store records no token counts.
    pub fn outcome_usage(&self, id: &OutcomeId) -> Result<OutcomeUsage> {
        need_v5(&self.conn)?;
        let planner_usd: f64 = self.conn.query_row(
            "SELECT COALESCE(SUM(COALESCE(cost_usd, reserved_usd)),0.0) FROM plan_runs WHERE outcome_id=?1",
            [id.as_str()],
            |r| r.get(0),
        )?;
        Ok(self.conn.query_row(
            "SELECT COALESCE(SUM(COALESCE(json_extract(s.usage,'$.cost_usd'),0)),0.0) \
                      + (SELECT COALESCE(SUM(cost_usd),0.0) FROM plan_runs WHERE outcome_id=?1), \
                    COALESCE(SUM(COALESCE(json_extract(s.usage,'$.wall_ms'),0)),0), \
                    COALESCE(SUM(CASE \
                      WHEN s.status IN ('completed','unverified','blocked','failed','cancelled') \
                       AND (COALESCE(json_extract(s.usage,'$.turns'),0) > 0 \
                         OR COALESCE(json_extract(s.usage,'$.wall_ms'),0) > 0 \
                         OR COALESCE(json_extract(s.usage,'$.cost_usd'),0) > 0) THEN 0 \
                      ELSE COALESCE(a.reserved_usd,0) END),0.0) \
                      + (SELECT COALESCE(SUM(reserved_usd),0.0) FROM plan_runs WHERE outcome_id=?1 AND cost_usd IS NULL) \
             FROM attempts a JOIN work_items w ON w.id=a.work_item_id \
             JOIN sessions s ON s.id=a.session_id WHERE w.outcome_id=?1",
            [id.as_str()],
            |r| {
                Ok(OutcomeUsage {
                    cost_usd: r.get(0)?,
                    wall_ms: r.get::<_, i64>(1)?.max(0) as u64,
                    reserved_usd: r.get(2)?,
                    planner_usd,
                })
            },
        )?)
    }

    pub fn list_decisions(&self, id: &OutcomeId) -> Result<Vec<DecisionRecord>> {
        need_v5(&self.conn)?;
        let mut st = self.conn.prepare(&format!(
            "SELECT {DECISION_COLS} FROM decisions WHERE outcome_id=?1 ORDER BY created_at, id LIMIT {LIST_LIMIT}"
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
             WHERE decision_id=?1 ORDER BY rowid LIMIT 1000",
        )?;
        Ok(st
            .query_map([id.as_str()], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
            .collect::<rusqlite::Result<_>>()?)
    }

    /// The evidence rows of the outcome that count (chained event AND valid MAC, the frozen
    /// command's hash for a frozen check) under its current contract: what the verdict sees.
    pub fn authentic_evidence(&self, id: &OutcomeId) -> Result<Vec<AuthenticEvidence>> {
        need_v5(&self.conn)?;
        let o = self.get_outcome(id)?;
        let (contract, ..) = self.get_contract(id, o.contract_version)?;
        let Some(key) = crate::trust::existing_chain_key(&self.home) else {
            return Ok(vec![]);
        };
        authentic_evidence(&self.conn, &key, id, &contract)?
            .into_iter()
            .map(|r| {
                Ok(AuthenticEvidence {
                    work_item_id: WorkItemId::parse(&r.item).map_err(refuse)?,
                    check_id: r.fact.check_id,
                    subject_commit: r.fact.commit,
                    trust_class: r.fact.trust,
                    result: r.fact.result,
                    flags: r.flags,
                    fence_current: r.fact.fence_current,
                })
            })
            .collect()
    }

    pub fn list_evidence(&self, id: &OutcomeId) -> Result<Vec<EvidenceRecord>> {
        need_v5(&self.conn)?;
        let mut st = self.conn.prepare(&format!(
            "SELECT {EVIDENCE_COLS},{EVENT_BACKED} FROM evidence e WHERE outcome_id=?1 ORDER BY captured_at, id LIMIT {LIST_LIMIT}"
        ))?;
        Ok(st
            .query_map([id.as_str()], evidence_row)?
            .collect::<rusqlite::Result<_>>()?)
    }

    /// The newest `limit` evidence rows of the outcome, newest first (`list_evidence` is the oldest
    /// rows first and stops at its own limit).
    pub fn recent_evidence(&self, id: &OutcomeId, limit: usize) -> Result<Vec<EvidenceRecord>> {
        need_v5(&self.conn)?;
        let mut st = self.conn.prepare(&format!(
            "SELECT {EVIDENCE_COLS},{EVENT_BACKED} FROM evidence e WHERE outcome_id=?1 ORDER BY captured_at DESC, id DESC LIMIT {}",
            limit.min(LIST_LIMIT)
        ))?;
        Ok(st
            .query_map([id.as_str()], evidence_row)?
            .collect::<rusqlite::Result<_>>()?)
    }

    pub fn list_outbox(&self, id: &OutcomeId) -> Result<Vec<OutboxRecord>> {
        need_v5(&self.conn)?;
        let mut st = self.conn.prepare(&format!(
            "SELECT {OUTBOX_COLS} FROM outbox WHERE outcome_id=?1 ORDER BY rowid LIMIT {LIST_LIMIT}"
        ))?;
        Ok(st
            .query_map([id.as_str()], outbox_row)?
            .collect::<rusqlite::Result<_>>()?)
    }

    /// Keys are scoped to the outcome: another outcome's command of the same key is unrelated.
    pub fn get_command(
        &self,
        outcome: &OutcomeId,
        idempotency_key: &str,
    ) -> Result<Option<CommandRecord>> {
        need_v5(&self.conn)?;
        Ok(self
            .conn
            .query_row(
                &format!(
                    "SELECT {COMMAND_COLS} FROM outcome_commands WHERE outcome_id=?1 AND idempotency_key=?2"
                ),
                params![outcome.as_str(), idempotency_key],
                command_row,
            )
            .optional()?)
    }

    /// The contract of `version` with its approval checked: the stored hash is the hash of the
    /// stored body and the approval MAC (keyed with the per-user chain key) matches. A row whose
    /// `approved_at` was written by a bare `UPDATE` is not approved.
    pub fn contract_is_approved(&self, id: &OutcomeId, version: u32) -> Result<bool> {
        need_v5(&self.conn)?;
        let key = crate::trust::existing_chain_key(&self.home);
        contract_authentic(&self.conn, key.as_ref(), id, version)
    }

    /// The full action stored with a decision (`None` when it has none), only if it still hashes to
    /// the `action_hash` of the binding (and the binding to its hash): anything else is tampering.
    pub fn decision_action(&self, id: &DecisionId) -> Result<Option<Value>> {
        need_v5(&self.conn)?;
        let (action, binding, hash): (Option<String>, String, String) = self
            .conn
            .query_row(
                "SELECT action,binding,binding_hash FROM decisions WHERE id=?1",
                [id.as_str()],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?
            .ok_or_else(|| Error::NotFound(format!("decision {id}")))?;
        let tampered = |why: &str| Error::Tampered(format!("decision {id}: {why}"));
        if sha_hex(binding.as_bytes()) != hash {
            return Err(tampered("the binding does not match its hash"));
        }
        let Some(text) = action else {
            return Ok(None);
        };
        let b: Value = serde_json::from_str(&binding)?;
        if b.get("action_hash").and_then(Value::as_str) != Some(sha_hex(text.as_bytes()).as_str()) {
            return Err(tampered(
                "the stored action is not the one the binding names",
            ));
        }
        Ok(Some(serde_json::from_str(&text)?))
    }

    /// The grants of an outcome (all states), authentic ones only: a row whose MAC does not verify
    /// is not returned.
    pub fn list_grants(&self, id: &OutcomeId) -> Result<Vec<GrantRecord>> {
        need_v5(&self.conn)?;
        let Some(key) = crate::trust::existing_chain_key(&self.home) else {
            return Ok(vec![]);
        };
        let mut st = self.conn.prepare(
            "SELECT id,decision_id,outcome_id,work_item_id,rules,action_hash,expires_at,created_at,consumed_at,consumed_by_attempt,revoked_at,mac \
             FROM grants WHERE outcome_id=?1 ORDER BY created_at, id",
        )?;
        let rows = st.query_map([id.as_str()], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, String>(4)?,
                r.get::<_, String>(5)?,
                r.get::<_, String>(6)?,
                r.get::<_, String>(7)?,
                r.get::<_, Option<String>>(8)?,
                r.get::<_, Option<String>>(9)?,
                r.get::<_, Option<String>>(10)?,
                r.get::<_, String>(11)?,
            ))
        })?;
        let mut out = vec![];
        for row in rows {
            let (gid, dec, oc, item, rules, ah, exp, created, consumed, by, revoked, m) = row?;
            let parts = grant_parts(&gid, &dec, &oc, &item, &rules, &ah, &exp, &created);
            if !mac_ok(&key, "grant", &parts, &m) {
                continue;
            }
            out.push(GrantRecord {
                id: gid,
                decision_id: DecisionId::parse(&dec).map_err(refuse)?,
                work_item_id: WorkItemId::parse(&item).map_err(refuse)?,
                rules: serde_json::from_str(&rules)?,
                action_hash: ah,
                expires_at: exp,
                created_at: created,
                consumed_at: consumed,
                consumed_by_attempt: by,
                revoked_at: revoked,
            });
        }
        Ok(out)
    }

    /// The resolution of a decision, only if everything the approval authorised still holds: the
    /// MAC (over the decision, its outcome, kind, revision, binding hash, expiry, brief hash,
    /// approver, choice and time) verifies, the binding hash is the hash of the stored binding,
    /// the brief hash is the stored brief's, and the state the row shows is the one the MAC'd
    /// choice leads to (`reject` is rejected, `request_evidence`/`edit_scope` need more info, an
    /// offered alternative is approved). `None` for an open decision and for anything that does
    /// not add up.
    pub fn verified_resolution(&self, id: &DecisionId) -> Result<Option<Resolved>> {
        need_v5(&self.conn)?;
        let (d, outcome, binding_text) = self
            .conn
            .query_row(
                &format!("SELECT {DECISION_COLS}, outcome_id, binding FROM decisions WHERE id=?1"),
                [id.as_str()],
                |r| {
                    Ok((
                        decision_row(r)?,
                        r.get::<_, String>(10)?,
                        r.get::<_, String>(11)?,
                    ))
                },
            )
            .optional()?
            .ok_or_else(|| Error::NotFound(format!("decision {id}")))?;
        if !matches!(
            d.row_state,
            DecisionState::Approved | DecisionState::Rejected | DecisionState::NeedsMoreInfo
        ) {
            return Ok(None);
        }
        let tampered = |why: &str| Error::Tampered(format!("decision {id}: {why}"));
        if sha_hex(binding_text.as_bytes()) != d.binding_hash {
            return Err(tampered("the binding does not match its hash"));
        }
        let Some(key) = crate::trust::existing_chain_key(&self.home) else {
            return Err(tampered("a resolved state but the chain key is missing"));
        };
        let row: Option<(String, String, String, String, String)> = self
            .conn
            .query_row(
                "SELECT approver,choice,brief_sha256,decided_at,mac FROM decision_approvals \
                 WHERE decision_id=?1 AND decision_revision=?2",
                params![id.as_str(), d.revision - 1],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
            )
            .optional()?;
        let Some((approver, choice, brief, at, stored)) = row else {
            return Err(tampered("a resolved state without its approval"));
        };
        let facts = ApprovalFacts {
            decision: id.as_str(),
            outcome: &outcome,
            kind: &d.kind,
            revision: d.revision - 1,
            binding_hash: &d.binding_hash,
            expires_at: &d.expires_at,
            brief_sha256: &brief,
            approver: &approver,
            choice: &choice,
            at: &at,
        };
        if !approval_mac_ok(&key, &facts, &stored) {
            return Err(tampered("the approval does not verify"));
        }
        if brief != d.brief_sha256 {
            return Err(tampered("the brief is not the one that was approved"));
        }
        let derived = match choice.as_str() {
            "reject" => DecisionState::Rejected,
            "request_evidence" | "edit_scope" => DecisionState::NeedsMoreInfo,
            c if offers(&d.brief, c) => DecisionState::Approved,
            _ => return Err(tampered("the approved choice is not one the brief offered")),
        };
        if derived != d.row_state {
            return Err(tampered(
                "the state does not follow from the approved choice",
            ));
        }
        Ok(Some(Resolved {
            state: derived,
            approver,
            choice,
        }))
    }

    /// The authoritative state of a decision. A resolved state (approved, rejected, needs more
    /// info) is returned only if [`Self::verified_resolution`] vouches for it; a row that shows one
    /// without a valid approval is an error, never a quiet `Approved`. `DecisionRecord::row_state`
    /// is the raw column and must not be used to authorise anything.
    pub fn decision_state(&self, id: &DecisionId) -> Result<DecisionState> {
        need_v5(&self.conn)?;
        let row_state: DecisionState = enum_from(&self.conn.query_row(
            "SELECT state FROM decisions WHERE id=?1",
            [id.as_str()],
            |r| r.get::<_, String>(0),
        )?)?;
        if matches!(
            row_state,
            DecisionState::Approved | DecisionState::Rejected | DecisionState::NeedsMoreInfo
        ) {
            // a concurrent move out of the resolved state between the two reads is not tampering: report what
            // the row says now
            return Ok(self.verified_resolution(id)?.map_or(row_state, |r| r.state));
        }
        Ok(row_state)
    }

    /// Commands waiting for an owner, oldest first.
    pub fn queued_commands(&self, id: &OutcomeId) -> Result<Vec<CommandRecord>> {
        need_v5(&self.conn)?;
        let mut st = self.conn.prepare(&format!(
            "SELECT {COMMAND_COLS} FROM outcome_commands WHERE outcome_id=?1 AND state='queued' \
             ORDER BY created_at, rowid LIMIT {LIST_LIMIT}"
        ))?;
        Ok(st
            .query_map([id.as_str()], command_row)?
            .collect::<rusqlite::Result<_>>()?)
    }
}

/// A one-shot permission for a new attempt of one work item, derived from an approved decision's
/// stored action.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GrantRecord {
    pub id: String,
    pub decision_id: DecisionId,
    pub work_item_id: WorkItemId,
    /// Allow rules (`Edit(/outside.txt)`, `Bash(git status)`), exactly those the stored action carries.
    pub rules: Vec<String>,
    pub action_hash: String,
    pub expires_at: String,
    pub created_at: String,
    pub consumed_at: Option<String>,
    pub consumed_by_attempt: Option<String>,
    pub revoked_at: Option<String>,
}

/// How many times evidence may be requested for one decision (each re-brief is a new card); after
/// that the person answers or rejects.
pub const MAX_REBRIEFS: u64 = 3;

fn rebriefs_of(brief: &Value) -> u64 {
    brief.get("rebrief").and_then(Value::as_u64).unwrap_or(0)
}

/// A rule a grant may carry: an exact `Edit(/path)` or `Bash(command)` of printable ASCII, no `*`
/// (the engine reads `*` and a trailing `:*` in a Bash rule as a glob or a prefix, so a command
/// containing one would grant more than that command), no glob characters in an edit path.
pub fn valid_grant_rule(r: &str) -> bool {
    let edit = r.strip_prefix("Edit(/");
    let bash = r.strip_prefix("Bash(");
    let body = edit.or(bash).and_then(|b| b.strip_suffix(')'));
    r.len() <= 300
        && r.chars().all(|c| matches!(c, ' '..='~'))
        && !r.contains('*')
        && body.is_some_and(|b| {
            !b.is_empty()
                && b == b.trim()
                && !b.contains(['(', ')'])
                && (bash.is_some() || !b.contains(['?', '[', ']', '{', '}', '!']))
        })
}

#[allow(clippy::too_many_arguments)]
fn grant_parts<'a>(
    id: &'a str,
    decision: &'a str,
    outcome: &'a str,
    item: &'a str,
    rules: &'a str,
    action_hash: &'a str,
    expires_at: &'a str,
    created_at: &'a str,
) -> [&'a str; 8] {
    [
        id,
        decision,
        outcome,
        item,
        rules,
        action_hash,
        expires_at,
        created_at,
    ]
}

/// A decision resolution whose approval row was verified.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resolved {
    pub state: DecisionState,
    pub approver: String,
    pub choice: String,
}

/// A MAC over length-prefixed parts (domain included), so no two different tuples share an input.
/// The key is a subkey of the per-user chain key (`HMAC(chain key, "synt outcome mac v1")`), so a
/// MAC never doubles as a chain link. The key is stored next to the data: the MAC detects
/// corruption, old binaries and writers that cannot read the key (the sandbox denies the key file
/// to attempts, PR #117); it does not stop a program running as the same user outside the sandbox.
fn mac_state(key: &[u8; 32], domain: &str, parts: &[&str]) -> Hmac<Sha256> {
    let mut sub = Hmac::<Sha256>::new_from_slice(key).expect("any key length");
    sub.update(b"synt outcome mac v1");
    let sub = sub.finalize().into_bytes();
    let mut m = Hmac::<Sha256>::new_from_slice(&sub).expect("any key length");
    for p in std::iter::once(&domain).chain(parts) {
        m.update(&(p.len() as u64).to_le_bytes());
        m.update(p.as_bytes());
    }
    m
}

fn mac(key: &[u8; 32], domain: &str, parts: &[&str]) -> String {
    mac_state(key, domain, parts)
        .finalize()
        .into_bytes()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// Constant-time check of a stored hex MAC.
fn mac_ok(key: &[u8; 32], domain: &str, parts: &[&str], stored_hex: &str) -> bool {
    let bytes: Option<Vec<u8>> = (stored_hex.len().is_multiple_of(2) && stored_hex.is_ascii())
        .then(|| {
            (0..stored_hex.len())
                .step_by(2)
                .map(|i| u8::from_str_radix(&stored_hex[i..i + 2], 16).ok())
                .collect()
        })
        .flatten();
    bytes.is_some_and(|b| mac_state(key, domain, parts).verify_slice(&b).is_ok())
}

/// What a contract approval authorises: the outcome, the version, the hash of the whole stored
/// body (goal, criteria, envelope and budgets are all in it), who approved and when.
fn contract_parts<'a>(
    outcome: &'a str,
    version: &'a str,
    hash: &'a str,
    approver: &'a str,
    at: &'a str,
) -> [&'a str; 5] {
    [outcome, version, hash, approver, at]
}

fn contract_mac(
    key: &[u8; 32],
    outcome: &str,
    version: u32,
    hash: &str,
    approver: &str,
    at: &str,
) -> String {
    let v = version.to_string();
    mac(
        key,
        "contract",
        &contract_parts(outcome, &v, hash, approver, at),
    )
}

/// What a decision approval authorises: the decision and the outcome it belongs to, its kind, the
/// revision answered, the binding (by hash), the expiry, the brief shown (by hash), who answered
/// what, and when.
struct ApprovalFacts<'a> {
    decision: &'a str,
    outcome: &'a str,
    kind: &'a str,
    revision: i64,
    binding_hash: &'a str,
    expires_at: &'a str,
    brief_sha256: &'a str,
    approver: &'a str,
    choice: &'a str,
    at: &'a str,
}

impl ApprovalFacts<'_> {
    fn parts(&self) -> Vec<String> {
        [
            self.decision,
            self.outcome,
            self.kind,
            &self.revision.to_string(),
            self.binding_hash,
            self.expires_at,
            self.brief_sha256,
            self.approver,
            self.choice,
            self.at,
        ]
        .iter()
        .map(|p| (*p).to_string())
        .collect()
    }
}

fn approval_mac(key: &[u8; 32], f: &ApprovalFacts<'_>) -> String {
    let p = f.parts();
    mac(
        key,
        "approval",
        &p.iter().map(String::as_str).collect::<Vec<_>>(),
    )
}

fn approval_mac_ok(key: &[u8; 32], f: &ApprovalFacts<'_>, stored: &str) -> bool {
    let p = f.parts();
    mac_ok(
        key,
        "approval",
        &p.iter().map(String::as_str).collect::<Vec<_>>(),
        stored,
    )
}

type ContractApprovalRow = (
    String,
    String,
    Option<String>,
    Option<String>,
    Option<String>,
);

/// Shared by [`Reader::contract_is_approved`] and the check before `Ready`. `Ok(false)`: no
/// approval is recorded. `Ok(true)`: it verifies. An approval that is recorded but does not
/// verify (no key, wrong MAC, a body that no longer matches its hash) is [`Error::Tampered`].
fn contract_authentic(
    conn: &Connection,
    key: Option<&[u8; 32]>,
    id: &OutcomeId,
    version: u32,
) -> Result<bool> {
    let row: Option<ContractApprovalRow> = conn
        .query_row(
            "SELECT body,hash,approved_by,approved_at,approved_mac FROM contracts \
             WHERE outcome_id=?1 AND version=?2",
            params![id.as_str(), version],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
        )
        .optional()?;
    let Some((body, hash, by, at, m)) = row else {
        return Ok(false);
    };
    if by.is_none() && at.is_none() && m.is_none() {
        return Ok(false);
    }
    let tampered = || {
        Error::Tampered(format!(
            "the approval of contract v{version} does not verify"
        ))
    };
    let (Some(by), Some(at), Some(m), Some(key)) = (by, at, m, key) else {
        return Err(tampered());
    };
    let body_ok = serde_json::from_str::<Value>(&body)
        .map(|v| body_hash(&v) == hash)
        .unwrap_or(false);
    let v = version.to_string();
    if body_ok
        && mac_ok(
            key,
            "contract",
            &contract_parts(id.as_str(), &v, &hash, &by, &at),
            &m,
        )
    {
        Ok(true)
    } else {
        Err(tampered())
    }
}

fn parse_ts(s: &str) -> Option<OffsetDateTime> {
    let fmt = time::macros::format_description!(
        "[year]-[month]-[day]T[hour]:[minute]:[second].[subsecond digits:3]Z"
    );
    time::PrimitiveDateTime::parse(s, &fmt)
        .ok()
        .map(time::PrimitiveDateTime::assume_utc)
}

/// Is `c` one of the alternatives the brief offers (a string, or an object with that `id`)?
fn offers(brief: &Value, c: &str) -> bool {
    brief
        .get("alternatives")
        .and_then(Value::as_array)
        .is_some_and(|a| {
            a.iter()
                .any(|x| x.as_str() == Some(c) || x.get("id").and_then(Value::as_str) == Some(c))
        })
}

fn is_sha256(s: &str) -> bool {
    s.len() == 64
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

// ------------------------------------------------------------------- policy

/// What the settings allow (`synt.orchestrator.*`, user and managed layers already merged, managed
/// winning). Required to create or open an outcome, so the store itself refuses when the
/// feature is off and never accepts a contract above the ceilings.
///
/// The policy is **caller-built**: the store trusts what it is handed, so the gate is exactly as
/// strong as the code that builds it. `#[non_exhaustive]` stops a struct literal outside this
/// crate; the one production builder is `synthesis_app::outcome_policy`, which reads the merged
/// settings (managed layer last, fail-closed on an unusable managed value).
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct OutcomePolicy {
    pub enabled: bool,
    pub max_parallel_workers: u32,
    pub max_attempts_per_work_item: u32,
    /// Longest a decision may stay open, in hours.
    pub decision_expiry_hours: u32,
    /// `None`: no ceiling on the outcome cap (the contract still has to name one).
    pub max_cost_usd: Option<f64>,
    /// A ceiling makes a per-attempt cap mandatory in the contract.
    pub attempt_max_cost_usd: Option<f64>,
    pub attempt_max_wall_secs: Option<u64>,
}

impl OutcomePolicy {
    /// A policy with no cost ceilings; set the optional fields on the value.
    pub fn new(
        enabled: bool,
        max_parallel_workers: u32,
        max_attempts_per_work_item: u32,
        decision_expiry_hours: u32,
    ) -> Self {
        Self {
            enabled,
            max_parallel_workers,
            max_attempts_per_work_item,
            decision_expiry_hours,
            max_cost_usd: None,
            attempt_max_cost_usd: None,
            attempt_max_wall_secs: None,
        }
    }

    /// Refuses a contract whose budgets exceed the ceilings (never silently clamps: what the
    /// person approves is what runs).
    pub fn check(&self, c: &Contract) -> Result<()> {
        let b = &c.envelope.budgets;
        let over =
            |what: &str| Error::Invalid(format!("contract {what} exceeds the settings ceiling"));
        if b.max_parallel_workers > self.max_parallel_workers {
            return Err(over("max_parallel_workers"));
        }
        if b.max_attempts_per_work_item > self.max_attempts_per_work_item {
            return Err(over("max_attempts_per_work_item"));
        }
        if self.max_cost_usd.is_some_and(|m| b.max_cost_usd > m) {
            return Err(over("max_cost_usd"));
        }
        if let Some(m) = self.attempt_max_cost_usd
            && !b.attempt_max_cost_usd.is_some_and(|c| c <= m)
        {
            return Err(over("attempt_max_cost_usd"));
        }
        if let Some(m) = self.attempt_max_wall_secs
            && !b.attempt_max_wall_secs.is_some_and(|w| w <= m)
        {
            return Err(over("attempt_max_wall_secs"));
        }
        Ok(())
    }
}

// ------------------------------------------------------------------- commands

#[derive(Debug, Clone)]
pub struct NewCommand {
    pub outcome_id: OutcomeId,
    /// The caller's retry key, scoped to the outcome: the same key returns the stored result
    /// and has no second effect.
    pub idempotency_key: String,
    /// `decision.resolve`, `outcome.start`, ...
    pub kind: String,
    pub params: Value,
    pub expected_revision: Option<i64>,
    /// Self-asserted until the orchestrator endpoint authenticates its clients (plan PR-12):
    /// the store records it, it is not proof of who sent the command.
    pub from_surface: Surface,
}

fn valid_command_text(s: &str, max: usize) -> bool {
    !s.is_empty() && s.len() <= max && s.bytes().all(|b| b.is_ascii_graphic())
}

/// What a person's answer to a decision carries (ADR-019 section 4): every field is required, so
/// a surface cannot resolve something it did not show.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resolution {
    pub decision_id: DecisionId,
    /// The decision revision the person saw.
    pub decision_revision: i64,
    pub approver: String,
    pub choice: String,
    /// Hash of the brief as shown ([`DecisionRecord::brief_sha256`]).
    pub brief_sha256: String,
}

impl Resolution {
    fn from_parts(
        params: &Value,
        expected_revision: Option<i64>,
        approver: String,
    ) -> Result<Self> {
        let text = |k: &str| params.get(k).and_then(Value::as_str);
        let bad = |what: &str| Error::Invalid(format!("decision.resolve needs {what}"));
        let decision_id = text("decision_id")
            .and_then(|s| DecisionId::parse(s).ok())
            .ok_or_else(|| bad("a decision_id"))?;
        let choice = text("choice")
            .filter(|c| valid_command_text(c, 128))
            .ok_or_else(|| bad("a choice (there is no default)"))?;
        let brief_sha256 = text("brief_sha256")
            .filter(|h| is_sha256(h))
            .ok_or_else(|| bad("the brief_sha256 of the brief that was shown"))?;
        let decision_revision = expected_revision
            .filter(|r| *r >= 1)
            .ok_or_else(|| bad("an expected_revision"))?;
        Ok(Self {
            decision_id,
            decision_revision,
            approver,
            choice: choice.to_string(),
            brief_sha256: brief_sha256.to_string(),
        })
    }

    /// The approver is derived from the command's surface, not taken from its params.
    pub fn from_command(c: &CommandRecord) -> Result<Self> {
        Self::from_parts(
            &c.params,
            c.expected_revision,
            format!("user:{}", enum_str(&c.from_surface)),
        )
    }
}

impl Store {
    /// The only write a surface makes (ADR-016 item 11): a plain insert, safe under WAL while an
    /// owner runs or while none does. Retrying with the same key returns the stored row; reusing a
    /// key for a different request is refused. Bounded: params at most 64 KiB, at most 1000
    /// commands waiting per outcome, and a `decision.resolve` must carry everything the resolution
    /// needs.
    pub fn enqueue_command(&self, c: NewCommand) -> Result<CommandRecord> {
        if !valid_command_text(&c.idempotency_key, 128) || !valid_command_text(&c.kind, 64) {
            return Err(Error::Invalid(
                "command key and kind must be 1 to 128 and 1 to 64 printable characters".into(),
            ));
        }
        let mut params_json = c.params.clone();
        // the store never keeps a secret a surface pasted into a command
        self.redact_value(&mut params_json);
        let text = params_json.to_string();
        if text.len() > MAX_COMMAND_PARAMS_BYTES {
            return Err(Error::TooLarge {
                what: "command params",
                size: text.len(),
                max: MAX_COMMAND_PARAMS_BYTES,
            });
        }
        if c.kind == "decision.resolve" {
            Resolution::from_parts(&params_json, c.expected_revision, String::new())?;
        }
        let conn = connect(self.home(), false)?;
        need_v5(&conn)?;
        let reader = self.reader()?;
        // One statement: the quota is counted by the same write that inserts, so concurrent
        // surfaces cannot slip past it. A retry of a stored key conflicts and inserts nothing.
        conn.execute(
            "INSERT INTO outcome_commands(id,outcome_id,idempotency_key,kind,params,expected_revision,from_surface,state,created_at) \
             SELECT ?1,?2,?3,?4,?5,?6,?7,'queued',?8 \
             WHERE (SELECT COUNT(*) FROM outcome_commands WHERE outcome_id=?2 AND state='queued') < ?9 \
             ON CONFLICT(outcome_id,idempotency_key) DO NOTHING",
            params![
                new_uuid(),
                c.outcome_id.as_str(),
                c.idempotency_key,
                c.kind,
                text,
                c.expected_revision,
                enum_str(&c.from_surface),
                now(),
                MAX_QUEUED_COMMANDS,
            ],
        )?;
        let Some(rec) = reader.get_command(&c.outcome_id, &c.idempotency_key)? else {
            return Err(Error::QuotaExceeded {
                what: "queued commands",
                limit: MAX_QUEUED_COMMANDS as u64,
            });
        };

        if rec.kind != c.kind
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
    /// Validated here against the contract rules and the policy ceilings; the stored body is
    /// what a person approves.
    pub contract: &'a Contract,
    pub policy: &'a OutcomePolicy,
}

/// `sha256` of the stored contract body (redacted JSON, fields in declaration order). Approval
/// records this hash, and a stored contract can never change (trigger in the v5 migration).
fn body_hash(body: &Value) -> String {
    sha_hex(body.to_string().as_bytes())
}

impl Store {
    /// Creates the outcome, its hidden control session (`kind = 'outcome'`, written with its kind
    /// in one transaction) and contract v1 with the `outcome.created` event, and returns the
    /// writer that owns the control-session lock. Refused when the settings turn outcomes off.
    pub fn create_outcome(&self, new: NewOutcome<'_>) -> Result<OutcomeWriter> {
        if !new.policy.enabled {
            return Err(Error::OutcomesDisabled);
        }
        need_v5(&connect(self.home(), true)?)?;
        validate_contract(new.contract).map_err(refuse)?;
        new.policy.check(new.contract)?;
        // a read-only goal has no base commit and never a git requirement; a write goal always has one
        if (new.contract.kind == ContractKind::ReadOnly) != new.base_commit.is_empty() {
            return Err(Error::Invalid(
                "a write goal needs a base commit, and a read-only goal has none".into(),
            ));
        }
        let id = OutcomeId::parse(&new_uuid()).map_err(refuse)?;
        // no Node transcript mirror: a control session holds events only
        let mut w = self.create_kind(
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
            Some("outcome"),
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
                params![
                    id.as_str(),
                    new.contract.version,
                    body_hash(&body),
                    body.to_string()
                ],
            )?;
            et.append(
                ev::OUTCOME_CREATED,
                json!({"outcome_id": id, "contract_version": new.contract.version,
                       "contract_hash": body_hash(&body), "revision": 1}),
            )?;
            Ok(())
        })?;
        Ok(OutcomeWriter::new(w, id, new.policy.clone()))
    }

    /// Takes the control-session lock of an existing outcome. [`Error::Locked`] means another
    /// process owns it: talk to that owner (or queue a command) instead. Refused when the
    /// settings turn outcomes off (a managed `enabled = false` stops a running install too).
    pub fn open_outcome(&self, id: &OutcomeId, policy: &OutcomePolicy) -> Result<OutcomeWriter> {
        if !policy.enabled {
            return Err(Error::OutcomesDisabled);
        }
        let rec = self.reader()?.get_outcome(id)?;
        let w = self.open_locked(&rec.control_session_id)?;
        Ok(OutcomeWriter::new(w, id.clone(), policy.clone()))
    }
}

// --------------------------------------------------------------------- writer

/// The single writer of one outcome: holds its control session's OS lock for its whole life.
pub struct OutcomeWriter {
    w: SessionWriter,
    id: OutcomeId,
    policy: OutcomePolicy,
    clock: fn() -> OffsetDateTime,
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

/// The result of a transition: the new revision, and per outbox row of the effects whether it
/// was newly inserted (`false`: the same effect was already queued under that key).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Applied {
    pub revision: i64,
    pub outbox_new: Vec<bool>,
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
    /// One open decision per `(key, binding)` (ADR-016 item 4): a second insert returns the first.
    pub dedupe_key: String,
    /// Alternatives are `alternatives`: strings or objects with an `id`. No `selected`,
    /// `recommended` or default field is accepted.
    pub brief: Value,
    /// Contract and plan versions, subject commit, policy hash, action hash, attempt ordinal.
    pub binding: Value,
    /// A store-format timestamp in the future, within `decision_expiry_hours` of the settings.
    pub expires_at: String,
    /// The FULL action the decision is about (what a grant is derived from; the brief only shows a
    /// shortened view). The store hashes it into `binding.action_hash` itself (a caller-supplied
    /// `action_hash` is overwritten) and keeps it immutable.
    pub action: Option<Value>,
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
    /// `(session_id, artifact_id, sha256)` of a stored artifact of this outcome; checked
    /// against the `artifacts` table.
    pub artifact: Option<(String, String, String)>,
    /// The fence of the attempt that produced it; an older one is rejected (ADR-017).
    pub fence: i64,
    /// Disqualifying flags, part of the row from the start (and of its MAC): a flag can neither
    /// be added nor removed later.
    pub flags: Vec<String>,
}

/// Flags a row may carry; each one keeps it from satisfying a criterion.
pub const EVIDENCE_FLAGS: [&str; 4] = [
    "definitions_changed",
    "tests_weakened",
    "outside_envelope",
    "mode_changed",
];

impl OutcomeWriter {
    fn new(w: SessionWriter, id: OutcomeId, policy: OutcomePolicy) -> Self {
        Self {
            w,
            id,
            policy,
            clock: OffsetDateTime::now_utc,
        }
    }

    /// Replaces the clock (expiry and binding checks); for tests.
    #[must_use]
    pub fn with_clock(mut self, clock: fn() -> OffsetDateTime) -> Self {
        self.clock = clock;
        self
    }

    pub fn id(&self) -> &OutcomeId {
        &self.id
    }

    /// The settings' limit on how long a decision may stay open.
    pub fn decision_expiry_hours(&self) -> u32 {
        self.policy.decision_expiry_hours
    }

    /// Is `session_id` the session of one of this outcome's attempts?
    pub(crate) fn owns_attempt_session(&self, session_id: &str) -> Result<bool> {
        Ok(self.w.conn.query_row(
            "SELECT EXISTS (SELECT 1 FROM attempts a JOIN work_items w ON w.id=a.work_item_id \
             WHERE a.session_id=?1 AND w.outcome_id=?2)",
            params![session_id, self.id.as_str()],
            |r| r.get(0),
        )?)
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
        let (id, policy, now) = (&self.id, &self.policy, (self.clock)());
        self.w.event_tx(|et| {
            f(&mut OutcomeTx {
                et,
                id,
                policy,
                now,
            })
        })
    }

    pub fn approve_contract(&mut self, version: u32, hash: &str, approver: &str) -> Result<()> {
        self.atomically(|tx| tx.approve_contract(version, hash, approver))
    }

    pub fn new_contract_version(
        &mut self,
        expected_revision: i64,
        contract: &Contract,
    ) -> Result<()> {
        self.atomically(|tx| tx.new_contract_version(expected_revision, contract))
    }

    pub fn insert_plan(&mut self, plan: &Plan) -> Result<Vec<(String, WorkItemId)>> {
        self.atomically(|tx| tx.insert_plan(plan))
    }

    pub fn transition_outcome(
        &mut self,
        expected_revision: i64,
        to: OutcomeState,
        reason: Option<&str>,
        fx: &Effects,
    ) -> Result<Applied> {
        self.atomically(|tx| tx.transition_outcome(expected_revision, to, reason, fx))
    }

    pub fn transition_work_item(
        &mut self,
        id: &WorkItemId,
        expected_revision: i64,
        to: WorkItemState,
        fx: &Effects,
    ) -> Result<Applied> {
        self.atomically(|tx| tx.transition_work_item(id, expected_revision, to, fx))
    }

    pub fn new_attempt(
        &mut self,
        item: &WorkItemId,
        ordinal: u32,
        session_id: &str,
        worker_kind: &str,
        fx: &Effects,
    ) -> Result<(AttemptRecord, Vec<bool>)> {
        self.atomically(|tx| tx.new_attempt(item, ordinal, session_id, worker_kind, fx))
    }

    pub fn transition_attempt(
        &mut self,
        id: &AttemptId,
        expected_revision: i64,
        to: AttemptState,
        end: &AttemptEnd,
        fx: &Effects,
    ) -> Result<Applied> {
        self.atomically(|tx| tx.transition_attempt(id, expected_revision, to, end, fx))
    }

    pub fn set_attempt_retained(&mut self, id: &AttemptId, reason: &str) -> Result<()> {
        self.atomically(|tx| tx.set_attempt_retained(id, reason))
    }

    pub fn record_worker(
        &mut self,
        id: &AttemptId,
        fence: i64,
        pid: u32,
        start: Option<&str>,
    ) -> Result<()> {
        self.atomically(|tx| tx.record_worker(id, fence, pid, start))
    }

    pub fn heartbeat(&mut self, id: &AttemptId, fence: i64) -> Result<()> {
        self.atomically(|tx| tx.heartbeat(id, fence))
    }

    pub fn reserve_budget(&mut self, id: &AttemptId, fence: i64, usd: f64) -> Result<()> {
        self.atomically(|tx| tx.reserve_budget(id, fence, usd))
    }

    pub fn release_budget(&mut self, id: &AttemptId, fence: i64) -> Result<()> {
        self.atomically(|tx| tx.release_budget(id, fence))
    }

    pub fn bind_sandbox(
        &mut self,
        id: &AttemptId,
        fence: i64,
        backend: &str,
        guarantee: &str,
    ) -> Result<()> {
        self.atomically(|tx| tx.bind_sandbox(id, fence, backend, guarantee))
    }

    pub fn bind_policy(
        &mut self,
        id: &AttemptId,
        fence: i64,
        contract_version: u32,
        policy_hash: &str,
    ) -> Result<()> {
        self.atomically(|tx| tx.bind_policy(id, fence, contract_version, policy_hash))
    }

    pub fn new_decision(&mut self, d: &NewDecision) -> Result<(DecisionRecord, bool)> {
        self.atomically(|tx| tx.new_decision(d))
    }

    /// Is `current` still the binding the decision was made with (compared as the store compares
    /// it: redacted, hashed)?
    pub fn binding_is_current(&mut self, d: &DecisionRecord, current: &Value) -> Result<bool> {
        self.atomically(|tx| Ok(tx.binding_current(d, current)))
    }

    /// Turns an APPROVED `grant_once` decision into a one-shot grant (ADR-019 section 4). The
    /// caller names the decision and nothing else: the rules come from the action stored with
    /// the decision (checked against the binding's action hash), the approval must verify (MAC),
    /// the decision must be of kind `approval_needed`, and `current_binding` must still be the
    /// binding the person saw (contract, plan, commit, policy, attempt). A grant never outlives
    /// `decision_expiry_hours` of the settings.
    pub fn create_grant(
        &mut self,
        decision: &DecisionId,
        current_binding: &Value,
        ttl_hours: u32,
    ) -> Result<GrantRecord> {
        let d = self
            .list_decisions(self.id())?
            .into_iter()
            .find(|x| x.id == *decision)
            .ok_or_else(|| Error::NotFound(format!("decision {decision}")))?;
        if d.kind != "approval_needed" {
            return Err(Error::Invalid(
                "only an approval_needed decision can grant a permission".into(),
            ));
        }
        match self.verified_resolution(decision)? {
            Some(r) if r.state == DecisionState::Approved && r.choice == "grant_once" => {}
            _ => {
                return Err(Error::Invalid(
                    "only a decision a person approved with grant_once can grant".into(),
                ));
            }
        }
        let action = self
            .decision_action(decision)?
            .ok_or_else(|| Error::Invalid("the decision has no stored action".into()))?;
        let rules: Vec<String> = action
            .get("grant_rules")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(|r| r.as_str().map(str::to_owned))
                    .collect()
            })
            .unwrap_or_default();
        let listed = action
            .get("grant_rules")
            .and_then(Value::as_array)
            .map_or(0, Vec::len);
        if rules.is_empty()
            || rules.len() > 10
            || rules.len() != listed
            || rules.iter().any(|r| !valid_grant_rule(r))
        {
            return Err(Error::Invalid(
                "the stored action carries no grantable rules".into(),
            ));
        }
        let item = d
            .binding
            .get("work_item_id")
            .and_then(Value::as_str)
            .and_then(|s| WorkItemId::parse(s).ok())
            .ok_or_else(|| Error::Invalid("the decision binding names no work item".into()))?;
        let ttl = ttl_hours.clamp(1, self.policy.decision_expiry_hours.max(1));
        self.atomically(|tx| tx.insert_grant(&d, current_binding, &item, &rules, ttl))
    }

    /// The grants a new attempt of `item` may use now: authentic, not consumed, not revoked, not
    /// expired, and the decision behind them still verifies as approved.
    pub fn usable_grants(&self, item: &WorkItemId) -> Result<Vec<GrantRecord>> {
        let now = (self.clock)();
        let mut out = vec![];
        for g in self.list_grants(self.id())? {
            let live = g.work_item_id == *item
                && g.consumed_at.is_none()
                && g.revoked_at.is_none()
                && parse_ts(&g.expires_at).is_some_and(|t| t > now);
            if live
                && self
                    .verified_resolution(&g.decision_id)?
                    .is_some_and(|r| r.state == DecisionState::Approved && r.choice == "grant_once")
            {
                out.push(g);
            }
        }
        Ok(out)
    }

    /// Uses a grant for the attempt that is starting: once. Refused when it is consumed, revoked,
    /// expired, not for this attempt's work item, the attempt's fence is stale, the decision no
    /// longer verifies as approved, or `current_binding` is no longer the one the person saw.
    pub fn consume_grant(
        &mut self,
        grant_id: &str,
        attempt: &AttemptId,
        fence: i64,
        current_binding: &Value,
    ) -> Result<GrantRecord> {
        let g = self
            .usable_grants_any(grant_id)?
            .ok_or_else(|| Error::Invalid("the grant is not usable".into()))?;
        let d = self
            .list_decisions(self.id())?
            .into_iter()
            .find(|x| x.id == g.decision_id)
            .ok_or_else(|| Error::NotFound(format!("decision {}", g.decision_id)))?;
        self.atomically(|tx| {
            let a = tx.live_attempt(attempt, fence)?;
            if a.work_item_id != g.work_item_id {
                return Err(Error::Invalid(
                    "the grant is for another work item".into(),
                ));
            }
            if !tx.binding_current(&d, current_binding) {
                return Err(Error::Stale(
                    "the contract, plan, commit or policy changed since the decision was shown"
                        .into(),
                ));
            }
            let at = ts(tx.now);
            let n = tx.et.tx.execute(
                "UPDATE grants SET consumed_at=?2, consumed_by_attempt=?3 WHERE id=?1 AND consumed_at IS NULL AND revoked_at IS NULL AND expires_at > ?2",
                params![g.id, at, attempt.as_str()],
            )?;
            if n != 1 {
                return Err(Error::Invalid("the grant was already used".into()));
            }
            let payload = json!({"grant_id": g.id, "decision_id": g.decision_id,
                                 "attempt_id": attempt, "rules": g.rules, "action_hash": g.action_hash});
            tx.et.append(ev::GRANT_CONSUMED, payload.clone())?;
            tx.et.append(
                ev::POLICY_DECISION,
                json!({"action": "grant.consume", "result": "allow", "grant": payload}),
            )?;
            Ok(())
        })?;
        Ok(g)
    }

    /// An authentic, unconsumed, unrevoked, unexpired grant by id with a verified approval behind it.
    fn usable_grants_any(&self, grant_id: &str) -> Result<Option<GrantRecord>> {
        let mut found = None;
        for g in self
            .list_grants(self.id())?
            .into_iter()
            .filter(|g| g.id == grant_id)
        {
            found = self
                .usable_grants(&g.work_item_id)?
                .into_iter()
                .find(|x| x.id == grant_id);
        }
        Ok(found)
    }

    /// Withdraws an unused grant (the binding changed, the contract denies it now, a person said
    /// so). A consumed grant cannot be revoked: it was used.
    pub fn revoke_grant(&mut self, grant_id: &str, reason: &str) -> Result<()> {
        let reason: String = reason
            .chars()
            .filter(|c| matches!(c, ' '..='~'))
            .take(200)
            .collect();
        self.atomically(|tx| {
            let at = ts(tx.now);
            let n = tx.et.tx.execute(
                "UPDATE grants SET revoked_at=?2, revoked_reason=?3 WHERE id=?1 AND outcome_id=?4 \
                   AND consumed_at IS NULL AND revoked_at IS NULL",
                params![grant_id, at, reason, tx.id.as_str()],
            )?;
            if n != 1 {
                return Err(Error::Invalid(
                    "no unused grant with that id to revoke".into(),
                ));
            }
            let payload = json!({"grant_id": grant_id, "reason": reason});
            tx.et.append(ev::GRANT_REVOKED, payload.clone())?;
            tx.et.append(
                ev::POLICY_DECISION,
                json!({"action": "grant.revoke", "result": "deny", "grant": payload}),
            )?;
            Ok(())
        })
    }

    /// Records a refusal or another authorization in the chained audit trail (`policy.decision`).
    pub fn audit_policy(&mut self, action: &str, result: &str, detail: Value) -> Result<()> {
        let (action, result) = (action.to_owned(), result.to_owned());
        self.atomically(|tx| {
            tx.et.append(
                ev::POLICY_DECISION,
                json!({"action": action, "result": result, "detail": detail}),
            )?;
            Ok(())
        })
    }

    /// The orchestrator's own decision transitions (prepare, expire, invalidate). A person's
    /// answer is [`Self::resolve_decision`].
    pub fn transition_decision(
        &mut self,
        id: &DecisionId,
        expected_revision: i64,
        to: DecisionState,
        current_binding: &Value,
        fx: &Effects,
    ) -> Result<Applied> {
        self.atomically(|tx| tx.transition_decision(id, expected_revision, to, current_binding, fx))
    }

    pub fn resolve_decision(
        &mut self,
        r: &Resolution,
        current_binding: &Value,
    ) -> Result<DecisionState> {
        self.atomically(|tx| tx.resolve_decision(r, current_binding))
    }

    /// The orchestrator's answer to `request_evidence`: closes the answered decision and inserts
    /// its successor in one transaction (see `OutcomeTx::rebrief_decision`). `evidence` is the
    /// server-made summary; the caller says nothing else about the new brief. Refused as
    /// [`Error::Stale`] when the decision is not an answered `request_evidence` (already
    /// re-briefed, expired, moved) and [`Error::RevisionConflict`] on a lost race.
    pub fn rebrief_decision(
        &mut self,
        id: &DecisionId,
        expected_revision: i64,
        current_binding: &Value,
        evidence: &Value,
    ) -> Result<DecisionRecord> {
        match self.verified_resolution(id)? {
            Some(r)
                if r.state == DecisionState::NeedsMoreInfo && r.choice == "request_evidence" => {}
            _ => {
                return Err(Error::Stale(
                    "the decision is not waiting for evidence".into(),
                ));
            }
        }
        let action = self.decision_action(id)?;
        self.atomically(|tx| {
            tx.rebrief_decision(
                id,
                expected_revision,
                current_binding,
                evidence,
                action.clone(),
            )
        })
    }

    pub fn record_evidence(&mut self, e: &NewEvidence) -> Result<(EvidenceId, bool)> {
        self.atomically(|tx| tx.record_evidence(e))
    }

    /// Stores a check's output as an artifact of the control session (redacted like every text
    /// artifact) and returns `(session id, artifact id, sha256)` for [`NewEvidence::artifact`].
    pub fn put_check_output(&mut self, bytes: &[u8]) -> Result<(String, String, String)> {
        let r = self.w.put_artifact(
            "check_output",
            "text/plain",
            bytes,
            crate::store::Retention::Keep,
        )?;
        Ok((self.w.id().to_owned(), r.artifact_id, r.sha256))
    }

    /// The commit the outcome would be verified at: the newest finished attempt of the current
    /// plan (the same rule the store's verdict uses).
    pub fn candidate_commit(&self) -> Result<Option<String>> {
        let o = self.get_outcome(&self.id)?;
        let Some(pv) = o.plan_version else {
            return Ok(None);
        };
        Ok(self
            .w
            .conn
            .query_row(
                "SELECT a.subject_commit FROM attempts a JOIN work_items w ON w.id=a.work_item_id \
                 WHERE w.outcome_id=?1 AND w.plan_version=?2 AND a.state='finished' \
                   AND a.subject_commit IS NOT NULL \
                 ORDER BY a.ended_at DESC, a.ordinal DESC LIMIT 1",
                params![self.id.as_str(), pv],
                |r| r.get(0),
            )
            .optional()?)
    }

    /// Applies one queued command exactly once. `f` does the work through the transaction and
    /// returns the result state and body; a lost revision race, a stale brief or a moved
    /// binding inside `f` becomes `stale`. A command that is no longer queued is returned as
    /// stored, with no second effect.
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
        let stale = match run {
            Err(Error::RevisionConflict {
                aggregate,
                id,
                expected,
                actual,
            }) => json!({"aggregate": aggregate, "id": id, "expected": expected, "actual": actual}),
            Err(Error::Stale(why)) => json!({ "reason": why }),
            other => return other,
        };
        self.atomically(|tx| {
            let cmd = tx.command(idempotency_key)?;
            if cmd.state != CommandState::Queued {
                return Ok(cmd);
            }
            tx.finish_command(&cmd, CommandState::Stale, stale.clone())
        })
    }

    // ---- outbox

    /// Moves the next due `pending` row to `dispatching` (one more try) and returns it. A row
    /// that cannot be read (malformed, written by something else) is marked `failed` with the
    /// reason and skipped, so it cannot block the rest.
    pub fn claim_outbox(
        &mut self,
        now: OffsetDateTime,
        only: &OutboxFilter<'_>,
    ) -> Result<Option<OutboxRecord>> {
        let at = ts(now);
        self.atomically(|tx| {
            for (id, row) in due_pending(tx.et.tx, tx.id, &at, only)? {
                match row {
                    Ok(mut row) => {
                        tx.et.tx.execute(
                            "UPDATE outbox SET state='dispatching', tries=tries+1 WHERE id=?1 AND state='pending'",
                            [&id],
                        )?;
                        row.state = OutboxState::Dispatching;
                        row.tries += 1;
                        return Ok(Some(row));
                    }
                    Err(why) => {
                        tx.et.tx.execute(
                            "UPDATE outbox SET state='failed', receipt=?2 WHERE id=?1 AND state='pending'",
                            params![id, format!("malformed row: {why}")],
                        )?;
                    }
                }
            }
            Ok(None)
        })
    }

    /// Only a row this writer is dispatching can be completed.
    pub fn complete_outbox(&mut self, id: &str, receipt: &str) -> Result<()> {
        self.atomically(|tx| {
            let mut receipt = Value::String(receipt.to_string());
            tx.et.redact_value(&mut receipt);
            let n = tx.et.tx.execute(
                "UPDATE outbox SET state='done', receipt=?2, next_at=NULL \
                 WHERE id=?1 AND outcome_id=?3 AND state='dispatching'",
                params![id, receipt.as_str().unwrap_or_default(), tx.id.as_str()],
            )?;
            if n == 1 {
                Ok(())
            } else {
                Err(Error::Invalid(format!(
                    "outbox row {id} is not being dispatched"
                )))
            }
        })
    }

    /// Back to `pending` after a growing delay, or `failed` once [`OUTBOX_MAX_TRIES`] tries are
    /// used; the reason is kept in `receipt` (redacted). Only a `dispatching` row changes.
    pub fn fail_outbox(&mut self, id: &str, now: OffsetDateTime, why: &str) -> Result<OutboxState> {
        self.atomically(|tx| {
            let tries: u32 = tx
                .et
                .tx
                .query_row(
                    "SELECT tries FROM outbox WHERE id=?1 AND outcome_id=?2 AND state='dispatching'",
                    params![id, tx.id.as_str()],
                    |r| r.get(0),
                )
                .optional()?
                .ok_or_else(|| Error::Invalid(format!("outbox row {id} is not being dispatched")))?;
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
                "UPDATE outbox SET state=?2, next_at=?3, receipt=?4 WHERE id=?1 AND state='dispatching'",
                params![id, enum_str(&state), next, why.as_str().unwrap_or_default()],
            )?;
            Ok(state)
        })
    }

    /// Restart path (ADR-016 item 5): rows left `dispatching` by a dead writer are asked
    /// [`OutboxEffect::probe`]; an effect that already happened is completed with its receipt.
    /// Anything else counts as a failed try (the one it was claimed for), so the cap and the
    /// backoff apply: it goes back to `pending` with a `next_at`, or to `failed`. Then every due
    /// `pending` row runs once. At-least-once, so receivers must be idempotent on the row's key;
    /// a probed-done effect never runs twice.
    pub fn run_outbox(
        &mut self,
        now: OffsetDateTime,
        only: &OutboxFilter<'_>,
        effect: &mut dyn OutboxEffect,
    ) -> Result<OutboxRun> {
        let mut run = OutboxRun::default();
        let outcome = self.id.clone();
        let (stuck, skipped) = dispatching_rows(&self.w.conn, &outcome, only)?;
        run.skipped = skipped;
        for row in stuck {
            match effect.probe(&row)? {
                Some(receipt) => {
                    self.complete_outbox(&row.id, &receipt)?;
                    run.reconciled += 1;
                }
                None => {
                    if self.fail_outbox(&row.id, now, "interrupted before its receipt")?
                        == OutboxState::Failed
                    {
                        run.failed += 1;
                    }
                }
            }
        }
        while let Some(row) = self.claim_outbox(now, only)? {
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

type LenientRow = (String, std::result::Result<OutboxRecord, String>);

fn lenient(r: &Row<'_>) -> rusqlite::Result<LenientRow> {
    let id: String = r.get(0)?;
    Ok((id.clone(), outbox_row(r).map_err(|e| format!("{id}: {e}"))))
}

/// Due `pending` rows, oldest first; a row that does not parse comes back as `Err`.
fn due_pending(
    conn: &Connection,
    outcome: &OutcomeId,
    at: &str,
    only: &OutboxFilter<'_>,
) -> Result<Vec<LenientRow>> {
    let mut st = conn.prepare(&format!(
        "SELECT {OUTBOX_COLS} FROM outbox WHERE outcome_id=?1 AND state='pending' \
         AND (next_at IS NULL OR next_at<=?2){} ORDER BY rowid LIMIT 50",
        only.sql()
    ))?;
    let rows = match only.key {
        Some(k) => st.query_map(params![outcome.as_str(), at, k], lenient)?,
        None => st.query_map(params![outcome.as_str(), at], lenient)?,
    };
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// Rows left `dispatching`, and a diagnostic for each one that could not be read.
fn dispatching_rows(
    conn: &Connection,
    outcome: &OutcomeId,
    only: &OutboxFilter<'_>,
) -> Result<(Vec<OutboxRecord>, Vec<String>)> {
    let mut st = conn.prepare(&format!(
        "SELECT {OUTBOX_COLS} FROM outbox WHERE outcome_id=?1 AND state='dispatching'{} \
         ORDER BY rowid LIMIT {LIST_LIMIT}",
        only.sql_key(2)
    ))?;
    let (mut rows, mut skipped) = (vec![], vec![]);
    let key = only.key.unwrap_or("");
    let rows_iter = if only.key.is_some() {
        st.query_map(params![outcome.as_str(), key], lenient)?
    } else {
        st.query_map(params![outcome.as_str()], lenient)?
    };
    for r in rows_iter {
        match r?.1 {
            Ok(row) => rows.push(row),
            Err(why) => skipped.push(format!("outbox row skipped: {why}")),
        }
    }
    Ok((rows, skipped))
}

/// Which outbox rows a dispatcher handles. A caller names the kinds (and optionally the one key) it
/// has an effect for: rows of other kinds are left untouched, no try is consumed on them.
#[derive(Debug, Clone, Copy)]
pub struct OutboxFilter<'a> {
    /// `None`: every kind (including rows of a kind this build does not know, which are parked).
    pub kinds: Option<&'a [OutboxKind]>,
    /// `Some`: only the row with this idempotency key.
    pub key: Option<&'a str>,
}

impl<'a> OutboxFilter<'a> {
    pub fn all() -> Self {
        Self {
            kinds: None,
            key: None,
        }
    }

    pub fn kinds(kinds: &'a [OutboxKind]) -> Self {
        Self {
            kinds: Some(kinds),
            key: None,
        }
    }

    /// Only the row `key` of `kind`.
    pub fn one(kind: &'a [OutboxKind], key: &'a str) -> Self {
        Self {
            kinds: Some(kind),
            key: Some(key),
        }
    }

    fn kind_clause(&self) -> String {
        match self.kinds {
            None => String::new(),
            Some([]) => " AND 0".into(),
            // the strings are the enum's own constants, never caller text
            Some(ks) => format!(
                " AND kind IN ({})",
                ks.iter()
                    .map(|k| format!("'{}'", enum_str(k)))
                    .collect::<Vec<_>>()
                    .join(",")
            ),
        }
    }

    /// Clause for `due_pending` (the key is parameter ?3).
    fn sql(&self) -> String {
        format!(
            "{}{}",
            self.kind_clause(),
            if self.key.is_some() {
                " AND idempotency_key=?3"
            } else {
                ""
            }
        )
    }

    /// Clause with the key as parameter `?n`.
    fn sql_key(&self, n: u8) -> String {
        format!(
            "{}{}",
            self.kind_clause(),
            if self.key.is_some() {
                format!(" AND idempotency_key=?{n}")
            } else {
                String::new()
            }
        )
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
    /// Rows that could not be read, one diagnostic each (the run went on without them).
    pub skipped: Vec<String>,
}

// ---------------------------------------------------------------- transaction

/// The primitives of one outcome transaction. Every method that changes a row appends its event
/// in the same transaction. Guard inputs (approval, plan, verdict, retry budget, independent
/// evidence, binding, expiry) are derived here from the rows and the clock, never passed in.
pub struct OutcomeTx<'a, 'e> {
    et: &'a mut EventTx<'e>,
    id: &'a OutcomeId,
    policy: &'a OutcomePolicy,
    now: OffsetDateTime,
}

/// The row has its chained `evidence.recorded` event in the control session with the same content.
/// Necessary, not sufficient: the MAC below is what makes a row authentic.
const EVENT_BACKED: &str = "EXISTS (SELECT 1 FROM events ev WHERE ev.type='evidence.recorded' \
   AND ev.session_id=(SELECT control_session_id FROM outcomes WHERE id=e.outcome_id) \
   AND json_extract(ev.payload,'$.evidence_id')=e.id \
   AND json_extract(ev.payload,'$.work_item_id')=e.work_item_id \
   AND json_extract(ev.payload,'$.check_id')=e.check_id \
   AND json_extract(ev.payload,'$.trust_class')=e.trust_class \
   AND json_extract(ev.payload,'$.result')=e.result \
   AND json_extract(ev.payload,'$.subject_commit')=e.subject_commit \
   AND json_extract(ev.payload,'$.fence')=e.fence \
   AND json_extract(ev.payload,'$.sha256') IS e.sha256 \
   AND json_extract(ev.payload,'$.command_hash') IS e.command_hash \
   AND json_extract(ev.payload,'$.exit_code') IS e.exit_code \
   AND json_extract(ev.payload,'$.env_fingerprint')=e.env_fingerprint \
   AND json_extract(ev.payload,'$.criteria_ids')=json(e.criteria_ids) \
   AND json_extract(ev.payload,'$.flags')=json(e.flags))";
/// The evidence is for the newest attempt of its item (a late one from a superseded attempt
/// does not count).
const FENCE_CURRENT: &str = "e.fence >= COALESCE((SELECT MAX(a.fence) FROM attempts a WHERE a.work_item_id=e.work_item_id),0)";

/// Everything an evidence row says, as text, for its MAC.
struct EvMac<'a> {
    id: &'a str,
    outcome: &'a str,
    item: &'a str,
    attempt: &'a str,
    trust: &'a str,
    producer: &'a str,
    commit: &'a str,
    check: &'a str,
    command_hash: &'a str,
    criteria: &'a str,
    result: &'a str,
    exit_code: &'a str,
    env: &'a str,
    artifact: &'a str,
    sha: &'a str,
    fence: &'a str,
    captured_at: &'a str,
    flags: &'a str,
    kind: &'a str,
    tree: &'a str,
}

impl EvMac<'_> {
    fn parts(&self) -> [&str; 20] {
        [
            self.id,
            self.outcome,
            self.item,
            self.attempt,
            self.trust,
            self.producer,
            self.commit,
            self.check,
            self.command_hash,
            self.criteria,
            self.result,
            self.exit_code,
            self.env,
            self.artifact,
            self.sha,
            self.fence,
            self.captured_at,
            self.flags,
            self.kind,
            self.tree,
        ]
    }
}

/// An evidence row that counts: with a chained event of the same content AND a MAC (under a
/// subkey of the chain key) over all of its columns, flags included. A writer who cannot read
/// `chain.key` cannot produce either, and cannot change a column, add or drop a flag, without the
/// MAC failing.
struct AuthRow {
    item: String,
    flags: Vec<String>,
    fact: synthesis_core::outcome::EvidenceFact,
}

/// An evidence row that passed the authenticity checks (chained event and MAC), for the
/// verifier's decisions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthenticEvidence {
    pub work_item_id: WorkItemId,
    pub check_id: String,
    pub subject_commit: String,
    pub trust_class: TrustClass,
    pub result: String,
    pub flags: Vec<String>,
    /// The row belongs to the newest attempt of its work item: evidence of an earlier attempt
    /// (even for the same commit) does not count for a later one.
    pub fence_current: bool,
}

fn authentic_evidence(
    conn: &Connection,
    key: &[u8; 32],
    id: &OutcomeId,
    contract: &Contract,
) -> Result<Vec<AuthRow>> {
    let mut st = conn.prepare(&format!(
        "SELECT e.id,e.work_item_id,COALESCE(e.attempt_id,''),e.trust_class,e.producer,e.subject_commit, \
                e.check_id,COALESCE(e.command_hash,''),e.criteria_ids,e.result,COALESCE(e.exit_code,''), \
                e.env_fingerprint,COALESCE(e.artifact_session_id,'')||'|'||COALESCE(e.artifact_id,''), \
                COALESCE(e.sha256,''),e.fence,e.captured_at,e.flags,COALESCE(e.mac,''), \
                e.kind,COALESCE(e.subject_tree,''), {FENCE_CURRENT} \
         FROM evidence e WHERE e.outcome_id=?1 AND {EVENT_BACKED} \
           AND EXISTS (SELECT 1 FROM work_items wi JOIN plans p \
                         ON p.outcome_id=wi.outcome_id AND p.version=wi.plan_version \
                       WHERE wi.id=e.work_item_id AND p.contract_version=?2)"
    ))?;
    // evidence of a plan made for another contract version never counts for this one (B4)
    let rows = st.query_map(params![id.as_str(), contract.version], |r| {
        let mut v: Vec<String> = Vec::with_capacity(20);
        for i in 0..20 {
            // numbers (exit code, fence) and text are all compared as text
            v.push(match r.get_ref(i)? {
                rusqlite::types::ValueRef::Integer(n) => n.to_string(),
                rusqlite::types::ValueRef::Text(t) => String::from_utf8_lossy(t).into_owned(),
                _ => String::new(),
            });
        }
        Ok((v, r.get::<_, bool>(20)?))
    })?;
    let mut out = vec![];
    for row in rows {
        let (v, fence_current) = row?;
        let m = EvMac {
            id: &v[0],
            outcome: id.as_str(),
            item: &v[1],
            attempt: &v[2],
            trust: &v[3],
            producer: &v[4],
            commit: &v[5],
            check: &v[6],
            command_hash: &v[7],
            criteria: &v[8],
            result: &v[9],
            exit_code: &v[10],
            env: &v[11],
            artifact: &v[12],
            sha: &v[13],
            fence: &v[14],
            captured_at: &v[15],
            flags: &v[16],
            kind: &v[18],
            tree: &v[19],
        };
        if !mac_ok(key, "evidence", &m.parts(), &v[17]) {
            continue; // forged, altered or from an old build: counts for nothing
        }
        // a check the contract froze runs that command and no other
        if let Some(c) = contract.checks.iter().find(|c| c.id == v[6])
            && v[7] != sha_hex(c.command.as_bytes())
        {
            continue;
        }
        let flags: Vec<String> = serde_json::from_str(&v[16])?;
        out.push(AuthRow {
            item: v[1].clone(),
            flags: flags.clone(),
            fact: synthesis_core::outcome::EvidenceFact {
                trust: serde_json::from_value(Value::String(v[3].clone()))?,
                result: v[9].clone(),
                commit: v[5].clone(),
                check_id: v[6].clone(),
                criteria: serde_json::from_str(&v[8])?,
                flagged: !flags.is_empty(),
                fence_current,
            },
        });
    }
    Ok(out)
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

    fn outcome(&self) -> Result<OutcomeRecord> {
        self.et
            .tx
            .query_row(
                &format!("SELECT {OUTCOME_COLS} FROM outcomes WHERE id=?1"),
                [self.id.as_str()],
                outcome_row,
            )
            .optional()?
            .ok_or_else(|| Error::NotFound(format!("outcome {}", self.id)))
    }

    /// Writes the effects; returns, per outbox row, whether it was newly inserted.
    fn effects(&mut self, fx: &Effects) -> Result<Vec<bool>> {
        for (kind, payload) in &fx.events {
            self.et.append(kind, payload.clone())?;
        }
        fx.outbox.iter().map(|o| self.enqueue(o)).collect()
    }

    fn applied(&mut self, revision: i64, fx: &Effects) -> Result<Applied> {
        Ok(Applied {
            revision,
            outbox_new: self.effects(fx)?,
        })
    }

    /// Inserts an outbox row. `false`: the key exists with the same effect (nothing changes). The
    /// same key with a different kind or payload is an error: one key, one effect.
    pub fn enqueue(&mut self, o: &NewOutbox) -> Result<bool> {
        let mut payload = o.payload.clone();
        self.et.redact_value(&mut payload);
        if payload.to_string().len() > MAX_OUTBOX_PAYLOAD_BYTES {
            return Err(Error::TooLarge {
                what: "outbox payload",
                size: payload.to_string().len(),
                max: MAX_OUTBOX_PAYLOAD_BYTES,
            });
        }
        let inserted = self.et.tx.execute(
            "INSERT INTO outbox(id,outcome_id,idempotency_key,kind,payload,state) \
             VALUES (?1,?2,?3,?4,?5,'pending') ON CONFLICT(outcome_id,idempotency_key) DO NOTHING",
            params![
                new_uuid(),
                self.id.as_str(),
                o.idempotency_key,
                enum_str(&o.kind),
                payload.to_string()
            ],
        )? == 1;
        if !inserted {
            let (kind, stored): (String, String) = self.et.tx.query_row(
                "SELECT kind,payload FROM outbox WHERE outcome_id=?1 AND idempotency_key=?2",
                params![self.id.as_str(), o.idempotency_key],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )?;
            let same = kind == enum_str(&o.kind)
                && serde_json::from_str::<Value>(&stored).is_ok_and(|v| v == payload);
            if !same {
                return Err(Error::Invalid(format!(
                    "outbox key {:?} is already queued for a different effect",
                    o.idempotency_key
                )));
            }
        }
        Ok(inserted)
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

    fn conflict_or(&self, agg: ev::Aggregate, id: &str, expected: i64, actual: i64) -> Result<()> {
        // a revision this close to the end of the range is not one of ours: no `+ 1` below can overflow
        if actual >= i64::MAX - 1 {
            return Err(Error::Tampered(format!(
                "{} {id} has an impossible revision",
                agg.as_str()
            )));
        }
        if expected == actual {
            Ok(())
        } else {
            Err(conflict(agg.as_str(), id, expected, actual))
        }
    }

    // ---- contract and plan

    pub fn approve_contract(&mut self, version: u32, hash: &str, approver: &str) -> Result<()> {
        if !valid_command_text(approver, 128) {
            return Err(Error::Invalid(
                "approver must be 1 to 128 printable characters".into(),
            ));
        }
        let stored: Option<(String, String, Option<String>)> = self
            .et
            .tx
            .query_row(
                "SELECT body,hash,approved_at FROM contracts WHERE outcome_id=?1 AND version=?2",
                params![self.id.as_str(), version],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?;
        let Some((body, stored, approved)) = stored else {
            return Err(Error::NotFound(format!("contract v{version}")));
        };
        // the person approved exactly the stored body, and the stored hash is that body's hash
        let body_ok = serde_json::from_str::<Value>(&body)
            .map(|v| body_hash(&v) == stored)
            .unwrap_or(false);
        if stored != hash || !body_ok {
            return Err(Error::Invalid(
                "contract hash does not match the stored contract".into(),
            ));
        }
        // A version above the outcome's own is a revision (B4): only a blocked outcome with no live
        // attempt moves to it, only to the newest version, and only from an approved contract.
        let o = self.outcome()?;
        let rebinds = version > o.contract_version;
        if rebinds {
            let newest: u32 = self.et.tx.query_row(
                "SELECT MAX(version) FROM contracts WHERE outcome_id=?1",
                [self.id.as_str()],
                |r| r.get(0),
            )?;
            let why = if o.state != OutcomeState::Blocked {
                Some(format!(
                    "a revised contract is approved while the outcome is blocked, not {}",
                    enum_str(&o.state)
                ))
            } else if version != newest {
                Some(format!(
                    "only the newest version (v{newest}) can be approved"
                ))
            } else if approved.is_some() {
                Some("a revision that is already approved cannot be rebound".into())
            } else if self.has_live_attempt()? {
                Some("an attempt is still live: approve when it has ended".into())
            } else if !contract_authentic(
                self.et.tx,
                Some(self.et.key()),
                self.id,
                o.contract_version,
            )? {
                Some("the outcome's current contract does not verify as approved".into())
            } else {
                None
            };
            if let Some(why) = why {
                return Err(Error::Invalid(why));
            }
        }
        if approved.is_some() {
            // an approval that does not verify (a bare UPDATE, a poisoned row) is tampering: it is
            // reported, never accepted as "already approved" and never silently replaced
            return if contract_authentic(self.et.tx, Some(self.et.key()), self.id, version)? {
                Ok(())
            } else {
                Err(Error::Invalid(format!(
                    "contract v{version} carries an approval that does not verify; the store was altered"
                )))
            };
        }
        // the ceilings may have tightened since the contract was created
        let contract: Contract = serde_json::from_str(&body)?;
        self.policy.check(&contract)?;
        if rebinds {
            // a row written around `revise_contract` (a bare INSERT) is held to the same rule
            self.same_kind(&contract)?;
        }
        let at = ts(self.now);
        let m = contract_mac(
            self.et.key(),
            self.id.as_str(),
            version,
            hash,
            approver,
            &at,
        );
        self.et.tx.execute(
            "UPDATE contracts SET approved_by=?3, approved_at=?4, approved_mac=?5 WHERE outcome_id=?1 AND version=?2",
            params![self.id.as_str(), version, approver, at, m],
        )?;
        self.et.append(
            "outcome.contract_approved",
            json!({"outcome_id": self.id, "version": version, "hash": hash, "approver": approver}),
        )?;
        if rebinds {
            // The outcome now stands on the revision: every decision and grant bound to the old
            // version no longer matches (the decision driver invalidates and revokes them), and
            // the old plan no longer fits (a replan is stored before the outcome runs again).
            let widens =
                diff_contracts(&self.stored_contract(o.contract_version)?, &contract).widens;
            let goal = self.et.redact_str(&contract.goal);
            self.et.tx.execute(
                "UPDATE outcomes SET contract_version=?2, goal=?4, revision=revision+1, updated_at=?3 WHERE id=?1",
                params![self.id.as_str(), version, now(), goal],
            )?;
            self.et.append(
                "outcome.contract_rebound",
                json!({"outcome_id": self.id, "from_version": o.contract_version, "version": version,
                       "hash": hash, "widens": widens}),
            )?;
        }
        Ok(())
    }

    /// A changed contract is a NEW version (stored versions are immutable): unapproved, and the
    /// current plan (made for the version before) no longer lets the outcome become `ready`, so
    /// the approval of the old version never covers the new text. Only before the work starts
    /// (`draft`, `contract_pending`); the next version only.
    pub fn new_contract_version(
        &mut self,
        expected_revision: i64,
        contract: &Contract,
    ) -> Result<()> {
        let o = self.outcome()?;
        self.conflict_or(
            ev::Aggregate::Outcome,
            self.id.as_str(),
            expected_revision,
            o.revision,
        )?;
        if !matches!(o.state, OutcomeState::Draft | OutcomeState::ContractPending) {
            return Err(Error::Invalid(format!(
                "the contract cannot change while the outcome is {}",
                enum_str(&o.state)
            )));
        }
        if contract.version != o.contract_version + 1 {
            return Err(Error::Invalid(
                "a contract version must follow the current one".into(),
            ));
        }
        let hash = self.insert_contract(contract)?;
        let goal = self.et.redact_str(&contract.goal);
        self.et.tx.execute(
            "UPDATE outcomes SET contract_version=?2, goal=?4, revision=revision+1, updated_at=?3 WHERE id=?1",
            params![self.id.as_str(), contract.version, now(), goal],
        )?;
        self.et.append(
            ev::CONTRACT_CREATED,
            json!({"outcome_id": self.id, "version": contract.version, "contract_hash": hash}),
        )?;
        Ok(())
    }

    /// Validates `contract` against the contract rules and the policy ceilings and stores it as an
    /// unapproved version; returns its hash.
    fn insert_contract(&mut self, contract: &Contract) -> Result<String> {
        validate_contract(contract).map_err(refuse)?;
        self.policy.check(contract)?;
        self.same_kind(contract)?;
        let mut body = serde_json::to_value(contract)?;
        self.et.redact_value(&mut body);
        let hash = body_hash(&body);
        self.et.tx.execute(
            "INSERT INTO contracts(outcome_id,version,hash,body) VALUES (?1,?2,?3,?4)",
            params![self.id.as_str(), contract.version, hash, body.to_string()],
        )?;
        Ok(hash)
    }

    /// A goal keeps its kind for good (a write goal never becomes read-only, nor the reverse): the
    /// outcome's base commit and its attempts are made for one kind. A person who wants the other
    /// kind creates a new goal.
    fn same_kind(&self, next: &Contract) -> Result<()> {
        let o = self.outcome()?;
        if self.stored_contract(o.contract_version)?.kind != next.kind {
            return Err(Error::Invalid(
                "the kind of a goal never changes: create a new goal instead".into(),
            ));
        }
        Ok(())
    }

    /// Is an attempt of this outcome still `starting` or `running`?
    fn has_live_attempt(&self) -> Result<bool> {
        Ok(self.et.tx.query_row(
            "SELECT EXISTS (SELECT 1 FROM attempts a JOIN work_items w ON w.id=a.work_item_id \
             WHERE w.outcome_id=?1 AND a.state IN ('starting','running'))",
            [self.id.as_str()],
            |r| r.get(0),
        )?)
    }

    /// A person's change of scope (`edit_scope`, orchestrator B4): stores `contract` as the next
    /// version, UNAPPROVED, while the outcome is `blocked` and no attempt is live. The outcome keeps
    /// pointing at the approved contract it has: nothing runs under the revision, and every decision
    /// and grant stays bound to the old version, until [`Self::approve_contract`] approves exactly
    /// this hash and rebinds. The version is numbered here (after the newest stored one), not by the
    /// caller. A revision that changes nothing is refused. Returns `(version, hash)`.
    pub fn revise_contract(
        &mut self,
        expected_revision: i64,
        mut contract: Contract,
    ) -> Result<(u32, String)> {
        let o = self.outcome()?;
        self.conflict_or(
            ev::Aggregate::Outcome,
            self.id.as_str(),
            expected_revision,
            o.revision,
        )?;
        if o.state != OutcomeState::Blocked {
            return Err(Error::Invalid(format!(
                "a contract is revised while the outcome is blocked, not {}",
                enum_str(&o.state)
            )));
        }
        if self.has_live_attempt()? {
            return Err(Error::Invalid(
                "an attempt is still live: revise the contract when it has ended".into(),
            ));
        }
        if !contract_authentic(self.et.tx, Some(self.et.key()), self.id, o.contract_version)? {
            return Err(Error::Invalid(
                "the outcome's current contract does not verify as approved".into(),
            ));
        }
        let newest: u32 = self.et.tx.query_row(
            "SELECT MAX(version) FROM contracts WHERE outcome_id=?1",
            [self.id.as_str()],
            |r| r.get(0),
        )?;
        let drafts: i64 = self.et.tx.query_row(
            "SELECT COUNT(*) FROM contracts WHERE outcome_id=?1 AND version>?2 AND approved_at IS NULL",
            params![self.id.as_str(), o.contract_version],
            |r| r.get(0),
        )?;
        if drafts >= MAX_CONTRACT_DRAFTS {
            return Err(Error::Invalid(format!(
                "{MAX_CONTRACT_DRAFTS} revisions wait for approval: approve the newest or cancel the outcome"
            )));
        }
        contract.version = newest + 1;
        let current = self.stored_contract(o.contract_version)?;
        if diff_contracts(&current, &contract).changes.is_empty() {
            return Err(Error::Invalid(
                "the revision changes nothing from the approved contract".into(),
            ));
        }
        let hash = self.insert_contract(&contract)?;
        self.et.tx.execute(
            "UPDATE outcomes SET revision=revision+1, updated_at=?2 WHERE id=?1",
            params![self.id.as_str(), now()],
        )?;
        self.et.append(
            "outcome.contract_revised",
            json!({"outcome_id": self.id, "version": contract.version, "contract_hash": hash,
                   "base_version": o.contract_version}),
        )?;
        Ok((contract.version, hash))
    }

    fn stored_contract(&self, version: u32) -> Result<Contract> {
        let body: String = self
            .et
            .tx
            .query_row(
                "SELECT body FROM contracts WHERE outcome_id=?1 AND version=?2",
                params![self.id.as_str(), version],
                |r| r.get(0),
            )
            .optional()?
            .ok_or_else(|| Error::NotFound(format!("contract v{version}")))?;
        Ok(serde_json::from_str(&body)?)
    }

    /// Validates `plan` against the stored, approved contract it names, and stores it with its
    /// work items (state `created`) and dependencies as the outcome's current plan. Allowed only
    /// while the outcome is `contract_pending`, `blocked` or `rework_required`, and only as the
    /// next version. Returns `(key, id)` of every work item.
    pub fn insert_plan(&mut self, plan: &Plan) -> Result<Vec<(String, WorkItemId)>> {
        let o = self.outcome()?;
        if !matches!(
            o.state,
            OutcomeState::ContractPending | OutcomeState::Blocked | OutcomeState::ReworkRequired
        ) {
            return Err(Error::Invalid(format!(
                "a plan cannot be stored while the outcome is {}",
                enum_str(&o.state)
            )));
        }
        if plan.version != o.plan_version.unwrap_or(0) + 1 {
            return Err(Error::Invalid(
                "a plan version must follow the current one".into(),
            ));
        }
        if plan.contract_version != o.contract_version
            || !contract_authentic(
                self.et.tx,
                Some(self.et.key()),
                self.id,
                plan.contract_version,
            )?
        {
            return Err(Error::Invalid(
                "a plan needs the approved current contract".into(),
            ));
        }
        let contract = self.stored_contract(plan.contract_version)?;
        let chash: String = self.et.tx.query_row(
            "SELECT hash FROM contracts WHERE outcome_id=?1 AND version=?2",
            params![self.id.as_str(), plan.contract_version],
            |r| r.get(0),
        )?;
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

    // ---- derived facts

    /// The stored contract the plan was made for.
    fn plan_contract(&self, plan_version: u32) -> Result<Contract> {
        let body: String = self.et.tx.query_row(
            "SELECT c.body FROM plans p JOIN contracts c \
               ON c.outcome_id=p.outcome_id AND c.version=p.contract_version \
             WHERE p.outcome_id=?1 AND p.version=?2",
            params![self.id.as_str(), plan_version],
            |r| r.get(0),
        )?;
        Ok(serde_json::from_str(&body)?)
    }

    /// Attempts per work item the plan's contract allows, under the settings ceiling.
    fn max_attempts(&self, plan_version: u32) -> Result<u32> {
        Ok(self
            .plan_contract(plan_version)?
            .envelope
            .budgets
            .max_attempts_per_work_item
            .min(self.policy.max_attempts_per_work_item))
    }

    /// The commit of the attempt that ended last among the current plan's items: the candidate
    /// the evidence must name.
    fn candidate_commit(&self, plan_version: u32) -> Result<Option<String>> {
        Ok(self
            .et
            .tx
            .query_row(
                "SELECT a.subject_commit FROM attempts a JOIN work_items w ON w.id=a.work_item_id \
                 WHERE w.outcome_id=?1 AND w.plan_version=?2 AND a.state='finished' \
                   AND a.subject_commit IS NOT NULL \
                 ORDER BY a.ended_at DESC, a.ordinal DESC LIMIT 1",
                params![self.id.as_str(), plan_version],
                |r| r.get(0),
            )
            .optional()?)
    }

    /// `independent_check` evidence, passed, for the commit of this item's last finished attempt
    /// (a `verify` item has no attempt: the last finished attempt it requires), from the newest
    /// attempt, without `definitions_changed`.
    fn item_has_independent_evidence(&self, item: &WorkItemId) -> Result<bool> {
        let own = self
            .et
            .tx
            .query_row(
                "SELECT subject_commit FROM attempts WHERE work_item_id=?1 AND state='finished' \
                 AND subject_commit IS NOT NULL ORDER BY ordinal DESC LIMIT 1",
                [item.as_str()],
                |r| r.get::<_, String>(0),
            )
            .optional()?;
        let commit = match own {
            Some(c) => Some(c),
            None => self
                .et
                .tx
                .query_row(
                    "SELECT a.subject_commit FROM attempts a \
                     JOIN work_item_deps d ON d.requires_id=a.work_item_id \
                     WHERE d.work_item_id=?1 AND a.state='finished' AND a.subject_commit IS NOT NULL \
                     ORDER BY a.ended_at DESC, a.ordinal DESC LIMIT 1",
                    [item.as_str()],
                    |r| r.get::<_, String>(0),
                )
                .optional()?,
        };
        let Some(commit) = commit else {
            return Ok(false);
        };
        let o = self.outcome()?;
        let contract = self.stored_contract(o.contract_version)?;
        Ok(
            authentic_evidence(self.et.tx, self.et.key(), self.id, &contract)?
                .iter()
                .any(|r| {
                    r.item == item.as_str()
                        && r.fact.trust == TrustClass::IndependentCheck
                        && r.fact.result == "passed"
                        && r.fact.commit == commit
                        && !r.fact.flagged
                        && r.fact.fence_current
                }),
        )
    }

    /// The verdict for `Verified` and `Completed` (ADR-020 section 4): every item of the current
    /// plan `succeeded`, and the pure `synthesis_core::outcome::verdict` over the AUTHENTIC evidence
    /// rows (event-backed, MAC valid, the frozen command's hash) says every mandatory criterion
    /// `Passed` for the candidate commit. The candidate is still "the newest finished attempt of
    /// the plan", which is unsound for parallel branches until integration exists.
    fn verdict(&self, o: &OutcomeRecord) -> Result<bool> {
        let Some(pv) = o.plan_version else {
            return Ok(false);
        };
        let (total, open): (i64, i64) = self.et.tx.query_row(
            "SELECT COUNT(*), COALESCE(SUM(state <> 'succeeded'),0) FROM work_items \
             WHERE outcome_id=?1 AND plan_version=?2",
            params![self.id.as_str(), pv],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        if total == 0 || open > 0 {
            return Ok(false);
        }
        let Some(commit) = self.candidate_commit(pv)? else {
            return Ok(false);
        };
        let contract = self.stored_contract(o.contract_version)?;
        let facts: Vec<_> = authentic_evidence(self.et.tx, self.et.key(), self.id, &contract)?
            .into_iter()
            .map(|r| r.fact)
            .collect();
        Ok(synthesis_core::outcome::verdict(&contract, &facts, &commit).verified)
    }

    // ---- transitions

    /// Guards are derived: the contract approval is checked against its MAC, the plan must
    /// exist for the current contract, and `Verified`/`Completed` need the store's own verdict.
    pub fn transition_outcome(
        &mut self,
        expected_revision: i64,
        to: OutcomeState,
        reason: Option<&str>,
        fx: &Effects,
    ) -> Result<Applied> {
        let o = self.outcome()?;
        self.conflict_or(
            ev::Aggregate::Outcome,
            self.id.as_str(),
            expected_revision,
            o.revision,
        )?;
        let approved =
            contract_authentic(self.et.tx, Some(self.et.key()), self.id, o.contract_version)?;
        let plan_valid: bool = self.et.tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM plans p WHERE p.outcome_id=?1 AND p.version=?2 AND p.contract_version=?3)",
            params![self.id.as_str(), o.plan_version, o.contract_version],
            |r| r.get(0),
        )?;
        let needs_verdict = matches!(to, OutcomeState::Verified | OutcomeState::Completed);
        let verdict = needs_verdict && self.verdict(&o)?;
        let guards = OutcomeGuards {
            contract_approved: approved,
            plan_valid,
            verdict_verified: verdict && to == OutcomeState::Verified,
            delivery_ready: verdict && to == OutcomeState::Completed,
        };
        o.state.transition(to, &guards).map_err(refuse)?;
        let rev = o.revision;
        let changed = self.et.tx.execute(
            "UPDATE outcomes SET state=?2, revision=revision+1, updated_at=?4 WHERE id=?1 AND revision=?3",
            params![self.id.as_str(), enum_str(&to), rev, now()],
        )?;
        if changed != 1 {
            return Err(conflict("outcome", self.id.as_str(), rev, rev + 1));
        }
        self.changed(
            ev::Aggregate::Outcome,
            self.id.as_str(),
            rev + 1,
            &enum_str(&o.state),
            &enum_str(&to),
            reason,
        )?;
        self.applied(rev + 1, fx)
    }

    fn load_item(&self, id: &WorkItemId) -> Result<(String, i64, u32, u32, WorkItemKind)> {
        self.et
            .tx
            .query_row(
                "SELECT state,revision,retries_used,plan_version,kind FROM work_items WHERE id=?1 AND outcome_id=?2",
                params![id.as_str(), self.id.as_str()],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, col(r, 4)?)),
            )
            .optional()?
            .ok_or_else(|| Error::NotFound(format!("work item {id}")))
    }

    /// The retry budget comes from the stored contract (under the settings ceiling) and
    /// `retries_used` from the row; `Succeeded` needs independent evidence found in the rows.
    /// Leaving `retryable_failure` for `ready` starts a retry and counts it.
    pub fn transition_work_item(
        &mut self,
        id: &WorkItemId,
        expected_revision: i64,
        to: WorkItemState,
        fx: &Effects,
    ) -> Result<Applied> {
        let (state, rev, used, plan_version, _) = self.load_item(id)?;
        self.conflict_or(ev::Aggregate::WorkItem, id.as_str(), expected_revision, rev)?;
        let from: WorkItemState = enum_from(&state)?;
        let guards = WorkItemGuards {
            retries_used: used,
            max_retries: self.max_attempts(plan_version)?.saturating_sub(1),
            independent_evidence: to == WorkItemState::Succeeded
                && self.item_has_independent_evidence(id)?,
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
        self.applied(rev + 1, fx)
    }

    // ---- attempts

    /// Inserts attempt `ordinal` of an `agent` work item in state `starting`. Refused unless
    /// `ordinal` is the next one (a replayed allocation is [`Error::AlreadyExists`]: adopt the
    /// existing row), within the attempt budget, with no live attempt before it, and with a
    /// session that is an ordinary (non-control) child of this outcome's control session. The
    /// fence equals the ordinal. Returns the row and, per outbox row, whether it was new.
    pub fn new_attempt(
        &mut self,
        item: &WorkItemId,
        ordinal: u32,
        session_id: &str,
        worker_kind: &str,
        fx: &Effects,
    ) -> Result<(AttemptRecord, Vec<bool>)> {
        if !matches!(worker_kind, "local-inproc" | "local-engine") {
            return Err(Error::Invalid("worker kind is invalid".into()));
        }
        let (state, _, _, plan_version, kind) = self.load_item(item)?;
        // allocated when the scheduler has the item ready or leased (a retry passes through `ready`)
        if !matches!(
            enum_from::<WorkItemState>(&state)?,
            WorkItemState::Ready | WorkItemState::Leased
        ) {
            return Err(Error::Invalid(format!(
                "an attempt starts from a ready or leased work item, not {state}"
            )));
        }
        // the ceilings may have tightened since the contract was approved
        self.policy.check(&self.plan_contract(plan_version)?)?;
        if kind != WorkItemKind::Agent {
            return Err(Error::Invalid(
                "only an agent work item runs attempts".into(),
            ));
        }
        let (max_ord, live): (u32, bool) = self.et.tx.query_row(
            "SELECT COALESCE(MAX(ordinal),0), COALESCE(MAX(state IN ('starting','running')),0) \
             FROM attempts WHERE work_item_id=?1",
            [item.as_str()],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        if ordinal != 0 && ordinal <= max_ord {
            return Err(Error::AlreadyExists(format!("attempt {ordinal} of {item}")));
        }
        let next_ord = max_ord
            .checked_add(1)
            .ok_or_else(|| Error::Invalid("attempt ordinal overflow".into()))?;
        if ordinal != next_ord {
            return Err(Error::Invalid(format!(
                "the next attempt of this item is {next_ord}"
            )));
        }
        if ordinal > self.max_attempts(plan_version)? {
            return Err(Error::Invalid(
                "the attempt budget of this item is used".into(),
            ));
        }
        if live {
            return Err(Error::Invalid("the previous attempt is still live".into()));
        }
        let control: String = self.et.tx.query_row(
            "SELECT control_session_id FROM outcomes WHERE id=?1",
            [self.id.as_str()],
            |r| r.get(0),
        )?;
        let sess: Option<(String, Option<String>)> = self
            .et
            .tx
            .query_row(
                "SELECT kind,parent_session_id FROM sessions WHERE id=?1",
                [session_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        match sess {
            Some((k, Some(parent))) if k == "chat" && parent == control => {}
            _ => {
                return Err(Error::Invalid(
                    "an attempt's session must be an ordinary child of the control session".into(),
                ));
            }
        }
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
        let outbox_new = self.effects(fx)?;
        let rec = self.et.tx.query_row(
            &format!("SELECT {ATTEMPT_COLS} FROM attempts a WHERE a.id=?1"),
            [id.as_str()],
            attempt_row,
        )?;
        Ok((rec, outbox_new))
    }

    pub fn transition_attempt(
        &mut self,
        id: &AttemptId,
        expected_revision: i64,
        to: AttemptState,
        end: &AttemptEnd,
        fx: &Effects,
    ) -> Result<Applied> {
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
        // the orchestrator may still mark a superseded attempt lost or cancelled; it may not
        // finish it as if its worker were current
        if to == AttemptState::Finished && self.superseded(id)? {
            return Err(Error::Stale(
                "a newer attempt of this work item exists".into(),
            ));
        }
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
        self.applied(rev + 1, fx)
    }

    /// Why an attempt's worktree was kept (a refused cleanup: dirty, ignored files, unmerged
    /// commits). Allowed on a finished attempt; the reason is redacted and bounded.
    pub fn set_attempt_retained(&mut self, id: &AttemptId, reason: &str) -> Result<()> {
        let mut r = Value::String(reason.chars().take(500).collect());
        self.et.redact_value(&mut r);
        let n = self.et.tx.execute(
            "UPDATE attempts SET retained_reason=?2 WHERE id=?1 AND work_item_id IN \
               (SELECT id FROM work_items WHERE outcome_id=?3)",
            params![
                id.as_str(),
                r.as_str().unwrap_or_default(),
                self.id.as_str()
            ],
        )?;
        if n == 1 {
            Ok(())
        } else {
            Err(Error::NotFound(format!("attempt {id}")))
        }
    }

    /// Is there an attempt of the same work item with a higher fence? A worker of an older attempt
    /// is stale: nothing it reports is accepted any more.
    fn superseded(&self, id: &AttemptId) -> Result<bool> {
        Ok(self.et.tx.query_row(
            "SELECT EXISTS (SELECT 1 FROM attempts n JOIN attempts a ON a.work_item_id=n.work_item_id \
             WHERE a.id=?1 AND n.fence > a.fence)",
            [id.as_str()],
            |r| r.get(0),
        )?)
    }

    fn live_attempt(&self, id: &AttemptId, fence: i64) -> Result<AttemptRecord> {
        let a = self
            .et
            .tx
            .query_row(
                &format!(
                    "SELECT {ATTEMPT_COLS} FROM attempts a JOIN work_items w ON w.id=a.work_item_id \
                     WHERE a.id=?1 AND w.outcome_id=?2"
                ),
                params![id.as_str(), self.id.as_str()],
                attempt_row,
            )
            .optional()?
            .ok_or_else(|| Error::NotFound(format!("attempt {id}")))?;
        if a.fence != fence {
            return Err(Error::Stale(format!(
                "fence {fence} is not attempt {id}'s fence {}",
                a.fence
            )));
        }
        if self.superseded(id)? {
            return Err(Error::Stale(
                "a newer attempt of this work item exists".into(),
            ));
        }
        if !matches!(a.state, AttemptState::Starting | AttemptState::Running) {
            return Err(Error::Stale(format!(
                "the attempt is {}",
                enum_str(&a.state)
            )));
        }
        Ok(a)
    }

    /// Records the worker process of a live attempt, once. The fence must be the attempt's own and
    /// no newer attempt may exist (a stale worker's writes are refused).
    pub fn record_worker(
        &mut self,
        id: &AttemptId,
        fence: i64,
        pid: u32,
        start: Option<&str>,
    ) -> Result<()> {
        self.live_attempt(id, fence)?;
        let at = ts(self.now);
        self.et.tx.execute(
            "UPDATE attempts SET worker_pid=?2, worker_start=?3, heartbeat_at=?4 WHERE id=?1",
            params![id.as_str(), pid, start, at],
        )?;
        Ok(())
    }

    /// Reserves `usd` for the attempt, once, before its worker is spawned: from then on the
    /// outcome counts it as spent until a final usage is recorded. Same fence rules as the
    /// worker's writes; the amount must be positive and finite. The cap is checked HERE, in the
    /// transaction that writes the reservation (like [`Self::reserve_plan_run`]): what the caller
    /// read earlier is only a proposal, so attempts and planning runs together never reserve past
    /// the outcome cap, in any order. A reservation over what is left is refused, never lowered
    /// (the worker was told its amount as its own cap).
    pub fn reserve_budget(&mut self, id: &AttemptId, fence: i64, usd: f64) -> Result<()> {
        if !usd.is_finite() || usd <= 0.0 {
            return Err(Error::Invalid(
                "a reservation must be a positive number".into(),
            ));
        }
        self.live_attempt(id, fence)?;
        let o = self.outcome()?;
        let cap = self
            .stored_contract(o.contract_version)?
            .envelope
            .budgets
            .max_cost_usd;
        let own: f64 = self.et.tx.query_row(
            "SELECT COALESCE(reserved_usd,0) FROM attempts WHERE id=?1",
            [id.as_str()],
            |r| r.get(0),
        )?;
        // this attempt's own earlier reservation is replaced, not added; a NaN spend is not "enough left"
        let left = cap - spend(self.et.tx, self.id)? + own;
        if left.is_nan() || usd > left + 1e-9 {
            return Err(Error::Invalid(
                "the outcome cost cap has too little left for this attempt".into(),
            ));
        }
        self.et.tx.execute(
            "UPDATE attempts SET reserved_usd=?2 WHERE id=?1",
            params![id.as_str(), usd],
        )?;
        Ok(())
    }

    /// Records, once, the contract version and compiled-policy hash the attempt starts under.
    pub fn bind_policy(
        &mut self,
        id: &AttemptId,
        fence: i64,
        contract_version: u32,
        policy_hash: &str,
    ) -> Result<()> {
        if policy_hash.is_empty() || policy_hash.len() > 128 {
            return Err(Error::Invalid(
                "a policy hash is 1 to 128 characters".into(),
            ));
        }
        self.live_attempt(id, fence)?;
        self.et.tx.execute(
            "UPDATE attempts SET bound_contract_version=?2, bound_policy_hash=?3 WHERE id=?1",
            params![id.as_str(), contract_version, policy_hash],
        )?;
        Ok(())
    }

    /// Records, once, the sandbox backend and guarantee the owner resolved for this launch.
    pub fn bind_sandbox(
        &mut self,
        id: &AttemptId,
        fence: i64,
        backend: &str,
        guarantee: &str,
    ) -> Result<()> {
        if backend.is_empty()
            || backend.len() > 64
            || !backend
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        {
            return Err(Error::Invalid(
                "a sandbox backend is 1 to 64 letters, digits, - or _".into(),
            ));
        }
        if !matches!(guarantee, "none" | "weak" | "strong") {
            return Err(Error::Invalid(
                "a sandbox guarantee is none, weak or strong".into(),
            ));
        }
        self.live_attempt(id, fence)?;
        self.et.tx.execute(
            "UPDATE attempts SET sandbox_backend=?2, sandbox_guarantee=?3 WHERE id=?1",
            params![id.as_str(), backend, guarantee],
        )?;
        Ok(())
    }

    /// Gives a reservation back: only for a worker that never started (its spawn failed), so
    /// nothing was spent. Same fence rules; a reservation can be lowered, never raised.
    pub fn release_budget(&mut self, id: &AttemptId, fence: i64) -> Result<()> {
        self.live_attempt(id, fence)?;
        // a recorded worker pid means a process existed: its reservation stays
        let n = self.et.tx.execute(
            "UPDATE attempts SET reserved_usd=0 WHERE id=?1 AND worker_pid IS NULL",
            params![id.as_str()],
        )?;
        if n == 0 {
            return Err(Error::Invalid(
                "a reservation of a worker that was started cannot be released".into(),
            ));
        }
        Ok(())
    }

    /// The worker's sign of life (extends its lease). Refused for a stale fence, a superseded or
    /// finished attempt.
    pub fn heartbeat(&mut self, id: &AttemptId, fence: i64) -> Result<()> {
        self.live_attempt(id, fence)?;
        self.et.tx.execute(
            "UPDATE attempts SET heartbeat_at=?2 WHERE id=?1",
            params![id.as_str(), ts(self.now)],
        )?;
        Ok(())
    }

    // ---- decisions

    /// Creates a decision in `preparing`. The same `(key, binding)` returns the existing decision
    /// (`false`) while it is open or was answered; one that expired or was invalidated does not
    /// block a new one, and neither does a different binding. A stored decision of that key and
    /// binding with a different brief is an error, not a silent reuse.
    pub fn new_decision(&mut self, d: &NewDecision) -> Result<(DecisionRecord, bool)> {
        if d.kind.is_empty()
            || d.kind.len() > 64
            || d.dedupe_key.is_empty()
            || d.dedupe_key.len() > 128
            || !d.brief.is_object()
            || !d.binding.is_object()
        {
            return Err(Error::Invalid(
                "a decision needs a kind, a key, a brief and a binding".into(),
            ));
        }
        // the brief offers choices; it never pre-selects one (AC-10)
        if let Some(k) = [
            "selected",
            "recommended",
            "default",
            "default_choice",
            "preselected",
        ]
        .iter()
        .find(|k| d.brief.get(**k).is_some())
        {
            return Err(Error::Invalid(format!(
                "a decision brief has no {k:?} field: there is no default choice"
            )));
        }
        let expires = parse_ts(&d.expires_at)
            .filter(|t| *t > self.now
                    && *t <= self.now + time::Duration::hours(i64::from(self.policy.decision_expiry_hours)))
            .ok_or_else(|| {
                Error::Invalid(
                    "expires_at must be a store timestamp in the future, within the settings limit (decision_expiry_hours)".into(),
                )
            })?;
        let _ = expires;
        let (mut brief, mut binding) = (d.brief.clone(), d.binding.clone());
        // An `approval_needed` decision that offers `grant_once` must carry the exact rules it
        // would grant, and the label the person reads is DERIVED from them (not trusted as given).
        if d.kind == "approval_needed" && offers(&brief, "grant_once") {
            let rules: Vec<String> = d
                .action
                .as_ref()
                .and_then(|a| a.get("grant_rules"))
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(|r| r.as_str().map(str::to_owned))
                        .collect()
                })
                .unwrap_or_default();
            let listed = d
                .action
                .as_ref()
                .and_then(|a| a.get("grant_rules"))
                .and_then(Value::as_array)
                .map_or(0, Vec::len);
            if rules.is_empty()
                || rules.len() > 10
                || rules.len() != listed
                || !rules.iter().all(|r| valid_grant_rule(r))
            {
                return Err(Error::Invalid(
                    "a decision that offers grant_once needs exact grantable rules in its action"
                        .into(),
                ));
            }
            let label = format!("allow exactly {} in one new attempt", rules.join(", "));
            if let Some(alts) = brief.get_mut("alternatives").and_then(Value::as_array_mut) {
                for a in alts.iter_mut() {
                    if a.get("id").and_then(Value::as_str) == Some("grant_once")
                        && let Some(o) = a.as_object_mut()
                    {
                        o.insert("label".into(), Value::String(label.clone()));
                    }
                }
            }
        }
        if d.kind == "plan_approval" {
            self.check_plan_approval(d, &mut brief)?;
        }
        self.et.redact_value(&mut brief);
        self.et.redact_value(&mut binding);
        // the action's hash is the store's own: nobody can bind a decision to an action it does not hold
        let action_text = d.action.as_ref().map(|a| {
            let mut a = a.clone();
            self.et.redact_value(&mut a);
            a.to_string()
        });
        if let (Some(t), Some(b)) = (&action_text, binding.as_object_mut()) {
            b.insert("action_hash".into(), Value::String(sha_hex(t.as_bytes())));
        }
        let binding_hash = sha_hex(binding.to_string().as_bytes());
        let latest: Option<DecisionRecord> = self
            .et
            .tx
            .query_row(
                &format!(
                    "SELECT {DECISION_COLS} FROM decisions WHERE outcome_id=?1 AND dedupe_key=?2 \
                     AND binding_hash=?3 ORDER BY created_at DESC, rowid DESC LIMIT 1"
                ),
                params![self.id.as_str(), d.dedupe_key, binding_hash],
                decision_row,
            )
            .optional()?;
        if let Some(rec) = latest
            && !matches!(
                rec.row_state,
                DecisionState::Expired | DecisionState::Invalidated
            )
        {
            if rec.brief != brief || rec.kind != d.kind {
                return Err(Error::Invalid(format!(
                    "decision key {:?} already holds a different brief for this binding",
                    d.dedupe_key
                )));
            }
            return Ok((rec, false));
        }
        let id = DecisionId::parse(&new_uuid()).map_err(refuse)?;
        self.et.tx.execute(
            "INSERT INTO decisions(id,outcome_id,kind,dedupe_key,state,revision,brief,binding,binding_hash,expires_at,created_at,action) \
             VALUES (?1,?2,?3,?4,'preparing',1,?5,?6,?7,?8,?9,?10)",
            params![
                id.as_str(),
                self.id.as_str(),
                d.kind,
                d.dedupe_key,
                brief.to_string(),
                binding.to_string(),
                binding_hash,
                d.expires_at,
                ts(self.now),
                action_text
            ],
        )?;
        let rec = self.et.tx.query_row(
            &format!("SELECT {DECISION_COLS} FROM decisions WHERE id=?1"),
            [id.as_str()],
            decision_row,
        )?;
        self.et.append(
            ev::DECISION_CREATED,
            json!({"decision_id": rec.id, "kind": rec.kind, "binding_hash": rec.binding_hash,
                   "brief_sha256": rec.brief_sha256, "revision": 1}),
        )?;
        Ok((rec, true))
    }

    /// The decision of this outcome; a row whose binding no longer hashes to `binding_hash` was
    /// written by something else and is refused.
    fn load_decision(&self, id: &DecisionId) -> Result<DecisionRecord> {
        let (d, binding_text) = self
            .et
            .tx
            .query_row(
                &format!(
                    "SELECT {DECISION_COLS}, binding FROM decisions WHERE id=?1 AND outcome_id=?2"
                ),
                params![id.as_str(), self.id.as_str()],
                |r| Ok((decision_row(r)?, r.get::<_, String>(10)?)),
            )
            .optional()?
            .ok_or_else(|| Error::NotFound(format!("decision {id}")))?;
        if sha_hex(binding_text.as_bytes()) != d.binding_hash {
            return Err(Error::Invalid(format!(
                "decision {id} is inconsistent: its binding does not match its hash"
            )));
        }
        Ok(d)
    }

    /// Stores a grant for an approved decision whose binding is still current. The caller has
    /// already verified the approval and read the rules from the decision's stored action.
    #[allow(clippy::too_many_arguments)]
    fn insert_grant(
        &mut self,
        d: &DecisionRecord,
        current_binding: &Value,
        item: &WorkItemId,
        rules: &[String],
        ttl_hours: u32,
    ) -> Result<GrantRecord> {
        if !self.binding_current(d, current_binding) {
            return Err(Error::Stale(
                "the contract, plan, commit or policy changed since the decision was shown".into(),
            ));
        }
        let in_outcome: bool = self.et.tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM work_items WHERE id=?1 AND outcome_id=?2)",
            params![item.as_str(), self.id.as_str()],
            |r| r.get(0),
        )?;
        if !in_outcome {
            return Err(Error::NotFound(format!("work item {item}")));
        }
        let exists: bool = self.et.tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM grants WHERE decision_id=?1)",
            [d.id.as_str()],
            |r| r.get(0),
        )?;
        if exists {
            return Err(Error::Invalid(
                "a grant for this decision already exists".into(),
            ));
        }
        let action_hash = d
            .binding
            .get("action_hash")
            .and_then(Value::as_str)
            .ok_or_else(|| Error::Invalid("the decision binding has no action hash".into()))?
            .to_owned();
        let id = new_uuid();
        let (created, expires) = (
            ts(self.now),
            ts(self.now + time::Duration::hours(i64::from(ttl_hours))),
        );
        let rules_text = serde_json::to_string(rules)?;
        let m = mac(
            self.et.key(),
            "grant",
            &grant_parts(
                &id,
                d.id.as_str(),
                self.id.as_str(),
                item.as_str(),
                &rules_text,
                &action_hash,
                &expires,
                &created,
            ),
        );
        self.et.tx.execute(
            "INSERT INTO grants(id,decision_id,outcome_id,work_item_id,rules,action_hash,expires_at,created_at,mac) \
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)",
            params![id, d.id.as_str(), self.id.as_str(), item.as_str(), rules_text, action_hash, expires, created, m],
        )?;
        let payload = json!({"grant_id": id, "decision_id": d.id, "work_item_id": item,
                             "rules": rules, "action_hash": action_hash, "expires_at": expires});
        self.et.append(ev::GRANT_CREATED, payload.clone())?;
        self.et.append(
            ev::POLICY_DECISION,
            json!({"action": "grant.create", "result": "allow", "grant": payload}),
        )?;
        Ok(GrantRecord {
            id,
            decision_id: d.id.clone(),
            work_item_id: item.clone(),
            rules: rules.to_vec(),
            action_hash,
            expires_at: expires,
            created_at: created,
            consumed_at: None,
            consumed_by_attempt: None,
            revoked_at: None,
        })
    }

    fn binding_current(&self, d: &DecisionRecord, current: &Value) -> bool {
        let mut c = current.clone();
        self.et.redact_value(&mut c);
        sha_hex(c.to_string().as_bytes()) == d.binding_hash
    }

    fn expired(&self, d: &DecisionRecord) -> bool {
        parse_ts(&d.expires_at).is_none_or(|t| t <= self.now)
    }

    /// The orchestrator's own transitions: prepare (`pending`), `expired` once the stored expiry
    /// has passed, `invalidated` once `current_binding` differs from the stored one. A person's
    /// answer (`approved`, `rejected`, `needs_more_info`) is refused here: it goes through
    /// [`Self::resolve_decision`], the one entry point that checks the brief, the binding and
    /// the clock and records the approval.
    pub fn transition_decision(
        &mut self,
        id: &DecisionId,
        expected_revision: i64,
        to: DecisionState,
        current_binding: &Value,
        fx: &Effects,
    ) -> Result<Applied> {
        if matches!(
            to,
            DecisionState::Approved | DecisionState::Rejected | DecisionState::NeedsMoreInfo
        ) {
            return Err(Error::Invalid(
                "a decision is resolved by a person through resolve_decision".into(),
            ));
        }
        let d = self.load_decision(id)?;
        self.conflict_or(
            ev::Aggregate::Decision,
            id.as_str(),
            expected_revision,
            d.revision,
        )?;
        let guards = DecisionGuards {
            actor: Actor::Orchestrator,
            binding_current: self.binding_current(&d, current_binding),
            expiry_reached: self.expired(&d),
        };
        d.row_state.transition(to, &guards).map_err(refuse)?;
        let changed = self.et.tx.execute(
            "UPDATE decisions SET state=?2, revision=revision+1 WHERE id=?1 AND revision=?3",
            params![id.as_str(), enum_str(&to), d.revision],
        )?;
        if changed != 1 {
            return Err(conflict(
                "decision",
                id.as_str(),
                d.revision,
                d.revision + 1,
            ));
        }
        self.changed(
            ev::Aggregate::Decision,
            id.as_str(),
            d.revision + 1,
            &enum_str(&d.row_state),
            &enum_str(&to),
            None,
        )?;
        self.applied(d.revision + 1, fx)
    }

    /// The one way a decision is resolved (ADR-019 section 4). One transaction: the decision is
    /// at the revision the person saw; the brief hash they echo is the hash of the stored brief;
    /// the stored binding equals `current_binding`; the decision has not expired; the choice is
    /// offered by the brief (an alternative, `reject`, `request_evidence` or `edit_scope`; there
    /// is no default). Then the approval row (with its MAC), the state change and the chained
    /// `decision.resolved` event are written together. A mismatch of brief or binding is
    /// [`Error::Stale`], a lost revision race [`Error::RevisionConflict`].
    pub fn resolve_decision(
        &mut self,
        r: &Resolution,
        current_binding: &Value,
    ) -> Result<DecisionState> {
        let d = self.load_decision(&r.decision_id)?;
        self.conflict_or(
            ev::Aggregate::Decision,
            r.decision_id.as_str(),
            r.decision_revision,
            d.revision,
        )?;
        if r.brief_sha256 != d.brief_sha256 {
            return Err(Error::Stale("the brief changed since it was shown".into()));
        }
        if !self.binding_current(&d, current_binding) {
            return Err(Error::Stale(
                "the contract, plan, commit or policy changed since it was shown".into(),
            ));
        }
        if !valid_command_text(&r.approver, 128) {
            return Err(Error::Invalid(
                "approver must be 1 to 128 printable characters".into(),
            ));
        }
        // a plan has no work item to re-brief or scope to edit: its answers are the plans offered and `reject`
        if d.kind == "plan_approval"
            && matches!(r.choice.as_str(), "request_evidence" | "edit_scope")
        {
            return Err(Error::Invalid(
                "the choice is not offered by the brief".into(),
            ));
        }
        let to = match r.choice.as_str() {
            "reject" => DecisionState::Rejected,
            "request_evidence" | "edit_scope" => DecisionState::NeedsMoreInfo,
            c if offers(&d.brief, c) => DecisionState::Approved,
            _ => {
                return Err(Error::Invalid(
                    "the choice is not offered by the brief".into(),
                ));
            }
        };
        if r.choice == "request_evidence" && rebriefs_of(&d.brief) >= MAX_REBRIEFS {
            return Err(Error::Invalid(format!(
                "evidence was already requested {MAX_REBRIEFS} times for this decision: answer it or reject it"
            )));
        }
        let guards = DecisionGuards {
            actor: Actor::User,
            binding_current: true,
            expiry_reached: self.expired(&d),
        };
        d.row_state.transition(to, &guards).map_err(refuse)?;
        let at = ts(self.now);
        let m = approval_mac(
            self.et.key(),
            &ApprovalFacts {
                decision: r.decision_id.as_str(),
                outcome: self.id.as_str(),
                kind: &d.kind,
                revision: d.revision,
                binding_hash: &d.binding_hash,
                expires_at: &d.expires_at,
                brief_sha256: &r.brief_sha256,
                approver: &r.approver,
                choice: &r.choice,
                at: &at,
            },
        );
        // the approval row first: the v5 trigger refuses a resolution without it
        self.et.tx.execute(
            "INSERT INTO decision_approvals(id,decision_id,decision_revision,approver,choice,brief_sha256,decided_at,mac) \
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8)",
            params![
                new_uuid(),
                r.decision_id.as_str(),
                d.revision,
                r.approver,
                r.choice,
                r.brief_sha256,
                at,
                m
            ],
        )?;
        let changed = self.et.tx.execute(
            "UPDATE decisions SET state=?2, revision=revision+1 WHERE id=?1 AND revision=?3",
            params![r.decision_id.as_str(), enum_str(&to), d.revision],
        )?;
        if changed != 1 {
            return Err(conflict(
                "decision",
                r.decision_id.as_str(),
                d.revision,
                d.revision + 1,
            ));
        }
        self.changed(
            ev::Aggregate::Decision,
            r.decision_id.as_str(),
            d.revision + 1,
            &enum_str(&d.row_state),
            &enum_str(&to),
            None,
        )?;
        self.et.append(
            ev::DECISION_RESOLVED,
            json!({"decision_id": r.decision_id, "outcome_id": self.id,
                   "revision": d.revision + 1, "binding_hash": d.binding_hash,
                   "approver": r.approver, "choice": r.choice,
                   "brief_sha256": r.brief_sha256, "state": to}),
        )?;
        Ok(to)
    }

    /// Re-briefs a decision the person answered with `request_evidence` (ADR-019 section 4a): one
    /// transaction closes the old row (`invalidated`) and inserts a NEW decision whose brief is the
    /// old one plus `supersedes`, `rebrief`, `rebriefs_left` and `evidence_summary` (server data);
    /// the alternatives, the stored action and so the binding (action hash included) are the old
    /// ones, and the expiry is carried over, never extended. The new brief is new content, so its
    /// hash differs and nothing the person said about the old one carries over.
    fn rebrief_decision(
        &mut self,
        id: &DecisionId,
        expected_revision: i64,
        current_binding: &Value,
        evidence: &Value,
        action: Option<Value>,
    ) -> Result<DecisionRecord> {
        let d = self.load_decision(id)?;
        self.conflict_or(
            ev::Aggregate::Decision,
            id.as_str(),
            expected_revision,
            d.revision,
        )?;
        let asked: bool = self.et.tx.query_row(
            "SELECT EXISTS (SELECT 1 FROM decision_approvals WHERE decision_id=?1 AND decision_revision=?2 \
               AND choice='request_evidence')",
            params![id.as_str(), d.revision - 1],
            |r| r.get(0),
        )?;
        if d.row_state != DecisionState::NeedsMoreInfo || !asked {
            return Err(Error::Stale(
                "the decision is not waiting for evidence".into(),
            ));
        }
        if !self.binding_current(&d, current_binding) || self.expired(&d) {
            return Err(Error::Stale(
                "the decision expired or its binding moved".into(),
            ));
        }
        let n = rebriefs_of(&d.brief);
        if n >= MAX_REBRIEFS || evidence.to_string().len() > 16 * 1024 {
            return Err(Error::Invalid(
                "the re-brief limit is reached or the evidence is too large".into(),
            ));
        }
        let mut brief = d.brief.clone();
        if let Some(o) = brief.as_object_mut() {
            o.insert("supersedes".into(), json!(id));
            o.insert("rebrief".into(), json!(n + 1));
            o.insert("rebriefs_left".into(), json!(MAX_REBRIEFS - n - 1));
            o.insert("evidence_summary".into(), evidence.clone());
        }
        // the old card is closed first: the open-decision index and the dedupe check then allow its successor.
        // A binding that differs from the stored one is what lets the orchestrator close it.
        self.transition_decision(
            id,
            expected_revision,
            DecisionState::Invalidated,
            &json!({"rebriefed": id}),
            &Effects::default(),
        )?;
        let (new, fresh) = self.new_decision(&NewDecision {
            kind: d.kind.clone(),
            dedupe_key: d.dedupe_key.clone(),
            brief,
            binding: d.binding.clone(),
            expires_at: d.expires_at.clone(),
            action,
        })?;
        if !fresh || new.binding_hash != d.binding_hash {
            return Err(Error::Invalid(
                "the re-brief must keep the stored action and binding".into(),
            ));
        }
        self.transition_decision(
            &new.id,
            new.revision,
            DecisionState::Pending,
            &new.binding,
            &Effects::default(),
        )?;
        self.et.append(
            ev::DECISION_REBRIEFED,
            json!({"decision_id": new.id, "supersedes": id, "outcome_id": self.id,
                   "rebrief": n + 1, "binding_hash": d.binding_hash}),
        )?;
        self.load_decision(&new.id)
    }

    // ---- evidence

    /// Inserts evidence. Refused: a trust class its producer cannot honestly hold (only the
    /// independent runner writes `independent_check`, only a person `human_review`), an attempt
    /// that is not this outcome's, a stale fence, and an artifact that is not stored with that
    /// hash. Idempotent on `(attempt, work item, check, commit)`: the same row again returns the
    /// stored id and `false`; the same key with different content is an error.
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
            || !valid_command_text(&e.check_id, 128)
            || e.subject_commit.is_empty()
            || e.subject_commit.len() > 128
            || e.criteria_ids.len() > 64
        {
            return Err(Error::Invalid(
                "evidence needs a result, a check and a commit".into(),
            ));
        }
        let mut flags = e.flags.clone();
        flags.sort();
        flags.dedup();
        if let Some(bad) = flags.iter().find(|f| !EVIDENCE_FLAGS.contains(&f.as_str())) {
            return Err(Error::Invalid(format!("unknown evidence flag {bad:?}")));
        }
        let flags_json = serde_json::to_string(&flags)?;
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
        let (art_session, art_id, sha) = match &e.artifact {
            Some((s, a, h)) => {
                if !valid_id(s) || !valid_id(a) || !is_sha256(h) {
                    return Err(Error::Invalid(
                        "evidence artifact reference is malformed".into(),
                    ));
                }
                // stored with that hash, in this outcome's control session or one of its attempts
                let ok: bool = self.et.tx.query_row(
                    "SELECT EXISTS (SELECT 1 FROM artifacts x WHERE x.id=?2 AND x.session_id=?1 AND x.sha256=?3 \
                       AND (x.session_id = (SELECT control_session_id FROM outcomes WHERE id=?4) \
                         OR EXISTS (SELECT 1 FROM attempts at JOIN work_items w ON w.id=at.work_item_id \
                                    WHERE at.session_id=x.session_id AND w.outcome_id=?4)))",
                    params![s, a, h, self.id.as_str()],
                    |r| r.get(0),
                )?;
                if !ok {
                    return Err(Error::Invalid(
                        "evidence artifact is not stored in this outcome with that hash".into(),
                    ));
                }
                (Some(s.as_str()), Some(a.as_str()), Some(h.as_str()))
            }
            None => (None, None, None),
        };
        let id = EvidenceId::parse(&new_uuid()).map_err(refuse)?;
        let criteria = serde_json::to_string(&e.criteria_ids)?;
        let captured = now();
        let trust = enum_str(&e.trust_class);
        let mac_text = {
            let attempt = e.attempt_id.as_ref().map_or("", AttemptId::as_str);
            let artifact = format!("{}|{}", art_session.unwrap_or(""), art_id.unwrap_or(""));
            let exit = e.exit_code.map(|c| c.to_string()).unwrap_or_default();
            let fence = e.fence.to_string();
            let m = EvMac {
                id: id.as_str(),
                outcome: self.id.as_str(),
                item: e.work_item_id.as_str(),
                attempt,
                trust: &trust,
                producer: &e.producer,
                commit: &e.subject_commit,
                check: &e.check_id,
                command_hash: e.command_hash.as_deref().unwrap_or(""),
                criteria: &criteria,
                result: &e.result,
                exit_code: &exit,
                env: &e.env_fingerprint,
                artifact: &artifact,
                sha: sha.unwrap_or(""),
                fence: &fence,
                captured_at: &captured,
                flags: &flags_json,
                kind: "check",
                tree: "",
            };
            mac(self.et.key(), "evidence", &m.parts())
        };
        let inserted = self.et.tx.execute(
            "INSERT INTO evidence(id,outcome_id,work_item_id,attempt_id,trust_class,producer,subject_commit,check_id,command_hash,\
               criteria_ids,result,exit_code,env_fingerprint,artifact_session_id,artifact_id,sha256,fence,captured_at,flags,mac) \
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19,?20) \
             ON CONFLICT DO NOTHING",
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
                criteria,
                e.result,
                e.exit_code,
                e.env_fingerprint,
                art_session,
                art_id,
                sha,
                e.fence,
                captured,
                flags_json,
                mac_text
            ],
        )? == 1;
        if !inserted {
            let (existing, same): (String, bool) = self.et.tx.query_row(
                "SELECT id, trust_class=?5 AND producer=?6 AND command_hash IS ?7 AND criteria_ids=?8 \
                   AND result=?9 AND exit_code IS ?10 AND env_fingerprint=?11 AND sha256 IS ?12 \
                   AND artifact_id IS ?13 AND fence=?14 AND flags=?15 \
                 FROM evidence WHERE COALESCE(attempt_id,'')=?1 AND work_item_id=?2 AND check_id=?3 AND subject_commit=?4",
                params![
                    e.attempt_id.as_ref().map_or("", AttemptId::as_str),
                    e.work_item_id.as_str(),
                    e.check_id,
                    e.subject_commit,
                    enum_str(&e.trust_class),
                    e.producer,
                    e.command_hash,
                    criteria,
                    e.result,
                    e.exit_code,
                    e.env_fingerprint,
                    sha,
                    art_id,
                    e.fence,
                    flags_json
                ],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )?;
            if !same {
                return Err(Error::Invalid(
                    "evidence for this check and commit is already recorded with different content"
                        .into(),
                ));
            }
            return Ok((EvidenceId::parse(&existing).map_err(refuse)?, false));
        }
        self.et.append(
            ev::EVIDENCE_RECORDED,
            json!({"evidence_id": id, "work_item_id": e.work_item_id, "check_id": e.check_id,
                   "trust_class": e.trust_class, "result": e.result, "sha256": sha,
                   "subject_commit": e.subject_commit, "fence": e.fence,
                   "command_hash": e.command_hash, "exit_code": e.exit_code,
                   "env_fingerprint": e.env_fingerprint, "criteria_ids": e.criteria_ids,
                   "flags": flags}),
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
            "UPDATE outcome_commands SET state=?2, result=?3, applied_at=?4 WHERE id=?1 AND outcome_id=?5 AND state='queued'",
            params![cmd.id.as_str(), enum_str(&state), result.to_string(), now(), self.id.as_str()],
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

// ------------------------------------------------------------------- model planning runs

/// Planning runs of one outcome (each is a session that costs money; the spend cap bounds them too).
pub const MAX_PLAN_RUNS: usize = 32;
/// The least a planning run may be given: below it the outcome has nothing left for planning.
pub const MIN_PLAN_RESERVE_USD: f64 = 0.01;
/// The only choices a `plan_approval` decision offers besides `reject`; each installs exactly one
/// plan of the decision's stored action.
pub const PLAN_CHOICES: [(&str, &str); 2] = [
    ("approve_model_plan", "plan"),
    ("use_stub_plan", "stub_plan"),
];
/// The answer of a replan decision that installs nothing: the current plan stays as it is. Offered
/// only when a plan of the current contract exists.
pub const KEEP_CURRENT_PLAN: &str = "keep_current_plan";

/// One planning run (ADR-017 P-B). `cost_usd` is `None` until settled; until then, and for a run
/// settled without a cost (a lost one), the reservation counts as spent.
#[derive(Debug, Clone, PartialEq)]
pub struct PlanRunRecord {
    pub id: String,
    pub contract_version: u32,
    pub base_commit: String,
    pub session_id: String,
    pub model: String,
    /// `running`, `finished`, `failed` or `lost`.
    pub state: String,
    pub reserved_usd: f64,
    pub cost_usd: Option<f64>,
    pub raw_sha256: Option<String>,
    pub plan_hash: Option<String>,
    pub error_code: Option<String>,
    pub started_at: String,
    pub ended_at: Option<String>,
}

/// How a planning run ended; the hashes are of the raw output and of the validated plan.
#[derive(Debug, Clone, PartialEq)]
pub struct PlanRunEnd {
    /// `finished`, `failed` or `lost`.
    pub state: String,
    /// The session's final cost; `None` keeps the reservation as the spend (a lost run).
    pub cost_usd: Option<f64>,
    pub raw_sha256: Option<String>,
    pub plan_hash: Option<String>,
    /// A fixed code (`ReasonCode`), never model text.
    pub error_code: Option<String>,
}

const PLAN_RUN_COLS: &str = "id,contract_version,base_commit,session_id,model,state,reserved_usd,cost_usd,raw_sha256,plan_hash,error_code,started_at,ended_at,mac";

fn plan_run_row(r: &Row<'_>) -> rusqlite::Result<(PlanRunRecord, String)> {
    Ok((
        PlanRunRecord {
            id: r.get(0)?,
            contract_version: r.get(1)?,
            base_commit: r.get(2)?,
            session_id: r.get(3)?,
            model: r.get(4)?,
            state: r.get(5)?,
            reserved_usd: r.get(6)?,
            cost_usd: r.get(7)?,
            raw_sha256: r.get(8)?,
            plan_hash: r.get(9)?,
            error_code: r.get(10)?,
            started_at: r.get(11)?,
            ended_at: r.get(12)?,
        },
        r.get(13)?,
    ))
}

/// The MAC of a run's identity: who, for what, with which reservation. Settlement columns are set
/// once by triggers and carry no MAC (like attempts).
fn plan_run_mac_ok(key: Option<&[u8; 32]>, outcome: &str, r: &PlanRunRecord, stored: &str) -> bool {
    let (v, usd) = (r.contract_version.to_string(), r.reserved_usd.to_string());
    key.is_some_and(|k| {
        mac_ok(
            k,
            "plan_run",
            &[
                &r.id,
                outcome,
                &v,
                &r.base_commit,
                &r.session_id,
                &r.model,
                &usd,
                &r.started_at,
            ],
            stored,
        )
    })
}

fn load_plan_runs(
    conn: &Connection,
    key: Option<&[u8; 32]>,
    id: &OutcomeId,
) -> Result<Vec<PlanRunRecord>> {
    let mut st = conn.prepare(&format!(
        "SELECT {PLAN_RUN_COLS} FROM plan_runs WHERE outcome_id=?1 ORDER BY started_at, id LIMIT {LIST_LIMIT}"
    ))?;
    let rows = st
        .query_map([id.as_str()], plan_run_row)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    rows.into_iter()
        .map(|(r, m)| {
            if plan_run_mac_ok(key, id.as_str(), &r, &m) {
                Ok(r)
            } else {
                Err(Error::Tampered(format!(
                    "plan run {}: its reservation does not verify",
                    r.id
                )))
            }
        })
        .collect()
}

impl Reader {
    /// The outcome's planning runs, oldest first. A row whose MAC does not verify is an error
    /// (`Tampered`), never a smaller spend.
    pub fn list_plan_runs(&self, id: &OutcomeId) -> Result<Vec<PlanRunRecord>> {
        need_v5(&self.conn)?;
        load_plan_runs(
            &self.conn,
            crate::trust::existing_chain_key(&self.home).as_ref(),
            id,
        )
    }
}

impl OutcomeTx<'_, '_> {
    /// Reserves the budget of a planning run BEFORE it starts: `min(max_usd, what the outcome cap
    /// has left)`, counted as spent from now on (a lost run keeps it). One transaction decides it,
    /// so attempts and planning runs together never reserve past the cap, in any order. Refused
    /// when: the outcome is not before a plan (no plan, blocked, rework), the current contract is
    /// not approved, a run is still going, the run limit is reached, a stored run does not verify,
    /// the session is not an ordinary child of the control session, or the cap has less than
    /// [`MIN_PLAN_RESERVE_USD`] left.
    pub fn reserve_plan_run(
        &mut self,
        session_id: &str,
        model: &str,
        max_usd: f64,
    ) -> Result<PlanRunRecord> {
        if !max_usd.is_finite() || max_usd <= 0.0 || !valid_command_text(model, 200) {
            return Err(Error::Invalid(
                "a planning run needs a positive budget and a model name of 1 to 200 printable characters".into(),
            ));
        }
        let o = self.outcome()?;
        if !matches!(
            o.state,
            OutcomeState::ContractPending | OutcomeState::Blocked | OutcomeState::ReworkRequired
        ) {
            return Err(Error::Invalid(format!(
                "a plan is not made while the outcome is {}",
                enum_str(&o.state)
            )));
        }
        if !contract_authentic(self.et.tx, Some(self.et.key()), self.id, o.contract_version)? {
            return Err(Error::Invalid(
                "planning needs the approved current contract".into(),
            ));
        }
        let runs = load_plan_runs(self.et.tx, Some(self.et.key()), self.id)?;
        if runs.len() >= MAX_PLAN_RUNS || runs.iter().any(|r| r.state == "running") {
            return Err(Error::Invalid(
                "a planning run is still going, or the limit of planning runs is reached".into(),
            ));
        }
        let control: String = self.et.tx.query_row(
            "SELECT control_session_id FROM outcomes WHERE id=?1",
            [self.id.as_str()],
            |r| r.get(0),
        )?;
        let parent: Option<(String, Option<String>)> = self
            .et
            .tx
            .query_row(
                "SELECT kind,parent_session_id FROM sessions WHERE id=?1",
                [session_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        if !matches!(parent, Some((k, Some(p))) if k == "chat" && p == control) {
            return Err(Error::Invalid(
                "a planning session must be an ordinary child of the control session".into(),
            ));
        }
        // the ceilings may have tightened since the contract was approved (as for an attempt)
        let contract = self.stored_contract(o.contract_version)?;
        self.policy.check(&contract)?;
        let cap = contract.envelope.budgets.max_cost_usd;
        let left = cap - spend(self.et.tx, self.id)?;
        // a NaN spend (a corrupt row) is not "enough left" either
        if left.is_nan() || left < MIN_PLAN_RESERVE_USD {
            return Err(Error::Invalid(
                "the outcome cost cap has too little left for a planning run".into(),
            ));
        }
        let usd = max_usd.min(left);
        let (id, started) = (new_uuid(), ts(self.now));
        let (v, usd_text) = (o.contract_version.to_string(), usd.to_string());
        let m = mac(
            self.et.key(),
            "plan_run",
            &[
                &id,
                self.id.as_str(),
                &v,
                &o.base_commit,
                session_id,
                model,
                &usd_text,
                &started,
            ],
        );
        self.et.tx.execute(
            "INSERT INTO plan_runs(id,outcome_id,contract_version,base_commit,session_id,model,state,reserved_usd,mac,started_at) \
             VALUES (?1,?2,?3,?4,?5,?6,'running',?7,?8,?9)",
            params![id, self.id.as_str(), o.contract_version, o.base_commit, session_id, model, usd, m, started],
        )?;
        self.et.append(
            ev::PLAN_RUN_RESERVED,
            json!({"plan_run_id": id, "session_id": session_id, "model": model,
                   "contract_version": o.contract_version, "reserved_usd": usd}),
        )?;
        let (rec, _) = self.et.tx.query_row(
            &format!("SELECT {PLAN_RUN_COLS} FROM plan_runs WHERE id=?1"),
            [id.as_str()],
            plan_run_row,
        )?;
        Ok(rec)
    }

    /// Settles a running planning run, once: its state, its final cost (replacing the
    /// reservation as the spend) and the hashes of the raw output and of the validated plan.
    pub fn settle_plan_run(&mut self, run_id: &str, end: &PlanRunEnd) -> Result<PlanRunRecord> {
        let sha = |h: &Option<String>| h.as_deref().is_none_or(is_sha256);
        if !matches!(end.state.as_str(), "finished" | "failed" | "lost")
            || end.cost_usd.is_some_and(|c| !c.is_finite() || c < 0.0)
            || !sha(&end.raw_sha256)
            || !sha(&end.plan_hash)
            || end
                .error_code
                .as_deref()
                .is_some_and(|c| !valid_command_text(c, 64))
        {
            return Err(Error::Invalid(
                "a planning run ends with a state, a cost and hashes".into(),
            ));
        }
        let n = self.et.tx.execute(
            "UPDATE plan_runs SET state=?3, cost_usd=?4, raw_sha256=?5, plan_hash=?6, error_code=?7, ended_at=?8 \
             WHERE id=?1 AND outcome_id=?2 AND state='running'",
            params![run_id, self.id.as_str(), end.state, end.cost_usd, end.raw_sha256, end.plan_hash, end.error_code, ts(self.now)],
        )?;
        if n != 1 {
            return Err(Error::Stale("the planning run is not running".into()));
        }
        self.et.append(
            ev::PLAN_RUN_SETTLED,
            json!({"plan_run_id": run_id, "state": end.state, "cost_usd": end.cost_usd,
                   "raw_sha256": end.raw_sha256, "plan_hash": end.plan_hash,
                   "error_code": end.error_code}),
        )?;
        let key = *self.et.key();
        load_plan_runs(self.et.tx, Some(&key), self.id)?
            .into_iter()
            .find(|r| r.id == run_id)
            .ok_or_else(|| Error::NotFound(format!("plan run {run_id}")))
    }

    /// The rules of a `plan_approval` decision, checked when it is created (ADR-019): it offers
    /// only the choices in [`PLAN_CHOICES`], each backed by a plan in the stored action that is
    /// valid for the current approved contract and is the next plan version; the label the person
    /// reads is DERIVED from that plan (not trusted as given). The action is the plan: nothing
    /// else a caller says can reach an install.
    fn check_plan_approval(&self, d: &NewDecision, brief: &mut Value) -> Result<()> {
        let bad = |why: &str| Error::Invalid(format!("a plan_approval decision {why}"));
        let action = d
            .action
            .as_ref()
            .ok_or_else(|| bad("needs a stored action"))?;
        let o = self.outcome()?;
        let contract = self.stored_contract(o.contract_version)?;
        let alts = brief
            .get_mut("alternatives")
            .and_then(Value::as_array_mut)
            .ok_or_else(|| bad("offers its choices as alternatives"))?;
        if alts.is_empty() {
            return Err(bad("offers at least one plan"));
        }
        for a in alts.iter_mut() {
            let id = a.get("id").and_then(Value::as_str).unwrap_or_default();
            if id == KEEP_CURRENT_PLAN {
                // nothing to install: it names the plan that stays, which must be a plan of this contract
                let stays = match o.plan_version {
                    Some(v) => self.plan_contract(v)?.version == o.contract_version,
                    None => false,
                };
                if !stays {
                    return Err(bad(
                        "offers keep_current_plan only with a current plan of the current contract",
                    ));
                }
                a["label"] = Value::String("keep the current plan; nothing changes".into());
                continue;
            }
            let field = PLAN_CHOICES
                .iter()
                .find(|(c, _)| *c == id)
                .map(|(_, f)| *f)
                .ok_or_else(|| {
                    bad("offers only approve_model_plan, use_stub_plan and keep_current_plan")
                })?;
            let plan: Plan = action
                .get(field)
                .cloned()
                .and_then(|p| serde_json::from_value(p).ok())
                .ok_or_else(|| bad("carries each offered plan in its action"))?;
            if plan.version != o.plan_version.unwrap_or(0) + 1
                || plan.contract_version != o.contract_version
                || validate_plan(&plan, &contract).is_err()
            {
                return Err(bad("offers only a valid next plan of the current contract"));
            }
            let hash = body_hash(&serde_json::to_value(&plan)?);
            let who = if field == "plan" {
                "the model's plan"
            } else {
                "the stub plan"
            };
            a["label"] = Value::String(format!(
                "install exactly {who} (hash {}, {} items)",
                &hash[..12],
                plan.items.len()
            ));
        }
        Ok(())
    }
}

impl OutcomeWriter {
    /// See [`OutcomeTx::reserve_plan_run`].
    pub fn reserve_plan_run(
        &mut self,
        session_id: &str,
        model: &str,
        max_usd: f64,
    ) -> Result<PlanRunRecord> {
        self.atomically(|tx| tx.reserve_plan_run(session_id, model, max_usd))
    }

    /// See [`OutcomeTx::settle_plan_run`].
    pub fn settle_plan_run(&mut self, run_id: &str, end: &PlanRunEnd) -> Result<PlanRunRecord> {
        self.atomically(|tx| tx.settle_plan_run(run_id, end))
    }

    /// Installs the plan a person approved: the ONLY way a model's plan reaches `plans`. The
    /// caller names the decision and nothing else. It must be a `plan_approval` a person approved
    /// with `approve_model_plan` or `use_stub_plan` (MAC verified), the plan is read from its
    /// stored action (checked against the binding's action hash, so an edited row is `Tampered`)
    /// and validated again against the current contract by `insert_plan`, and `current_binding`
    /// must still be the binding the person saw (contract, plan version, policy).
    pub fn install_plan_from_decision(
        &mut self,
        decision: &DecisionId,
        current_binding: &Value,
    ) -> Result<Vec<(String, WorkItemId)>> {
        let d = self
            .list_decisions(self.id())?
            .into_iter()
            .find(|x| x.id == *decision)
            .ok_or_else(|| Error::NotFound(format!("decision {decision}")))?;
        if d.kind != "plan_approval" {
            return Err(Error::Invalid(
                "only a plan_approval decision installs a plan".into(),
            ));
        }
        let choice = match self.verified_resolution(decision)? {
            Some(r) if r.state == DecisionState::Approved => r.choice,
            _ => {
                return Err(Error::Invalid(
                    "only a plan a person approved is installed".into(),
                ));
            }
        };
        let field = PLAN_CHOICES
            .iter()
            .find(|(c, _)| *c == choice)
            .map(|(_, f)| *f)
            .ok_or_else(|| Error::Invalid("the approved choice installs no plan".into()))?;
        let action = self
            .decision_action(decision)?
            .ok_or_else(|| Error::Invalid("the decision has no stored action".into()))?;
        let plan: Plan = action
            .get(field)
            .cloned()
            .ok_or_else(|| Error::Invalid("the stored action carries no such plan".into()))
            .and_then(|p| serde_json::from_value(p).map_err(refuse))?;
        let plan_hash = body_hash(&serde_json::to_value(&plan)?);
        self.atomically(|tx| {
            if !tx.binding_current(&d, current_binding) {
                return Err(Error::Stale(
                    "the contract, plan or policy changed since the decision was shown".into(),
                ));
            }
            let ids = tx.insert_plan(&plan)?;
            tx.emit(
                ev::PLAN_APPROVED,
                json!({"decision_id": d.id, "choice": choice, "plan_version": plan.version,
                       "plan_hash": plan_hash, "action_hash": d.binding.get("action_hash")}),
            )?;
            Ok(ids)
        })
    }
}
