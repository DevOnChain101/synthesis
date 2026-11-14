//! Versioned migrations (`PRAGMA user_version` + `schema_migrations`, session-store.md B.2/C.5).

use rusqlite::Connection;

use crate::error::{Error, Result};

/// Migration N is `MIGRATIONS[N-1]`. Never edit an applied entry; append a new one.
const MIGRATIONS: &[&str] = &[
    // v1: spec B.2 plus marked additions.
    r#"
CREATE TABLE schema_migrations(version INTEGER PRIMARY KEY, applied_at TEXT NOT NULL);
CREATE TABLE sessions(
  id TEXT PRIMARY KEY, status TEXT NOT NULL, workspace_id TEXT NOT NULL,
  workspace_root TEXT NOT NULL, cwd TEXT NOT NULL,
  additional_dirs TEXT NOT NULL DEFAULT '[]' CHECK(json_valid(additional_dirs)),
  git_worktree TEXT, model_config TEXT NOT NULL CHECK(json_valid(model_config)),
  permission_mode TEXT NOT NULL, permission_policy_id TEXT NOT NULL,
  budget TEXT NOT NULL CHECK(json_valid(budget)),
  usage TEXT NOT NULL DEFAULT '{}' CHECK(json_valid(usage)),
  event_cursor INTEGER NOT NULL DEFAULT 0, checkpoint_id TEXT, title TEXT,
  parent_session_id TEXT REFERENCES sessions(id), parent_tool_call_id TEXT,
  legacy_transcript_path TEXT,
  legacy_size INTEGER, legacy_mtime_ns INTEGER,   -- addition: C.3 change detection
  created_at TEXT NOT NULL, updated_at TEXT NOT NULL);
CREATE INDEX sessions_cwd ON sessions(cwd, updated_at);
CREATE TABLE tasks(id TEXT PRIMARY KEY, session_id TEXT NOT NULL REFERENCES sessions(id),
  request TEXT NOT NULL, acceptance_criteria TEXT NOT NULL DEFAULT '[]' CHECK(json_valid(acceptance_criteria)),
  status TEXT NOT NULL, reason TEXT, started_at TEXT NOT NULL, ended_at TEXT);
CREATE TABLE events(session_id TEXT NOT NULL REFERENCES sessions(id), seq INTEGER NOT NULL,
  v INTEGER NOT NULL, ts TEXT NOT NULL, task_id TEXT, turn_id TEXT, agent_id TEXT, type TEXT NOT NULL,
  payload TEXT CHECK(payload IS NULL OR json_valid(payload)), payload_ref TEXT,
  CHECK((payload IS NULL) <> (payload_ref IS NULL)),
  PRIMARY KEY(session_id, seq)) WITHOUT ROWID;
CREATE TABLE messages(session_id TEXT NOT NULL REFERENCES sessions(id), idx INTEGER NOT NULL,
  role TEXT NOT NULL, content TEXT NOT NULL CHECK(json_valid(content)),
  compacted INTEGER NOT NULL DEFAULT 0,
  meta TEXT NOT NULL DEFAULT '{}' CHECK(json_valid(meta)),   -- addition: Node line extras
  PRIMARY KEY(session_id, idx));
CREATE TABLE tool_calls(id TEXT NOT NULL, session_id TEXT NOT NULL REFERENCES sessions(id),
  turn_id TEXT NOT NULL, tool_name TEXT NOT NULL, input_ref TEXT, result_ref TEXT,
  status TEXT NOT NULL, decision TEXT, duration_ms INTEGER, PRIMARY KEY(session_id, id));
CREATE TABLE processes(id TEXT PRIMARY KEY, session_id TEXT NOT NULL REFERENCES sessions(id),
  name TEXT, pid INTEGER, pid_start TEXT,                    -- addition: pid-reuse guard
  cwd TEXT NOT NULL, command TEXT NOT NULL, started_at TEXT NOT NULL,
  ended_at TEXT, status TEXT NOT NULL, exit_code INTEGER, log_artifact_id TEXT, log_cursor TEXT);
CREATE INDEX processes_status ON processes(status);
CREATE TABLE browser_contexts(id TEXT PRIMARY KEY, session_id TEXT NOT NULL REFERENCES sessions(id),
  backend TEXT NOT NULL, profile_ref TEXT, status TEXT NOT NULL);
CREATE TABLE artifacts(id TEXT PRIMARY KEY, session_id TEXT NOT NULL REFERENCES sessions(id),
  kind TEXT NOT NULL, path TEXT NOT NULL, mime TEXT NOT NULL, size INTEGER NOT NULL,
  sha256 TEXT NOT NULL, retention_policy TEXT NOT NULL DEFAULT 'session',
  expires_at TEXT,                                           -- addition: retention_policy='ttl'
  created_at TEXT NOT NULL);
CREATE TABLE verification_reports(id TEXT PRIMARY KEY, task_id TEXT NOT NULL REFERENCES tasks(id),
  attempt INTEGER NOT NULL, status TEXT NOT NULL, report TEXT NOT NULL CHECK(json_valid(report)),
  created_at TEXT NOT NULL);
CREATE TABLE approvals(id TEXT PRIMARY KEY, session_id TEXT NOT NULL REFERENCES sessions(id),
  tool_call_id TEXT NOT NULL, dialog TEXT NOT NULL CHECK(json_valid(dialog)), decision TEXT NOT NULL,
  reviewer TEXT NOT NULL, requested_at TEXT NOT NULL, resolved_at TEXT);
CREATE TABLE checkpoints(                                    -- addition: named restore points
  id TEXT PRIMARY KEY, session_id TEXT NOT NULL REFERENCES sessions(id), name TEXT NOT NULL,
  event_seq INTEGER NOT NULL, message_count INTEGER NOT NULL,
  files TEXT NOT NULL CHECK(json_valid(files)), created_at TEXT NOT NULL,
  UNIQUE(session_id, name));
"#,
    // v2: `/close` hides a session from the lists; nothing is deleted and it stays resumable by id.
    "ALTER TABLE sessions ADD COLUMN closed_at TEXT;",
    // v3: governance. `events.prev` + `sessions.chain_head` hash-chain the events (verify with
    // `Reader::verify_chain`; NULL `prev` = written before the chain existed); `stop_requested_at`
    // is the out-of-band kill switch (`synt stop`); `git_info` is the worktree a session runs in
    // (opaque JSON of the caller) so a resume can re-pin it.
    r#"
ALTER TABLE events ADD COLUMN prev TEXT;
ALTER TABLE sessions ADD COLUMN chain_head TEXT;
ALTER TABLE sessions ADD COLUMN stop_requested_at TEXT;
ALTER TABLE sessions ADD COLUMN git_info TEXT;
"#,
    // v4: 1 = the session was created with the chain on, so a missing link or head is damage, not
    // history (a downgrade cannot be passed off as an old session).
    "ALTER TABLE sessions ADD COLUMN chained INTEGER NOT NULL DEFAULT 0;",
    // v5: outcome orchestration (ADR-016). OPT-IN: a default open stays at v4 (`DEFAULT_TARGET`);
    // only the explicit enable step migrates here, after a backup. An older binary refuses a v5
    // database (`SchemaTooNew`), which is the point: it must not write a store it cannot read.
    // Additive only: `sessions.kind` is the one change to an existing table.
    r#"
ALTER TABLE sessions ADD COLUMN kind TEXT NOT NULL DEFAULT 'chat';
CREATE TABLE outcomes(
  id TEXT PRIMARY KEY,
  workspace_id TEXT NOT NULL, repo_root TEXT NOT NULL, base_commit TEXT NOT NULL,
  control_session_id TEXT NOT NULL UNIQUE REFERENCES sessions(id),
  goal TEXT NOT NULL, state TEXT NOT NULL, revision INTEGER NOT NULL,
  contract_version INTEGER NOT NULL, plan_version INTEGER,
  created_at TEXT NOT NULL, updated_at TEXT NOT NULL);
CREATE TABLE contracts(
  outcome_id TEXT NOT NULL REFERENCES outcomes(id), version INTEGER NOT NULL,
  hash TEXT NOT NULL, body TEXT NOT NULL CHECK(json_valid(body)),
  approved_by TEXT, approved_at TEXT,
  approved_mac TEXT,   -- HMAC(chain key) over the approval; a bare UPDATE cannot forge it
  PRIMARY KEY(outcome_id, version));
CREATE TABLE plans(
  outcome_id TEXT NOT NULL REFERENCES outcomes(id), version INTEGER NOT NULL,
  contract_version INTEGER NOT NULL, hash TEXT NOT NULL,
  dag TEXT NOT NULL CHECK(json_valid(dag)),
  PRIMARY KEY(outcome_id, version));
CREATE TABLE work_items(
  id TEXT PRIMARY KEY, outcome_id TEXT NOT NULL REFERENCES outcomes(id),
  plan_version INTEGER NOT NULL, key TEXT NOT NULL, kind TEXT NOT NULL,
  spec TEXT NOT NULL CHECK(json_valid(spec)),
  state TEXT NOT NULL, revision INTEGER NOT NULL, retries_used INTEGER NOT NULL DEFAULT 0,
  UNIQUE(outcome_id, plan_version, key));
CREATE TABLE work_item_deps(
  work_item_id TEXT NOT NULL REFERENCES work_items(id),
  requires_id  TEXT NOT NULL REFERENCES work_items(id),
  PRIMARY KEY(work_item_id, requires_id));
CREATE TABLE attempts(
  id TEXT PRIMARY KEY, work_item_id TEXT NOT NULL REFERENCES work_items(id),
  ordinal INTEGER NOT NULL, session_id TEXT NOT NULL UNIQUE REFERENCES sessions(id),
  worker_kind TEXT NOT NULL,
  state TEXT NOT NULL, revision INTEGER NOT NULL, fence INTEGER NOT NULL,
  task_status TEXT, subject_commit TEXT, retained_reason TEXT,
  worker_pid INTEGER, worker_start TEXT, heartbeat_at TEXT,   -- the worker process and its lease
  reserved_usd REAL,   -- the cost cap allocated to this attempt: counts as spent until a final usage is recorded
  started_at TEXT NOT NULL, ended_at TEXT,
  UNIQUE(work_item_id, ordinal));
CREATE TABLE evidence(
  id TEXT PRIMARY KEY, outcome_id TEXT NOT NULL REFERENCES outcomes(id),
  work_item_id TEXT NOT NULL REFERENCES work_items(id), attempt_id TEXT REFERENCES attempts(id),
  kind TEXT NOT NULL DEFAULT 'check',
  trust_class TEXT NOT NULL, producer TEXT NOT NULL,
  subject_commit TEXT NOT NULL, subject_tree TEXT, check_id TEXT NOT NULL, command_hash TEXT,
  criteria_ids TEXT NOT NULL CHECK(json_valid(criteria_ids)),
  result TEXT NOT NULL, exit_code INTEGER, env_fingerprint TEXT NOT NULL,
  artifact_session_id TEXT, artifact_id TEXT, sha256 TEXT, flags TEXT NOT NULL DEFAULT '[]' CHECK(json_valid(flags)),
  fence INTEGER NOT NULL, captured_at TEXT NOT NULL);
-- NULL attempt ids (human reviews) dedupe too
CREATE UNIQUE INDEX evidence_dedupe ON evidence(COALESCE(attempt_id, ''), work_item_id, check_id, subject_commit);
CREATE TABLE decisions(
  id TEXT PRIMARY KEY, outcome_id TEXT NOT NULL REFERENCES outcomes(id),
  kind TEXT NOT NULL, dedupe_key TEXT NOT NULL,
  state TEXT NOT NULL, revision INTEGER NOT NULL,
  brief TEXT NOT NULL CHECK(json_valid(brief)),
  binding TEXT NOT NULL CHECK(json_valid(binding)),
  binding_hash TEXT NOT NULL,
  expires_at TEXT NOT NULL, created_at TEXT NOT NULL);
-- one open decision per (key, binding); a finished one does not block a new binding
CREATE UNIQUE INDEX decisions_open ON decisions(outcome_id, dedupe_key, binding_hash)
  WHERE state IN ('preparing','pending','needs_more_info');
CREATE TABLE decision_approvals(
  id TEXT PRIMARY KEY, decision_id TEXT NOT NULL REFERENCES decisions(id),
  decision_revision INTEGER NOT NULL, approver TEXT NOT NULL,
  choice TEXT NOT NULL, brief_sha256 TEXT NOT NULL,
  decided_at TEXT NOT NULL,
  mac TEXT NOT NULL,    -- HMAC over the row and the decision it resolves, checked before use
  UNIQUE(decision_id, decision_revision));
CREATE TABLE grants(
  id TEXT PRIMARY KEY, decision_id TEXT NOT NULL REFERENCES decisions(id),
  action_hash TEXT NOT NULL, expires_at TEXT NOT NULL, consumed_at TEXT);
CREATE TABLE outcome_commands(
  id TEXT PRIMARY KEY, outcome_id TEXT NOT NULL REFERENCES outcomes(id),
  idempotency_key TEXT NOT NULL, kind TEXT NOT NULL,
  params TEXT NOT NULL CHECK(json_valid(params)),
  expected_revision INTEGER,
  from_surface TEXT NOT NULL CHECK(from_surface IN ('cli','tui','superagent')),
  state TEXT NOT NULL CHECK(state IN ('queued','applied','stale','already_resolved','denied')),
  result TEXT CHECK(result IS NULL OR json_valid(result)),
  created_at TEXT NOT NULL, applied_at TEXT,
  UNIQUE(outcome_id, idempotency_key));
CREATE INDEX outcome_commands_queue ON outcome_commands(state, created_at);
CREATE TABLE outbox(
  id TEXT PRIMARY KEY, outcome_id TEXT NOT NULL REFERENCES outcomes(id),
  idempotency_key TEXT NOT NULL, kind TEXT NOT NULL,
  payload TEXT NOT NULL CHECK(json_valid(payload)),
  state TEXT NOT NULL CHECK(state IN ('pending','dispatching','done','failed')),
  tries INTEGER NOT NULL DEFAULT 0, next_at TEXT, receipt TEXT,
  UNIQUE(outcome_id, idempotency_key));
CREATE INDEX outbox_pending ON outbox(state, next_at);
CREATE INDEX work_items_state ON work_items(outcome_id, state);
-- What a person approved or was shown never changes afterwards. Triggers stop accidents and old
-- or careless writers. They are NOT a security boundary against a program running as the same user
-- outside the sandbox: `INSERT OR REPLACE` skips delete triggers (recursive_triggers is off) and a
-- writer that opens the file can drop a trigger. The MACs (contracts.approved_mac,
-- decision_approvals.mac, keyed under the per-user chain key stored next to the data) detect
-- corruption and writers that cannot read that key; they do not stop one that can. Evidence rows,
-- attempts and work-item columns carry no MAC. Outcome state is protected from agent attempts by
-- the OS sandbox (platform dependent), not by this file.
CREATE TRIGGER contracts_immutable BEFORE UPDATE OF outcome_id, version, hash, body ON contracts
  BEGIN SELECT RAISE(ABORT, 'contracts are immutable'); END;
CREATE TRIGGER contracts_approval_once BEFORE UPDATE OF approved_by, approved_at, approved_mac ON contracts
  WHEN OLD.approved_at IS NOT NULL OR NEW.approved_at IS NULL OR NEW.approved_by IS NULL OR NEW.approved_mac IS NULL
  BEGIN SELECT RAISE(ABORT, 'a contract is approved once, with its mac'); END;
CREATE TRIGGER contracts_no_delete BEFORE DELETE ON contracts
  BEGIN SELECT RAISE(ABORT, 'contracts are never deleted'); END;
CREATE TRIGGER plans_immutable BEFORE UPDATE OF outcome_id, version, contract_version, hash, dag ON plans
  BEGIN SELECT RAISE(ABORT, 'plans are immutable'); END;
CREATE TRIGGER plans_no_delete BEFORE DELETE ON plans
  BEGIN SELECT RAISE(ABORT, 'plans are never deleted'); END;
CREATE TRIGGER evidence_immutable BEFORE UPDATE OF id, outcome_id, work_item_id, attempt_id, kind,
    trust_class, producer, subject_commit, subject_tree, check_id, command_hash, criteria_ids, result,
    exit_code, env_fingerprint, artifact_session_id, artifact_id, sha256, fence, captured_at ON evidence
  BEGIN SELECT RAISE(ABORT, 'evidence is immutable (only flags change)'); END;
CREATE TRIGGER evidence_no_delete BEFORE DELETE ON evidence
  BEGIN SELECT RAISE(ABORT, 'evidence is never deleted'); END;
CREATE TRIGGER decisions_immutable BEFORE UPDATE OF id, outcome_id, kind, dedupe_key, brief, binding,
    binding_hash, expires_at, created_at ON decisions
  BEGIN SELECT RAISE(ABORT, 'a decision brief and binding are immutable'); END;
CREATE TRIGGER decisions_resolution_needs_approval BEFORE UPDATE OF state ON decisions
  WHEN NEW.state IN ('approved','rejected','needs_more_info')
   AND NOT EXISTS (SELECT 1 FROM decision_approvals a WHERE a.decision_id = OLD.id AND a.decision_revision = OLD.revision)
  BEGIN SELECT RAISE(ABORT, 'a resolution needs its approval row first'); END;
CREATE TRIGGER decisions_no_delete BEFORE DELETE ON decisions
  BEGIN SELECT RAISE(ABORT, 'decisions are never deleted'); END;
CREATE TRIGGER decision_approvals_no_update BEFORE UPDATE ON decision_approvals
  BEGIN SELECT RAISE(ABORT, 'approvals are append-only'); END;
CREATE TRIGGER decision_approvals_no_delete BEFORE DELETE ON decision_approvals
  BEGIN SELECT RAISE(ABORT, 'approvals are append-only'); END;
CREATE TRIGGER outcomes_immutable BEFORE UPDATE OF id, control_session_id, workspace_id, repo_root, base_commit, created_at ON outcomes
  BEGIN SELECT RAISE(ABORT, 'an outcome keeps its identity, workspace and base commit'); END;
CREATE TRIGGER outcomes_no_delete BEFORE DELETE ON outcomes
  BEGIN SELECT RAISE(ABORT, 'outcomes are never deleted'); END;
-- a state change is a compare-and-swap: it must bump the revision by one
CREATE TRIGGER outcomes_revision BEFORE UPDATE OF state ON outcomes
  WHEN NEW.revision <> OLD.revision + 1 BEGIN SELECT RAISE(ABORT, 'state change without a revision bump'); END;
CREATE TRIGGER work_items_revision BEFORE UPDATE OF state ON work_items
  WHEN NEW.revision <> OLD.revision + 1 BEGIN SELECT RAISE(ABORT, 'state change without a revision bump'); END;
CREATE TRIGGER attempts_revision BEFORE UPDATE OF state ON attempts
  WHEN NEW.revision <> OLD.revision + 1 BEGIN SELECT RAISE(ABORT, 'state change without a revision bump'); END;
CREATE TRIGGER decisions_revision BEFORE UPDATE OF state ON decisions
  WHEN NEW.revision <> OLD.revision + 1 BEGIN SELECT RAISE(ABORT, 'state change without a revision bump'); END;
-- a revision never goes backwards; a decision's moves exactly one step and only with its state
CREATE TRIGGER outcomes_revision_forward BEFORE UPDATE OF revision ON outcomes
  WHEN NEW.revision < OLD.revision BEGIN SELECT RAISE(ABORT, 'a revision never decreases'); END;
CREATE TRIGGER work_items_revision_forward BEFORE UPDATE OF revision ON work_items
  WHEN NEW.revision < OLD.revision BEGIN SELECT RAISE(ABORT, 'a revision never decreases'); END;
CREATE TRIGGER attempts_revision_forward BEFORE UPDATE OF revision ON attempts
  WHEN NEW.revision < OLD.revision BEGIN SELECT RAISE(ABORT, 'a revision never decreases'); END;
CREATE TRIGGER decisions_revision_step BEFORE UPDATE OF revision ON decisions
  WHEN NEW.revision <> OLD.revision + 1 OR NEW.state = OLD.state
  BEGIN SELECT RAISE(ABORT, 'a decision revision moves one step, with its state'); END;
-- a finished state is final, and a person's answer only comes from `pending`
CREATE TRIGGER outcomes_terminal BEFORE UPDATE OF state ON outcomes
  WHEN OLD.state IN ('completed','failed','cancelled') BEGIN SELECT RAISE(ABORT, 'a finished outcome does not change'); END;
CREATE TRIGGER work_items_terminal BEFORE UPDATE OF state ON work_items
  WHEN OLD.state IN ('succeeded','terminal_failure','cancelled') BEGIN SELECT RAISE(ABORT, 'a finished work item does not change'); END;
CREATE TRIGGER attempts_terminal BEFORE UPDATE OF state ON attempts
  WHEN OLD.state IN ('finished','lost','cancelled') BEGIN SELECT RAISE(ABORT, 'a finished attempt does not change'); END;
CREATE TRIGGER decisions_terminal BEFORE UPDATE OF state ON decisions
  WHEN OLD.state IN ('approved','rejected','expired','invalidated') BEGIN SELECT RAISE(ABORT, 'a finished decision does not change'); END;
CREATE TRIGGER decisions_answer_from_pending BEFORE UPDATE OF state ON decisions
  WHEN NEW.state IN ('approved','rejected','needs_more_info') AND OLD.state <> 'pending'
  BEGIN SELECT RAISE(ABORT, 'a decision is answered from pending only'); END;
-- what the plan fixed: items, their order, which attempt ran where
CREATE TRIGGER work_items_spec_immutable BEFORE UPDATE OF id, outcome_id, plan_version, key, kind, spec ON work_items
  BEGIN SELECT RAISE(ABORT, 'a work item spec is immutable'); END;
CREATE TRIGGER work_item_deps_no_update BEFORE UPDATE ON work_item_deps
  BEGIN SELECT RAISE(ABORT, 'dependencies are immutable'); END;
CREATE TRIGGER work_item_deps_no_delete BEFORE DELETE ON work_item_deps
  BEGIN SELECT RAISE(ABORT, 'dependencies are immutable'); END;
CREATE TRIGGER attempts_identity_immutable BEFORE UPDATE OF id, work_item_id, ordinal, session_id, worker_kind, fence, started_at ON attempts
  BEGIN SELECT RAISE(ABORT, 'an attempt keeps its identity and fence'); END;
CREATE TRIGGER attempts_commit_once BEFORE UPDATE OF subject_commit ON attempts
  WHEN OLD.subject_commit IS NOT NULL AND NEW.subject_commit IS NOT OLD.subject_commit
  BEGIN SELECT RAISE(ABORT, 'an attempt commit is set once'); END;
CREATE TRIGGER attempts_worker_once BEFORE UPDATE OF worker_pid, worker_start ON attempts
  WHEN OLD.worker_pid IS NOT NULL BEGIN SELECT RAISE(ABORT, 'a worker is recorded once'); END;
CREATE TRIGGER attempts_reserved_once BEFORE UPDATE OF reserved_usd ON attempts
  WHEN OLD.reserved_usd IS NOT NULL AND (NEW.reserved_usd IS NULL OR NEW.reserved_usd > OLD.reserved_usd)
  BEGIN SELECT RAISE(ABORT, 'a budget reservation is set once and can only be lowered'); END;
CREATE TRIGGER grants_consume_once BEFORE UPDATE OF consumed_at ON grants
  WHEN OLD.consumed_at IS NOT NULL BEGIN SELECT RAISE(ABORT, 'a grant is consumed once'); END;
-- a finished command keeps its request and result
CREATE TRIGGER commands_request_immutable BEFORE UPDATE OF outcome_id, idempotency_key, kind, params,
    expected_revision, from_surface, created_at ON outcome_commands
  BEGIN SELECT RAISE(ABORT, 'a command request is immutable'); END;
CREATE TRIGGER commands_finish_once BEFORE UPDATE ON outcome_commands WHEN OLD.state <> 'queued'
  BEGIN SELECT RAISE(ABORT, 'a finished command does not change'); END;
-- the revision of THIS DDL (1000 + revision); a v5 database made by an earlier draft lacks it
INSERT INTO schema_migrations(version, applied_at) VALUES (1004, 'outcome-ddl-rev-4');
"#,
];

/// What a default open migrates to: the schema every released binary knows (D19: opt-in).
pub const DEFAULT_TARGET: i64 = 4;
/// First version with the outcome tables.
pub const OUTCOMES: i64 = 5;
/// `schema_migrations.version` of the outcome DDL revision this build writes (1000 + revision).
/// Developer databases that enabled an earlier draft of v5 lack it; the outcome API refuses them
/// with `OutcomeSchemaOutdated` and `synt doctor` says to restore the pre-v5 backup.
pub const OUTCOME_DDL_MARKER: i64 = 1004;
/// Newest schema this build can read and write.
pub const LATEST: i64 = MIGRATIONS.len() as i64;

/// Brings the database up to `target` (never past what this build supports). A database that is
/// already at or above `target` is left alone (`Ok(false)`; `Ok(true)` = this call changed it), so a binary with the feature off keeps working on
/// a database another binary enabled.
pub fn migrate(conn: &mut Connection, now: &str, target: i64) -> Result<bool> {
    migrate_with(conn, now, LATEST, target)
}

/// [`migrate`] with the supported version as a parameter, so tests can play an older binary.
pub(crate) fn migrate_with(
    conn: &mut Connection,
    now: &str,
    supported: i64,
    target: i64,
) -> Result<bool> {
    let target = target.min(supported);
    let version = user_version(conn)?;
    if version > supported {
        return Err(Error::SchemaTooNew {
            found: version,
            supported,
        });
    }
    if version >= target {
        return Ok(false);
    }
    // IMMEDIATE: two processes opening a fresh DB serialise here; the loser re-reads the version.
    let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let version: i64 = tx.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    for (i, sql) in MIGRATIONS
        .iter()
        .enumerate()
        .skip(version as usize)
        .take((target - version).max(0) as usize)
    {
        tx.execute_batch(sql)?;
        tx.execute(
            "INSERT INTO schema_migrations(version, applied_at) VALUES (?1, ?2)",
            (i as i64 + 1, now),
        )?;
        tx.pragma_update(None, "user_version", i as i64 + 1)?;
    }
    tx.commit()?;
    Ok(true)
}

pub(crate) fn user_version(conn: &Connection) -> Result<i64> {
    Ok(conn.query_row("PRAGMA user_version", [], |r| r.get(0))?)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn db(dir: &tempfile::TempDir) -> Connection {
        Connection::open(dir.path().join("synt.db")).unwrap()
    }

    #[test]
    fn migrates_stepwise_to_the_target_and_never_past_it() {
        let d = tempfile::tempdir().unwrap();
        let mut c = db(&d);
        migrate(&mut c, "t", DEFAULT_TARGET).unwrap();
        assert_eq!(user_version(&c).unwrap(), 4);
        assert!(
            c.prepare("SELECT kind FROM sessions").is_err(),
            "v4 has no kind column"
        );
        assert!(c.prepare("SELECT 1 FROM outcomes").is_err());
        migrate(&mut c, "t", DEFAULT_TARGET).unwrap(); // idempotent
        migrate(&mut c, "t", OUTCOMES).unwrap();
        assert_eq!(user_version(&c).unwrap(), 5);
        c.prepare("SELECT kind FROM sessions").unwrap();
        c.prepare("SELECT 1 FROM outcomes").unwrap();
        let n: i64 = c
            .query_row("SELECT COUNT(*) FROM schema_migrations", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 6, "five migrations plus the outcome DDL revision marker");
        // a lower target on a newer database is a no-op: the feature-off binary keeps working
        migrate(&mut c, "t", DEFAULT_TARGET).unwrap();
        assert_eq!(user_version(&c).unwrap(), 5);
        // a target above what this build knows is clamped, not an error
        migrate(&mut c, "t", 99).unwrap();
        assert_eq!(user_version(&c).unwrap(), LATEST);
    }

    #[test]
    fn a_fresh_database_can_go_straight_to_v5() {
        let d = tempfile::tempdir().unwrap();
        let mut c = db(&d);
        migrate(&mut c, "t", OUTCOMES).unwrap();
        assert_eq!(user_version(&c).unwrap(), 5);
    }

    /// What an older released binary does with a v5 database: it supports 4 and refuses 5, with
    /// this exact text (documented in ADR-016 and docs/UNATTENDED.md).
    #[test]
    fn a_binary_that_supports_v4_refuses_v5_with_the_documented_message() {
        let d = tempfile::tempdir().unwrap();
        let mut c = db(&d);
        migrate(&mut c, "t", OUTCOMES).unwrap();
        let e = migrate_with(&mut c, "t", 4, 4).unwrap_err();
        assert!(matches!(
            e,
            Error::SchemaTooNew {
                found: 5,
                supported: 4
            }
        ));
        assert_eq!(
            e.to_string(),
            "database schema v5 is newer than this build supports (v4)"
        );
        assert_eq!(user_version(&c).unwrap(), 5, "refusing changes nothing");
    }

    #[test]
    fn the_v5_ddl_is_additive_and_leaves_existing_columns_alone() {
        let d = tempfile::tempdir().unwrap();
        let mut c = db(&d);
        migrate(&mut c, "t", DEFAULT_TARGET).unwrap();
        let cols = |c: &Connection| -> Vec<String> {
            c.prepare("SELECT name FROM pragma_table_info('sessions') ORDER BY cid")
                .unwrap()
                .query_map([], |r| r.get(0))
                .unwrap()
                .map(std::result::Result::unwrap)
                .collect()
        };
        let before = cols(&c);
        migrate(&mut c, "t", OUTCOMES).unwrap();
        let after = cols(&c);
        assert_eq!(after[..before.len()], before[..]);
        assert_eq!(after[before.len()..], ["kind"]);
    }
}
