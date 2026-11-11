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
];

pub const LATEST: i64 = MIGRATIONS.len() as i64;

pub fn migrate(conn: &mut Connection, now: &str) -> Result<()> {
    let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    if version > LATEST {
        return Err(Error::SchemaTooNew {
            found: version,
            supported: LATEST,
        });
    }
    if version == LATEST {
        return Ok(());
    }
    // IMMEDIATE: two processes opening a fresh DB serialise here; the loser re-reads the version.
    let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let version: i64 = tx.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    for (i, sql) in MIGRATIONS.iter().enumerate().skip(version as usize) {
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
