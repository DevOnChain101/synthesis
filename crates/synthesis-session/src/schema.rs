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
  artifact_session_id TEXT, artifact_id TEXT, sha256 TEXT, flags TEXT NOT NULL DEFAULT '[]',
  fence INTEGER NOT NULL, captured_at TEXT NOT NULL,
  UNIQUE(attempt_id, check_id, subject_commit));
CREATE TABLE decisions(
  id TEXT PRIMARY KEY, outcome_id TEXT NOT NULL REFERENCES outcomes(id),
  kind TEXT NOT NULL, dedupe_key TEXT NOT NULL,
  state TEXT NOT NULL, revision INTEGER NOT NULL,
  brief TEXT NOT NULL CHECK(json_valid(brief)),
  binding TEXT NOT NULL CHECK(json_valid(binding)),
  expires_at TEXT NOT NULL, created_at TEXT NOT NULL,
  UNIQUE(outcome_id, dedupe_key));
CREATE TABLE decision_approvals(
  id TEXT PRIMARY KEY, decision_id TEXT NOT NULL REFERENCES decisions(id),
  decision_revision INTEGER NOT NULL, approver TEXT NOT NULL,
  choice TEXT NOT NULL, brief_sha256 TEXT NOT NULL,
  decided_at TEXT NOT NULL);
CREATE TABLE grants(
  id TEXT PRIMARY KEY, decision_id TEXT NOT NULL REFERENCES decisions(id),
  action_hash TEXT NOT NULL, expires_at TEXT NOT NULL, consumed_at TEXT);
CREATE TABLE outcome_commands(
  id TEXT PRIMARY KEY, outcome_id TEXT NOT NULL REFERENCES outcomes(id),
  idempotency_key TEXT NOT NULL UNIQUE, kind TEXT NOT NULL,
  params TEXT NOT NULL CHECK(json_valid(params)),
  expected_revision INTEGER,
  from_surface TEXT NOT NULL CHECK(from_surface IN ('cli','tui','superagent')),
  state TEXT NOT NULL CHECK(state IN ('queued','applied','stale','already_resolved','denied')),
  result TEXT CHECK(result IS NULL OR json_valid(result)),
  created_at TEXT NOT NULL, applied_at TEXT);
CREATE INDEX outcome_commands_queue ON outcome_commands(state, created_at);
CREATE TABLE outbox(
  id TEXT PRIMARY KEY, outcome_id TEXT NOT NULL REFERENCES outcomes(id),
  idempotency_key TEXT NOT NULL UNIQUE, kind TEXT NOT NULL,
  payload TEXT NOT NULL CHECK(json_valid(payload)),
  state TEXT NOT NULL CHECK(state IN ('pending','dispatching','done','failed')),
  tries INTEGER NOT NULL DEFAULT 0, next_at TEXT, receipt TEXT);
CREATE INDEX outbox_pending ON outbox(state, next_at);
CREATE INDEX work_items_state ON work_items(outcome_id, state);
-- what a person approved or was shown never changes afterwards
CREATE TRIGGER contracts_immutable BEFORE UPDATE OF outcome_id, version, hash, body ON contracts
  BEGIN SELECT RAISE(ABORT, 'contracts are immutable'); END;
CREATE TRIGGER contracts_approval_once BEFORE UPDATE OF approved_by, approved_at ON contracts
  WHEN OLD.approved_at IS NOT NULL BEGIN SELECT RAISE(ABORT, 'a contract is approved once'); END;
CREATE TRIGGER plans_immutable BEFORE UPDATE OF outcome_id, version, contract_version, hash, dag ON plans
  BEGIN SELECT RAISE(ABORT, 'plans are immutable'); END;
CREATE TRIGGER decision_approvals_no_update BEFORE UPDATE ON decision_approvals
  BEGIN SELECT RAISE(ABORT, 'approvals are append-only'); END;
CREATE TRIGGER decision_approvals_no_delete BEFORE DELETE ON decision_approvals
  BEGIN SELECT RAISE(ABORT, 'approvals are append-only'); END;
"#,
];

/// What a default open migrates to: the schema every released binary knows (D19: opt-in).
pub const DEFAULT_TARGET: i64 = 4;
/// First version with the outcome tables.
pub const OUTCOMES: i64 = 5;
/// Newest schema this build can read and write.
pub const LATEST: i64 = MIGRATIONS.len() as i64;

/// Brings the database up to `target` (never past what this build supports). A database that is
/// already at or above `target` is left alone, so a binary with the feature off keeps working on
/// a database another binary enabled.
pub fn migrate(conn: &mut Connection, now: &str, target: i64) -> Result<()> {
    migrate_with(conn, now, LATEST, target)
}

/// [`migrate`] with the supported version as a parameter, so tests can play an older binary.
pub(crate) fn migrate_with(
    conn: &mut Connection,
    now: &str,
    supported: i64,
    target: i64,
) -> Result<()> {
    let target = target.min(supported);
    let version = user_version(conn)?;
    if version > supported {
        return Err(Error::SchemaTooNew {
            found: version,
            supported,
        });
    }
    if version >= target {
        return Ok(());
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
    Ok(())
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
        assert_eq!(n, 5);
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
