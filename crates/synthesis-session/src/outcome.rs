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
    Actor, AttemptId, AttemptState, CommandId, Contract, DecisionGuards, DecisionId, DecisionState,
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
    pub started_at: String,
    pub ended_at: Option<String>,
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

    pub fn list_work_items(&self, id: &OutcomeId) -> Result<Vec<WorkItemRecord>> {
        need_v5(&self.conn)?;
        let mut st = self.conn.prepare(&format!(
            "SELECT {ITEM_COLS} FROM work_items WHERE outcome_id=?1 ORDER BY plan_version, key LIMIT {LIST_LIMIT}"
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

    pub fn list_evidence(&self, id: &OutcomeId) -> Result<Vec<EvidenceRecord>> {
        need_v5(&self.conn)?;
        let mut st = self.conn.prepare(&format!(
            "SELECT {EVIDENCE_COLS} FROM evidence WHERE outcome_id=?1 ORDER BY captured_at, id LIMIT {LIST_LIMIT}"
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
}

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

    pub fn new_decision(&mut self, d: &NewDecision) -> Result<(DecisionRecord, bool)> {
        self.atomically(|tx| tx.new_decision(d))
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

    pub fn record_evidence(&mut self, e: &NewEvidence) -> Result<(EvidenceId, bool)> {
        self.atomically(|tx| tx.record_evidence(e))
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
    pub fn claim_outbox(&mut self, now: OffsetDateTime) -> Result<Option<OutboxRecord>> {
        let at = ts(now);
        self.atomically(|tx| {
            for (id, row) in due_pending(tx.et.tx, tx.id, &at)? {
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
        effect: &mut dyn OutboxEffect,
    ) -> Result<OutboxRun> {
        let mut run = OutboxRun::default();
        let outcome = self.id.clone();
        let (stuck, skipped) = dispatching_rows(&self.w.conn, &outcome)?;
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

type LenientRow = (String, std::result::Result<OutboxRecord, String>);

fn lenient(r: &Row<'_>) -> rusqlite::Result<LenientRow> {
    let id: String = r.get(0)?;
    Ok((id.clone(), outbox_row(r).map_err(|e| format!("{id}: {e}"))))
}

/// Due `pending` rows, oldest first; a row that does not parse comes back as `Err`.
fn due_pending(conn: &Connection, outcome: &OutcomeId, at: &str) -> Result<Vec<LenientRow>> {
    let mut st = conn.prepare(&format!(
        "SELECT {OUTBOX_COLS} FROM outbox WHERE outcome_id=?1 AND state='pending' \
         AND (next_at IS NULL OR next_at<=?2) ORDER BY rowid LIMIT 50"
    ))?;
    Ok(st
        .query_map(params![outcome.as_str(), at], lenient)?
        .collect::<rusqlite::Result<_>>()?)
}

/// Rows left `dispatching`, and a diagnostic for each one that could not be read.
fn dispatching_rows(
    conn: &Connection,
    outcome: &OutcomeId,
) -> Result<(Vec<OutboxRecord>, Vec<String>)> {
    let mut st = conn.prepare(&format!(
        "SELECT {OUTBOX_COLS} FROM outbox WHERE outcome_id=?1 AND state='dispatching' \
         ORDER BY rowid LIMIT {LIST_LIMIT}"
    ))?;
    let (mut rows, mut skipped) = (vec![], vec![]);
    for r in st.query_map([outcome.as_str()], lenient)? {
        match r?.1 {
            Ok(row) => rows.push(row),
            Err(why) => skipped.push(format!("outbox row skipped: {why}")),
        }
    }
    Ok((rows, skipped))
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

const FLAG_FREE: &str =
    "NOT EXISTS (SELECT 1 FROM json_each(e.flags) f WHERE f.value='definitions_changed')";
/// The evidence is for the newest attempt of its item (a late one from a superseded attempt
/// does not count).
const FENCE_CURRENT: &str = "e.fence >= COALESCE((SELECT MAX(a.fence) FROM attempts a WHERE a.work_item_id=e.work_item_id),0)";

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
        Ok(())
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
        Ok(self.et.tx.query_row(
            &format!(
                "SELECT EXISTS (SELECT 1 FROM evidence e WHERE e.work_item_id=?1 \
                   AND e.trust_class='independent_check' AND e.result='passed' \
                   AND e.subject_commit=?2 AND {FLAG_FREE} AND {FENCE_CURRENT})"
            ),
            params![item.as_str(), commit],
            |r| r.get(0),
        )?)
    }

    /// **Do not enable outcomes for real attempts before PR-08 and PR-09.** This MVP verdict reads
    /// unsigned evidence rows (a writer without the key can INSERT a passing `independent_check`
    /// row), nothing produces `definitions_changed` yet, the evidence `command_hash` is not bound to
    /// a frozen command, and "the attempt that ended last" is unsound for parallel branches. PR-09
    /// replaces it with a verdict over signed `evidence.recorded` events.
    ///
    /// The verdict (ADR-020 section 4, MVP rule), computed from the rows (a criterion with
    /// `check_ids` is only satisfied by evidence of one of those checks): every item of the
    /// current plan `succeeded`, and every mandatory criterion has evidence of the class it
    /// requires (`independent_check`, or `human_review` for a manual one), passed, for the
    /// candidate commit, from the current fence, without `definitions_changed`, and no failed
    /// evidence of that class for the same commit. The pure verdict function of PR-09 replaces this.
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
        for c in contract.criteria.iter().filter(|c| c.mandatory) {
            let class = if c.manual {
                "human_review"
            } else {
                "independent_check"
            };
            let check_ids = serde_json::to_string(&c.check_ids)?;
            let any = |result: &str| -> Result<bool> {
                Ok(self.et.tx.query_row(
                    &format!(
                        "SELECT EXISTS (SELECT 1 FROM evidence e WHERE e.outcome_id=?1 \
                           AND e.subject_commit=?2 AND e.trust_class=?3 AND e.result IN ({result}) \
                           AND EXISTS (SELECT 1 FROM json_each(e.criteria_ids) j WHERE j.value=?4) \
                           AND (?5='[]' OR e.check_id IN (SELECT value FROM json_each(?5))) \
                           AND {FLAG_FREE} AND {FENCE_CURRENT})"
                    ),
                    params![self.id.as_str(), commit, class, c.id, check_ids],
                    |r| r.get(0),
                )?)
            };
            if any("'failed','error'")? || !any("'passed'")? {
                return Ok(false);
            }
        }
        Ok(true)
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
        self.et.redact_value(&mut brief);
        self.et.redact_value(&mut binding);
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
            "INSERT INTO decisions(id,outcome_id,kind,dedupe_key,state,revision,brief,binding,binding_hash,expires_at,created_at) \
             VALUES (?1,?2,?3,?4,'preparing',1,?5,?6,?7,?8,?9)",
            params![
                id.as_str(),
                self.id.as_str(),
                d.kind,
                d.dedupe_key,
                brief.to_string(),
                binding.to_string(),
                binding_hash,
                d.expires_at,
                now()
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
        let inserted = self.et.tx.execute(
            "INSERT INTO evidence(id,outcome_id,work_item_id,attempt_id,trust_class,producer,subject_commit,check_id,command_hash,\
               criteria_ids,result,exit_code,env_fingerprint,artifact_session_id,artifact_id,sha256,fence,captured_at) \
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18) \
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
                now()
            ],
        )? == 1;
        if !inserted {
            let (existing, same): (String, bool) = self.et.tx.query_row(
                "SELECT id, trust_class=?5 AND producer=?6 AND command_hash IS ?7 AND criteria_ids=?8 \
                   AND result=?9 AND exit_code IS ?10 AND env_fingerprint=?11 AND sha256 IS ?12 \
                   AND artifact_id IS ?13 AND fence=?14 \
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
                    e.fence
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
