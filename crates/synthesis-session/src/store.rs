//! SQLite session store (session-store.md B): one writer per session, any number of readers.
//!
//! File modes (unix): the synthesis home and every directory we create are 0700; the database, its
//! WAL/SHM, the session lock, artifacts and Node mirrors are 0600. On Windows these are not set
//! by the code: files inherit the ACL of the user-profile directory that holds `synthesis_home`
//! (unverified on a real Windows machine). Artifacts are created with `create_new`, so a
//! planted symlink is never written through.

use std::collections::HashSet;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::ops::Deref;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use rusqlite::{
    Connection, OpenFlags, OptionalExtension, Transaction, TransactionBehavior, params,
};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use synthesis_core::{
    BudgetUsage, ParentRef, PathPolicy, PermissionMode, SessionStatus, WorkspaceContext, contains,
    lexical_normalize, realpath_lenient,
};
use synthesis_events::{
    ArtifactRef, Budget, Criterion, Envelope, Event, RawEnvelope, Record, Redactor, SCHEMA_VERSION,
    TaskStatus,
};
use time::OffsetDateTime;

use crate::error::{Error, Result};
use crate::{node, schema};

/// Redacted event payloads above this go to an artifact (events.md §5, session-store B.2).
pub const MAX_INLINE_PAYLOAD: usize = 8 * 1024;
/// Hard caps; larger input is rejected, never truncated silently.
pub const MAX_PAYLOAD_BYTES: usize = 16 * 1024 * 1024;
pub const MAX_MESSAGE_BYTES: usize = 8 * 1024 * 1024;
pub const MAX_ARTIFACT_BYTES: usize = 256 * 1024 * 1024;
pub const MAX_IMPORT_BYTES: u64 = 256 * 1024 * 1024;
const MAX_SNAPSHOT_BYTES: u64 = 64 * 1024 * 1024;

const RUNNING_STATES: [SessionStatus; 3] = [
    SessionStatus::Running,
    SessionStatus::WaitingApproval,
    SessionStatus::Verifying,
];

pub(crate) fn now() -> String {
    let fmt = time::macros::format_description!(
        "[year]-[month]-[day]T[hour]:[minute]:[second].[subsecond digits:3]Z"
    );
    OffsetDateTime::now_utc()
        .format(&fmt)
        .expect("format utc timestamp")
}

fn enum_str<T: Serialize>(v: &T) -> String {
    match serde_json::to_value(v) {
        Ok(Value::String(s)) => s,
        _ => unreachable!("unit-variant enum serialises to a string"),
    }
}
fn enum_from<T: DeserializeOwned>(s: &str) -> Result<T> {
    Ok(serde_json::from_value(Value::String(s.to_string()))?)
}

/// Ids end up in file names (`sessions/<id>/`, `<id>.jsonl`): no separators, no dots.
pub fn valid_id(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 128
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}
fn check_id(s: &str) -> Result<()> {
    if valid_id(s) {
        Ok(())
    } else {
        Err(Error::InvalidId(s.to_string()))
    }
}

fn utf8(p: &Path) -> Result<String> {
    p.to_str()
        .map(str::to_string)
        .ok_or_else(|| Error::Invalid(format!("non-UTF-8 path {}", p.display())))
}

/// Directory (and missing parents) created 0700 on unix; a symlink in its place is refused.
fn private_dir(p: &Path) -> Result<()> {
    let mut b = fs::DirBuilder::new();
    b.recursive(true);
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut b, 0o700);
    b.create(p)?;
    if fs::symlink_metadata(p)?.file_type().is_symlink() {
        return Err(Error::Invalid(format!("{} is a symlink", p.display())));
    }
    Ok(())
}

/// 0600 on unix (the umask can only narrow it).
fn private_file(o: &mut OpenOptions) -> &mut OpenOptions {
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(o, 0o600);
    o
}

#[cfg(unix)]
fn chmod(p: &Path, mode: u32) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    Ok(fs::set_permissions(p, fs::Permissions::from_mode(mode))?)
}

/// Creates `synt.db` 0600 before SQLite does (it gives -wal/-shm the same mode); tightens an
/// existing one; refuses a symlink.
fn ensure_db_file(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(m) if m.file_type().is_symlink() => {
            Err(Error::Invalid(format!("{} is a symlink", path.display())))
        }
        Ok(_) => {
            #[cfg(unix)]
            chmod(path, 0o600)?;
            Ok(())
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            match private_file(OpenOptions::new().write(true).create_new(true)).open(path) {
                Ok(_) => Ok(()),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
                Err(e) => Err(e.into()),
            }
        }
        Err(e) => Err(e.into()),
    }
}

fn is_busy(e: &Error) -> bool {
    matches!(
        e,
        Error::Sqlite(rusqlite::Error::SqliteFailure(f, _))
            if matches!(f.code, rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked)
    )
}

/// Runs `f` again while it fails with a busy/locked database, for at most `limit`.
fn retry_busy<T>(limit: Duration, mut f: impl FnMut() -> Result<T>) -> Result<T> {
    let start = std::time::Instant::now();
    loop {
        match f() {
            Err(e) if is_busy(&e) && start.elapsed() < limit => {
                std::thread::sleep(Duration::from_millis(25));
            }
            r => return r,
        }
    }
}

#[cfg(test)]
mod retry_tests {
    use super::*;

    fn busy() -> Error {
        Error::Sqlite(rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_BUSY),
            None,
        ))
    }

    #[test]
    fn busy_is_retried_until_it_works_and_other_errors_are_not() {
        let mut n = 0;
        let r = retry_busy(Duration::from_secs(5), || {
            n += 1;
            if n < 3 { Err(busy()) } else { Ok(n) }
        });
        assert_eq!(r.unwrap(), 3);
        let mut m = 0;
        let r: Result<()> = retry_busy(Duration::from_secs(5), || {
            m += 1;
            Err(Error::SchemaTooNew {
                found: 9,
                supported: 1,
            })
        });
        assert!(r.is_err() && m == 1);
        let r: Result<()> = retry_busy(Duration::from_millis(60), || Err(busy()));
        assert!(is_busy(&r.unwrap_err()), "gives up after the limit");
    }
}

/// One connection, set up with the busy timeout first. SQLite answers "locked" at once to some
/// steps (journal-mode switch, WAL recovery on first open) without consulting the timeout, so the
/// whole setup is retried while busy/locked, for up to 15 s (slow Windows runners).
pub(crate) fn connect(home: &Path, read_only: bool) -> Result<Connection> {
    retry_busy(Duration::from_secs(15), || connect_once(home, read_only))
}

fn connect_once(home: &Path, read_only: bool) -> Result<Connection> {
    let path = home.join("synt.db");
    let conn = if read_only {
        Connection::open_with_flags(
            &path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?
    } else {
        Connection::open(&path)?
    };
    conn.busy_timeout(Duration::from_millis(5000))?;
    conn.pragma_update(None, "foreign_keys", true)?;
    if !read_only {
        let mode: String = conn.query_row("PRAGMA journal_mode", [], |r| r.get(0))?;
        if !mode.eq_ignore_ascii_case("wal") {
            conn.pragma_update(None, "journal_mode", "WAL")?;
        }
    }
    Ok(conn)
}

#[derive(Debug, Clone)]
pub struct StoreOptions {
    /// `compat.write_node_transcripts` (default ON until R9).
    pub write_node_transcripts: bool,
    /// Exact secret values masked in everything persisted (on top of the built-in patterns).
    pub secrets: Vec<String>,
    /// Checkpoint snapshots go through the redactor and secret-file files (`.env*`, keys,
    /// credentials) are replaced by a placeholder; default ON. Turn off for byte-exact snapshots.
    pub redact_snapshots: bool,
    /// Per-session caps. Rows are a hard cap (seq stays gapless, so rows are never deleted);
    /// the byte cap is checked every [`BYTE_CHECK_EVERY`] events and first reclaims space by
    /// collapsing old `model.delta` rows. `None` = unlimited.
    pub max_events_per_session: Option<u64>,
    pub max_event_bytes_per_session: Option<u64>,
    /// Completed-message deltas beyond the newest N are collapsed when the byte cap is hit.
    pub keep_recent_deltas: u64,
    /// Total artifact bytes per session (disk quota).
    pub max_artifact_bytes_per_session: Option<u64>,
}

/// Event byte accounting is a scan, so it runs once per this many appended events.
pub const BYTE_CHECK_EVERY: u64 = 128;
/// A collapsed delta batch stays well under [`MAX_INLINE_PAYLOAD`].
const MAX_BATCH_TEXT: usize = 4 * 1024;

impl Default for StoreOptions {
    fn default() -> Self {
        Self {
            write_node_transcripts: true,
            secrets: Vec::new(),
            redact_snapshots: true,
            max_events_per_session: Some(1_000_000),
            max_event_bytes_per_session: Some(512 * 1024 * 1024),
            keep_recent_deltas: 1000,
            max_artifact_bytes_per_session: Some(4 * 1024 * 1024 * 1024),
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct Limits {
    max_events: Option<u64>,
    max_event_bytes: Option<u64>,
    keep_deltas: u64,
    max_artifact_bytes: Option<u64>,
    redact_snapshots: bool,
}

/// Entry point. Holds only the injected `synthesis_home`; no ambient env or cwd is read.
pub struct Store {
    home: PathBuf,
    mirror: bool,
    redactor: Arc<Redactor>,
    limits: Limits,
}

/// One local calendar day of [`Reader::usage_by_day`]; `day` counts days since 1970-01-01.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UsageDay {
    pub day: i64,
    pub tokens: u64,
    pub output: u64,
    pub messages: u64,
    pub tools: u64,
    pub agents: u64,
    pub sessions: u64,
    /// `tool.started` of `Bash`.
    pub commands: u64,
    /// `tool.started` of a file-writing tool (`Write`, `Edit`, `MultiEdit`, `NotebookEdit`).
    pub files: u64,
    /// `tool.started` of a `browser_*` tool.
    pub browser: u64,
    /// `tool.started` of an MCP tool (`mcp__server__tool`).
    pub mcp: u64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SessionRecord {
    pub id: String,
    pub status: SessionStatus,
    pub workspace_id: String,
    pub workspace_root: String,
    pub cwd: String,
    pub additional_dirs: Vec<String>,
    pub git_worktree: Option<String>,
    pub model_config_id: String,
    pub permission_mode: PermissionMode,
    pub permission_policy_id: String,
    pub budget: Budget,
    pub usage: BudgetUsage,
    pub event_cursor: u64,
    pub checkpoint_id: Option<String>,
    pub title: Option<String>,
    pub parent: Option<ParentRef>,
    pub legacy_transcript_path: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

impl SessionRecord {
    /// Rebuilds the persisted workspace (never the process cwd); fails if a directory is gone.
    pub fn open_workspace(
        &self,
        home: &Path,
    ) -> std::result::Result<WorkspaceContext, synthesis_core::ResolveError> {
        let dirs: Vec<PathBuf> = self.additional_dirs.iter().map(PathBuf::from).collect();
        let mut ws = WorkspaceContext::open(
            self.workspace_id.clone(),
            Path::new(&self.workspace_root),
            Path::new(&self.cwd),
            &dirs,
            home.join("sessions").join(&self.id),
            uuid::Uuid::new_v4().to_string(),
        )?;
        ws.git_worktree = self.git_worktree.as_ref().map(PathBuf::from);
        Ok(ws)
    }
}

pub struct NewSession<'a> {
    /// Generated (UUIDv4) when `None`.
    pub id: Option<String>,
    /// Explicit by design: nothing is inherited from the process or a previous session.
    pub workspace: &'a WorkspaceContext,
    pub model_config_id: String,
    pub permission_mode: PermissionMode,
    pub permission_policy_id: String,
    pub budget: Budget,
    pub parent: Option<ParentRef>,
    pub title: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    User,
    Assistant,
    System,
}

impl Role {
    fn as_str(self) -> &'static str {
        match self {
            Role::User => "user",
            Role::Assistant => "assistant",
            Role::System => "system",
        }
    }
}

/// One provider-visible message. `top` / `msg` hold the Node line extras (`displayText`,
/// `isMeta`, `synthetic`, ... / `id`, `model`, `usage`, `stop_reason`, ...).
#[derive(Debug, Clone)]
pub struct NewMessage {
    pub role: Role,
    pub content: Value,
    pub top: Map<String, Value>,
    pub msg: Map<String, Value>,
    /// Marks every earlier message `compacted` (used with a compact boundary).
    pub compacts_previous: bool,
}

impl NewMessage {
    pub fn user(content: Value) -> Self {
        Self {
            role: Role::User,
            content,
            top: Map::new(),
            msg: Map::new(),
            compacts_previous: false,
        }
    }
    pub fn assistant(content: Value) -> Self {
        Self {
            role: Role::Assistant,
            ..Self::user(content)
        }
    }
    /// Node `system/compact_boundary` (A.3).
    pub fn compact_boundary(trigger: &str, summary: &str) -> Self {
        let mut m = Self {
            role: Role::System,
            ..Self::user(Value::Null)
        };
        m.top.insert("subtype".into(), json!("compact_boundary"));
        m.top.insert("trigger".into(), json!(trigger));
        m.top.insert("summary".into(), json!(summary));
        m.compacts_previous = true;
        m
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct MessageRow {
    pub idx: u64,
    pub role: String,
    pub content: Value,
    pub compacted: bool,
    pub meta: Value,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Retention {
    Session,
    Keep,
    /// RFC 3339 expiry for a future `synt gc`.
    Until(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolCallRecord {
    pub id: String,
    pub turn_id: String,
    pub tool_name: String,
    pub input_ref: Option<String>,
    pub result_ref: Option<String>,
    pub status: String,
    pub decision: Option<String>,
    pub duration_ms: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessRecord {
    pub id: String,
    pub name: Option<String>,
    pub pid: Option<u32>,
    /// OS start-time token from [`crate::process_start_time`], recorded right after spawn.
    pub pid_start: Option<String>,
    pub cwd: String,
    pub command: String,
    pub started_at: String,
    pub ended_at: Option<String>,
    pub status: String,
    pub exit_code: Option<i64>,
    pub log_artifact_id: Option<String>,
    pub log_cursor: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApprovalRecord {
    pub id: String,
    pub tool_call_id: String,
    pub dialog: Value,
    pub decision: String,
    pub reviewer: String,
    pub requested_at: String,
    pub resolved_at: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ArtifactRow {
    pub reference: ArtifactRef,
    pub kind: String,
    pub retention_policy: String,
    pub expires_at: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileSnapshot {
    pub path: String,
    /// `None`: the file did not exist at checkpoint time.
    pub artifact_id: Option<String>,
    /// Set when the stored artifact is NOT the file's bytes (redacted copy or secret-file
    /// placeholder): the sha256 of the real content. Restore must not write such an artifact back.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lossy_sha256: Option<String>,
    /// [`file_state_hash`] of the file after the run's last write to it (set when that write
    /// completed). `synt rewind` refuses a file whose state differs, unless forced.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub post_sha256: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Checkpoint {
    pub id: String,
    pub name: String,
    pub event_seq: u64,
    pub message_count: u64,
    pub files: Vec<FileSnapshot>,
    pub created_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportStats {
    pub lines: usize,
    pub messages: usize,
    pub skipped: usize,
}

impl Store {
    /// Creates `synthesis_home` and the database if needed and applies pending migrations.
    pub fn open(synthesis_home: &Path, opts: StoreOptions) -> Result<Store> {
        private_dir(synthesis_home)?;
        #[cfg(unix)]
        chmod(synthesis_home, 0o700)?;
        ensure_db_file(&synthesis_home.join("synt.db"))?;
        // Two processes opening a fresh home at once: `connect` retries its setup; the migration
        // is retried here too.
        retry_busy(Duration::from_secs(15), || {
            let mut conn = connect(synthesis_home, false)?;
            schema::migrate(&mut conn, &now())
        })?;
        Ok(Store {
            home: synthesis_home.to_path_buf(),
            mirror: opts.write_node_transcripts,
            redactor: Arc::new(Redactor::new(opts.secrets)),
            limits: Limits {
                max_events: opts.max_events_per_session,
                max_event_bytes: opts.max_event_bytes_per_session,
                keep_deltas: opts.keep_recent_deltas,
                max_artifact_bytes: opts.max_artifact_bytes_per_session,
                redact_snapshots: opts.redact_snapshots,
            },
        })
    }

    pub fn home(&self) -> &Path {
        &self.home
    }

    /// Retention of attached images: in every session not touched for `days` days (`updated_at`
    /// before `now - days`; messages carry no time of their own), each image block of a stored
    /// message becomes the text `[image removed after N days]`. Rows keep their `idx` and order;
    /// nothing else changes. One immediate transaction; a database that is locked by another
    /// process is skipped (`Ok(0)`), never an error. Returns the images removed. `days == 0`
    /// keeps everything.
    pub fn purge_images(&self, days: u32, now: OffsetDateTime) -> Result<PurgeReport> {
        if days == 0 {
            return Ok(PurgeReport::default());
        }
        let fmt = time::macros::format_description!(
            "[year]-[month]-[day]T[hour]:[minute]:[second].[subsecond digits:3]Z"
        );
        let cutoff = (now - time::Duration::days(i64::from(days)))
            .format(&fmt)
            .expect("format utc timestamp");
        let mut conn = connect(&self.home, false)?;
        conn.busy_timeout(Duration::from_millis(200))?;
        let busy = |e: &rusqlite::Error| {
            matches!(
                e,
                rusqlite::Error::SqliteFailure(f, _)
                    if matches!(f.code, rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked)
            )
        };
        // freed pages are overwritten with zeros, not just unlinked (per connection; must be set
        // before the transaction starts)
        conn.pragma_update(None, "secure_delete", true)?;
        let tx = match conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate) {
            Ok(t) => t,
            Err(e) if busy(&e) => return Ok(PurgeReport::default()),
            Err(e) => return Err(e.into()),
        };
        let rows: Vec<(String, i64, String, String)> = {
            let mut st = tx.prepare(
                "SELECT m.session_id, m.idx, m.content, m.meta FROM messages m \
                 JOIN sessions s ON s.id = m.session_id \
                 WHERE s.updated_at < ?1 \
                   AND (m.content LIKE '%\"image\"%' OR m.meta LIKE '%\"image\"%')",
            )?;
            st.query_map([&cutoff], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
            })?
            .collect::<std::result::Result<_, _>>()?
        };
        let text = format!("[image removed after {days} days]");
        let mut removed = 0;
        for (sid, idx, content, meta) in rows {
            let (Ok(mut c), Ok(mut m)) = (
                serde_json::from_str::<Value>(&content),
                serde_json::from_str::<Value>(&meta),
            ) else {
                continue;
            };
            // `meta` holds the extras of a Node-format line: images anywhere in it go too
            let n = replace_images(&mut c, &text) + replace_images_deep(&mut m, &text);
            if n > 0 {
                tx.execute(
                    "UPDATE messages SET content = ?1, meta = ?2 WHERE session_id = ?3 AND idx = ?4",
                    rusqlite::params![c.to_string(), m.to_string(), sid, idx],
                )?;
                removed += n;
            }
        }
        match tx.commit() {
            Ok(()) => {
                let mut wal_truncated = true;
                if removed > 0 {
                    // The old pages may still sit in the write-ahead log: fold it into the
                    // database and truncate it. A reader holding the log makes that "busy":
                    // retried briefly, then reported (the images are already gone from the
                    // tables; only stale copies remain in the log until the next checkpoint).
                    // No VACUUM: it rewrites the whole database under an exclusive lock and
                    // needs free disk for a copy; secure_delete already zeroed what was freed.
                    wal_truncated = false;
                    for _ in 0..5 {
                        let r: std::result::Result<(i64, i64, i64), _> =
                            conn.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |r| {
                                Ok((r.get(0)?, r.get(1)?, r.get(2)?))
                            });
                        if matches!(r, Ok((0, _, _))) {
                            wal_truncated = true;
                            break;
                        }
                        std::thread::sleep(Duration::from_millis(100));
                    }
                }
                Ok(PurgeReport {
                    images: removed,
                    wal_truncated,
                })
            }
            Err(e) if busy(&e) => Ok(PurgeReport::default()),
            Err(e) => Err(e.into()),
        }
    }

    /// Deletes the sessions that hold no message and no task, were last touched more than
    /// `min_age` ago, are not `keep` and are not locked by a live process, with every dependent
    /// row and their directory. Anything doubtful (a child session, a lock held, a failed
    /// delete, a busy database) is skipped. Returns the sessions removed.
    pub fn prune_empty_sessions(
        &self,
        keep: &str,
        min_age: Duration,
        now: OffsetDateTime,
    ) -> Result<usize> {
        let fmt = time::macros::format_description!(
            "[year]-[month]-[day]T[hour]:[minute]:[second].[subsecond digits:3]Z"
        );
        let cutoff = (now - min_age).format(&fmt).expect("format utc timestamp");
        let mut conn = connect(&self.home, false)?;
        conn.busy_timeout(Duration::from_millis(200))?;
        let ids: Vec<String> = {
            let mut st = conn.prepare(
                "SELECT id FROM sessions s WHERE id != ?1 AND updated_at < ?2 \
                 AND NOT EXISTS (SELECT 1 FROM messages WHERE session_id=s.id) \
                 AND NOT EXISTS (SELECT 1 FROM tasks WHERE session_id=s.id) \
                 AND NOT EXISTS (SELECT 1 FROM sessions c WHERE c.parent_session_id=s.id)",
            )?;
            st.query_map([keep, &cutoff], |r| r.get(0))?
                .collect::<std::result::Result<_, _>>()?
        };
        let mut removed = 0;
        for id in ids.iter().filter(|id| valid_id(id)) {
            let dir = self.home.join("sessions").join(id);
            // a held lock (live process) is skipped at once; no lock file means nobody holds it
            let lock = match File::options().write(true).open(dir.join("lock")) {
                Ok(f) if f.try_lock().is_ok() => Some(f),
                Ok(_) => continue,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
                Err(_) => continue,
            };
            let Ok(tx) = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            else {
                continue;
            };
            // re-checked inside the write transaction: a session that got a message meanwhile stays
            let used: bool = tx.query_row(
                "SELECT EXISTS (SELECT 1 FROM messages WHERE session_id=?1) \
                     OR EXISTS (SELECT 1 FROM tasks WHERE session_id=?1)",
                [id],
                |r| r.get(0),
            )?;
            if used {
                continue;
            }
            let gone = [
                "DELETE FROM verification_reports WHERE task_id IN (SELECT id FROM tasks WHERE session_id=?1)",
                "DELETE FROM tasks WHERE session_id=?1",
                "DELETE FROM events WHERE session_id=?1",
                "DELETE FROM messages WHERE session_id=?1",
                "DELETE FROM tool_calls WHERE session_id=?1",
                "DELETE FROM processes WHERE session_id=?1",
                "DELETE FROM browser_contexts WHERE session_id=?1",
                "DELETE FROM artifacts WHERE session_id=?1",
                "DELETE FROM approvals WHERE session_id=?1",
                "DELETE FROM checkpoints WHERE session_id=?1",
                "DELETE FROM sessions WHERE id=?1",
            ]
            .iter()
            .try_for_each(|q| tx.execute(q, [id]).map(drop));
            if gone.is_err() || tx.commit().is_err() {
                continue;
            }
            drop(lock); // Windows cannot remove an open file
            let _ = fs::remove_dir_all(&dir);
            removed += 1;
        }
        Ok(removed)
    }

    /// `/close`: hides the session from [`Reader::list_used_sessions`]; messages stay and it can
    /// still be resumed by id. Returns whether a session of that id exists.
    pub fn close_session(&self, id: &str) -> Result<bool> {
        check_id(id)?;
        let conn = connect(&self.home, false)?;
        let n = conn.execute(
            "UPDATE sessions SET closed_at=?2 WHERE id=?1 AND closed_at IS NULL",
            (id, now()),
        )?;
        Ok(n > 0)
    }

    /// A read-only connection; any number may exist next to the single writer.
    pub fn reader(&self) -> Result<Reader> {
        Reader::open(&self.home)
    }

    /// Persists the explicit workspace and takes the session's writer lock.
    pub fn create_session(&self, new: NewSession<'_>) -> Result<SessionWriter> {
        let mirror = |id: &str, cwd: &str| {
            self.mirror
                .then(|| node::transcript_path(&self.home, cwd, id))
        };
        self.create_inner(new, mirror)
    }

    fn create_inner(
        &self,
        new: NewSession<'_>,
        mirror: impl FnOnce(&str, &str) -> Option<PathBuf>,
    ) -> Result<SessionWriter> {
        let id = new
            .id
            .clone()
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
        check_id(&id)?;
        let ws = new.workspace;
        let (root, cwd) = (utf8(&ws.root)?, utf8(&ws.cwd)?);
        let dirs = ws
            .additional_dirs
            .iter()
            .map(|d| utf8(d))
            .collect::<Result<Vec<_>>>()?;
        let mirror_path = mirror(&id, &cwd);
        let legacy = mirror_path.as_ref().map(|p| utf8(p)).transpose()?;
        let lock = lock_session(&self.home, &id)?;
        let conn = connect(&self.home, false)?;
        let ts = now();
        let r = conn.execute(
            "INSERT INTO sessions(id,status,workspace_id,workspace_root,cwd,additional_dirs,git_worktree,
               model_config,permission_mode,permission_policy_id,budget,usage,title,parent_session_id,
               parent_tool_call_id,legacy_transcript_path,created_at,updated_at)
             VALUES (?1,'created',?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?16)",
            params![
                id,
                ws.workspace_id,
                root,
                cwd,
                serde_json::to_string(&dirs)?,
                ws.git_worktree.as_deref().map(utf8).transpose()?,
                serde_json::to_string(&new.model_config_id)?,
                enum_str(&new.permission_mode),
                new.permission_policy_id,
                serde_json::to_string(&new.budget)?,
                serde_json::to_string(&BudgetUsage::default())?,
                new.title.as_deref().map(|t| self.redactor.redact_str(t)),
                new.parent.as_ref().map(|p| p.session_id.clone()),
                new.parent.as_ref().map(|p| p.tool_call_id.clone()),
                legacy,
                ts,
            ],
        );
        match r {
            Err(rusqlite::Error::SqliteFailure(e, _))
                if e.code == rusqlite::ErrorCode::ConstraintViolation && {
                    conn.query_row("SELECT 1 FROM sessions WHERE id=?1", [&id], |_| Ok(()))
                        .optional()?
                        .is_some()
                } =>
            {
                return Err(Error::AlreadyExists(id));
            }
            r => r?,
        };
        Ok(self.writer(conn, id, lock, mirror_path, cwd, root, dirs))
    }

    #[allow(clippy::too_many_arguments)]
    fn writer(
        &self,
        conn: Connection,
        id: String,
        lock: File,
        mirror: Option<PathBuf>,
        cwd: String,
        root: String,
        dirs: Vec<String>,
    ) -> SessionWriter {
        let roots = std::iter::once(root)
            .chain(dirs)
            .map(PathBuf::from)
            .collect();
        let w = SessionWriter {
            reader: Reader {
                conn,
                home: self.home.clone(),
            },
            id,
            lock,
            redactor: self.redactor.clone(),
            limits: self.limits,
            mirror,
            cwd,
            roots,
            mirror_failures: 0,
            task_id: None,
            turn_id: None,
            agent_id: None,
        };
        w.sweep_orphan_artifacts(); // we hold the session lock: nothing else is mid-write
        w
    }

    /// Takes the writer lock of an existing session. A session left `running` by a dead
    /// process is first moved to `interrupted`. Fails with [`Error::TranscriptChanged`] when
    /// the Node JSONL it mirrors was modified by someone else, unless `accept_external_changes`.
    pub fn resume_session(&self, id: &str, accept_external_changes: bool) -> Result<SessionWriter> {
        let mut w = self.open_locked(id)?;
        let rec = w.get_session(id)?;
        if let (Some(p), false) = (&rec.legacy_transcript_path, accept_external_changes)
            && transcript_changed(&w.conn, id, Path::new(p))?
        {
            return Err(Error::TranscriptChanged(PathBuf::from(p)));
        }
        if RUNNING_STATES.contains(&rec.status) {
            w.update_status(
                SessionStatus::Interrupted,
                Some("writer process exited; found running on resume"),
            )?;
        } else if rec.status == SessionStatus::Cancelling {
            w.repair_cancelling("writer process exited; found cancelling on resume")?;
        }
        Ok(w)
    }

    /// Lock + connection for an existing session without any state checks (recovery).
    pub(crate) fn open_locked(&self, id: &str) -> Result<SessionWriter> {
        check_id(id)?;
        let reader = Reader::open(&self.home)?;
        let rec = reader.get_session(id)?;
        let lock = lock_session(&self.home, id)?;
        let conn = connect(&self.home, false)?;
        let mirror = rec
            .legacy_transcript_path
            .as_ref()
            .filter(|_| self.mirror)
            .map(PathBuf::from);
        Ok(self.writer(
            conn,
            id.to_string(),
            lock,
            mirror,
            rec.cwd,
            rec.workspace_root,
            rec.additional_dirs,
        ))
    }

    /// Node -> Rust (C.1): messages, title and permission mode become rows plus one
    /// `legacy.imported` event. The JSONL is only read; later mirror writes append to it.
    /// The imported `permission-mode` is capped at `default` (plan and dontAsk are kept: they are
    /// stricter); use [`Store::import_node_transcript_trusted`] to restore the recorded mode.
    pub fn import_node_transcript(
        &self,
        path: &Path,
        new: NewSession<'_>,
    ) -> Result<(SessionWriter, ImportStats)> {
        self.import_node_transcript_trusted(path, new, false)
    }

    /// `trust_permission_mode`: the caller vouches for the file, so a recorded `acceptEdits` /
    /// `auto` is restored. `bypassPermissions` is never restored.
    pub fn import_node_transcript_trusted(
        &self,
        path: &Path,
        new: NewSession<'_>,
        trust_permission_mode: bool,
    ) -> Result<(SessionWriter, ImportStats)> {
        let size = fs::metadata(path)?.len();
        if size > MAX_IMPORT_BYTES {
            return Err(Error::TooLarge {
                what: "transcript",
                size: size as usize,
                max: MAX_IMPORT_BYTES as usize,
            });
        }
        let text = String::from_utf8_lossy(&fs::read(path)?).into_owned();
        let lines = text.split('\n').filter(|l| !l.trim().is_empty()).count();
        let entries = node::parse_lines(&text);
        let mut new = new;
        if new.id.is_none() {
            new.id = path
                .file_stem()
                .and_then(|s| s.to_str())
                .map(str::to_string);
        }
        let src = path.to_path_buf();
        let mut w = self.create_inner(new, |_, _| Some(src))?;
        // The transcript is the one being continued: keep the model/permission rows consistent.
        w.update_status(SessionStatus::Preparing, Some("imported"))?;
        w.update_status(SessionStatus::Idle, Some("imported"))?;
        let mut stats = ImportStats {
            lines,
            messages: 0,
            skipped: lines - entries.len(),
        };
        let (mut title, mut mode) = (None, None);
        let redactor = w.redactor.clone();
        let tx = w
            .reader
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        for e in &entries {
            if e.get("type").and_then(Value::as_str) == Some("summary") {
                title = e
                    .get("summary")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_string);
                continue;
            }
            if e.get("type").and_then(Value::as_str) == Some("permission-mode") {
                let m = e
                    .get("permissionMode")
                    .and_then(Value::as_str)
                    .and_then(|s| enum_from::<PermissionMode>(s).ok());
                let capped = m.map(|m| match m {
                    PermissionMode::Plan | PermissionMode::DontAsk | PermissionMode::Default => m,
                    PermissionMode::BypassPermissions => m,
                    _ if trust_permission_mode => m,
                    _ => PermissionMode::Default,
                });
                mode = capped
                    .filter(|m| *m != PermissionMode::BypassPermissions)
                    .or(mode);
                continue;
            }
            let sidechain = e
                .get("isSidechain")
                .is_some_and(|v| v.as_bool().unwrap_or(true));
            let Some((role, mut content, mut meta)) = node::split_entry(e).filter(|_| !sidechain)
            else {
                stats.skipped += 1;
                continue;
            };
            redactor.redact_value(&mut content);
            redactor.redact_value(&mut meta);
            insert_message(&tx, &w.id, role, &content, &meta, false)?;
            stats.messages += 1;
        }
        if let Some(t) = title {
            tx.execute(
                "UPDATE sessions SET title=?2 WHERE id=?1",
                params![w.id, redactor.redact_str(&t)],
            )?;
        }
        if let Some(m) = mode {
            tx.execute(
                "UPDATE sessions SET permission_mode=?2 WHERE id=?1",
                params![w.id, enum_str(&m)],
            )?;
        }
        tx.commit()?;
        let payload =
            json!({"source_path": utf8(path)?, "lines": stats.lines, "skipped": stats.skipped});
        w.append_parts("legacy.imported", payload)?;
        w.record_transcript_stat(path)?;
        Ok((w, stats))
    }
}

/// Read side. `SessionWriter` derefs to it, so a writer can also read.
pub struct Reader {
    pub(crate) conn: Connection,
    home: PathBuf,
}

const SESSION_COLS: &str = "id,status,workspace_id,workspace_root,cwd,additional_dirs,git_worktree,model_config,
  permission_mode,permission_policy_id,budget,usage,event_cursor,checkpoint_id,title,parent_session_id,
  parent_tool_call_id,legacy_transcript_path,created_at,updated_at";

fn bad(e: impl std::fmt::Display) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, e.to_string().into())
}

fn session_from_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<SessionRecord> {
    let js = |i: usize| -> rusqlite::Result<String> { r.get(i) };
    let parent: Option<String> = r.get(15)?;
    let ptool: Option<String> = r.get(16)?;
    Ok(SessionRecord {
        id: r.get(0)?,
        status: enum_from(&js(1)?).map_err(bad)?,
        workspace_id: r.get(2)?,
        workspace_root: r.get(3)?,
        cwd: r.get(4)?,
        additional_dirs: serde_json::from_str(&js(5)?).map_err(bad)?,
        git_worktree: r.get(6)?,
        model_config_id: serde_json::from_str(&js(7)?).map_err(bad)?,
        permission_mode: enum_from(&js(8)?).map_err(bad)?,
        permission_policy_id: r.get(9)?,
        budget: serde_json::from_str(&js(10)?).map_err(bad)?,
        usage: serde_json::from_str(&js(11)?).map_err(bad)?,
        event_cursor: r.get::<_, i64>(12)? as u64,
        checkpoint_id: r.get(13)?,
        title: r.get(14)?,
        parent: parent.map(|session_id| ParentRef {
            session_id,
            tool_call_id: ptool.unwrap_or_default(),
        }),
        legacy_transcript_path: r.get(17)?,
        created_at: r.get(18)?,
        updated_at: r.get(19)?,
    })
}

fn safe_rel(p: &str) -> Result<&Path> {
    let path = Path::new(p);
    if path.components().all(|c| matches!(c, Component::Normal(_))) {
        Ok(path)
    } else {
        Err(Error::Invalid(format!("unsafe artifact path {p:?}")))
    }
}

impl Reader {
    pub fn open(synthesis_home: &Path) -> Result<Reader> {
        Ok(Reader {
            conn: connect(synthesis_home, true)?,
            home: synthesis_home.to_path_buf(),
        })
    }

    pub fn get_session(&self, id: &str) -> Result<SessionRecord> {
        self.conn
            .query_row(
                &format!("SELECT {SESSION_COLS} FROM sessions WHERE id=?1"),
                [id],
                session_from_row,
            )
            .optional()?
            .ok_or_else(|| Error::NotFound(format!("session {id}")))
    }

    /// Newest first; `cwd` filters by the persisted working directory.
    pub fn list_sessions(&self, cwd: Option<&str>) -> Result<Vec<SessionRecord>> {
        let mut st = self.conn.prepare(&format!(
            "SELECT {SESSION_COLS} FROM sessions WHERE (?1 IS NULL OR cwd=?1) ORDER BY updated_at DESC, id"
        ))?;
        let rows = st.query_map([cwd], session_from_row)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// [`Self::list_sessions`] without the sessions that hold no message and no task (the ones
    /// every launch leaves behind when nothing was typed) and the closed ones.
    pub fn list_used_sessions(&self, cwd: Option<&str>) -> Result<Vec<SessionRecord>> {
        let mut st = self.conn.prepare(&format!(
            "SELECT {SESSION_COLS} FROM sessions WHERE (?1 IS NULL OR cwd=?1) \
             AND closed_at IS NULL \
             AND (EXISTS (SELECT 1 FROM messages WHERE session_id=sessions.id) \
               OR EXISTS (SELECT 1 FROM tasks WHERE session_id=sessions.id)) \
             ORDER BY updated_at DESC, id"
        ))?;
        let rows = st.query_map([cwd], session_from_row)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Per-day activity of every session in the store since `since_ms`, days taken at
    /// `tz_offset_min` east of UTC (read-only; sparse: days without events are absent).
    pub fn usage_by_day(&self, since_ms: i64, tz_offset_min: i32) -> Result<Vec<UsageDay>> {
        let mut st = self.conn.prepare(
            "SELECT (CAST(strftime('%s', ts) AS INTEGER) + ?2 * 60) / 86400 AS day,
               SUM(CASE WHEN type='turn.completed'
                   THEN COALESCE(json_extract(payload,'$.usage.input_tokens'),0)
                      + COALESCE(json_extract(payload,'$.usage.output_tokens'),0) ELSE 0 END),
               SUM(CASE WHEN type='turn.completed'
                   THEN COALESCE(json_extract(payload,'$.usage.output_tokens'),0) ELSE 0 END),
               SUM(type='user.message'), SUM(type='tool.started'), SUM(type='subagent.started'),
               COUNT(DISTINCT session_id),
               SUM(type='tool.started' AND json_extract(payload,'$.name')='Bash'),
               SUM(type='tool.started' AND json_extract(payload,'$.name') IN ('Write','Edit','MultiEdit','NotebookEdit')),
               SUM(type='tool.started' AND substr(json_extract(payload,'$.name'),1,8)='browser_'),
               SUM(type='tool.started' AND substr(json_extract(payload,'$.name'),1,5)='mcp__')
             FROM events
             WHERE type IN ('turn.completed','user.message','tool.started','subagent.started')
               AND payload IS NOT NULL AND CAST(strftime('%s', ts) AS INTEGER) >= ?1 / 1000
             GROUP BY day ORDER BY day",
        )?;
        let rows = st.query_map(params![since_ms, tz_offset_min], |r| {
            let n = |i: usize| {
                r.get::<_, Option<i64>>(i)
                    .map(|v| v.unwrap_or(0).max(0) as u64)
            };
            Ok(UsageDay {
                day: r.get(0)?,
                tokens: n(1)?,
                output: n(2)?,
                messages: n(3)?,
                tools: n(4)?,
                agents: n(5)?,
                sessions: n(6)?,
                commands: n(7)?,
                files: n(8)?,
                browser: n(9)?,
                mcp: n(10)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Events with `seq > from_cursor` in order, for replay and reconnect (`from_cursor` 0 = all).
    pub fn read_events(
        &self,
        session_id: &str,
        from_cursor: u64,
        limit: usize,
    ) -> Result<Vec<Record>> {
        let mut st = self.conn.prepare(
            "SELECT v,seq,ts,task_id,turn_id,agent_id,type,payload,payload_ref FROM events
             WHERE session_id=?1 AND seq>?2 ORDER BY seq LIMIT ?3",
        )?;
        let rows = st.query_map(params![session_id, from_cursor as i64, limit as i64], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, Option<String>>(3)?,
                r.get::<_, Option<String>>(4)?,
                r.get::<_, Option<String>>(5)?,
                r.get::<_, String>(6)?,
                r.get::<_, Option<String>>(7)?,
                r.get::<_, Option<String>>(8)?,
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (v, seq, ts, task, turn, agent, kind, payload, pref) = row?;
            let payload: Value = match (payload, pref) {
                (Some(p), _) => serde_json::from_str(&p)?,
                (None, Some(id)) => self
                    .artifact_bytes(session_id, &id)
                    .ok()
                    .and_then(|b| serde_json::from_slice(&b).ok())
                    .unwrap_or_else(|| json!({"payload_ref": id, "unavailable": true})),
                (None, None) => Value::Null,
            };
            let value = json!({"v": v, "seq": seq, "ts": ts, "session_id": session_id, "task_id": task,
                "turn_id": turn, "agent_id": agent, "type": kind, "payload": payload});
            out.push(match serde_json::from_value::<Envelope>(value.clone()) {
                Ok(env) => Record::Known(env),
                Err(_) => Record::Unknown(serde_json::from_value::<RawEnvelope>(value)?),
            });
        }
        Ok(out)
    }

    pub fn read_messages(&self, session_id: &str) -> Result<Vec<MessageRow>> {
        let mut st = self.conn.prepare(
            "SELECT idx,role,content,compacted,meta FROM messages WHERE session_id=?1 ORDER BY idx",
        )?;
        let rows = st.query_map([session_id], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, i64>(3)?,
                r.get::<_, String>(4)?,
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (idx, role, content, compacted, meta) = row?;
            out.push(MessageRow {
                idx: idx as u64,
                role,
                content: serde_json::from_str(&content)?,
                compacted: compacted != 0,
                meta: serde_json::from_str(&meta)?,
            });
        }
        Ok(out)
    }

    pub fn tool_calls(&self, session_id: &str) -> Result<Vec<ToolCallRecord>> {
        let mut st = self.conn.prepare(
            "SELECT id,turn_id,tool_name,input_ref,result_ref,status,decision,duration_ms
             FROM tool_calls WHERE session_id=?1 ORDER BY rowid",
        )?;
        let rows = st.query_map([session_id], |r| {
            Ok(ToolCallRecord {
                id: r.get(0)?,
                turn_id: r.get(1)?,
                tool_name: r.get(2)?,
                input_ref: r.get(3)?,
                result_ref: r.get(4)?,
                status: r.get(5)?,
                decision: r.get(6)?,
                duration_ms: r.get(7)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn processes(&self, session_id: &str) -> Result<Vec<ProcessRecord>> {
        let mut st = self.conn.prepare(
            "SELECT id,name,pid,pid_start,cwd,command,started_at,ended_at,status,exit_code,log_artifact_id,log_cursor
             FROM processes WHERE session_id=?1 ORDER BY started_at, id",
        )?;
        let rows = st.query_map([session_id], |r| {
            Ok(ProcessRecord {
                id: r.get(0)?,
                name: r.get(1)?,
                pid: r.get::<_, Option<i64>>(2)?.map(|p| p as u32),
                pid_start: r.get(3)?,
                cwd: r.get(4)?,
                command: r.get(5)?,
                started_at: r.get(6)?,
                ended_at: r.get(7)?,
                status: r.get(8)?,
                exit_code: r.get(9)?,
                log_artifact_id: r.get(10)?,
                log_cursor: r.get(11)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn approvals(&self, session_id: &str) -> Result<Vec<ApprovalRecord>> {
        let mut st = self.conn.prepare(
            "SELECT id,tool_call_id,dialog,decision,reviewer,requested_at,resolved_at
             FROM approvals WHERE session_id=?1 ORDER BY requested_at, id",
        )?;
        let rows = st.query_map([session_id], |r| {
            Ok(ApprovalRecord {
                id: r.get(0)?,
                tool_call_id: r.get(1)?,
                dialog: serde_json::from_str(&r.get::<_, String>(2)?).map_err(bad)?,
                decision: r.get(3)?,
                reviewer: r.get(4)?,
                requested_at: r.get(5)?,
                resolved_at: r.get(6)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn artifacts(&self, session_id: &str) -> Result<Vec<ArtifactRow>> {
        let mut st = self.conn.prepare(
            "SELECT id,path,size,sha256,mime,kind,retention_policy,expires_at
             FROM artifacts WHERE session_id=?1 ORDER BY created_at, id",
        )?;
        let rows = st.query_map([session_id], |r| {
            Ok(ArtifactRow {
                reference: ArtifactRef {
                    artifact_id: r.get(0)?,
                    path: r.get(1)?,
                    size: r.get::<_, i64>(2)? as u64,
                    sha256: r.get(3)?,
                    mime: r.get(4)?,
                },
                kind: r.get(5)?,
                retention_policy: r.get(6)?,
                expires_at: r.get(7)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn artifact_bytes(&self, session_id: &str, artifact_id: &str) -> Result<Vec<u8>> {
        let rel: String = self
            .conn
            .query_row(
                "SELECT path FROM artifacts WHERE id=?1 AND session_id=?2",
                [artifact_id, session_id],
                |r| r.get(0),
            )
            .optional()?
            .ok_or_else(|| Error::NotFound(format!("artifact {artifact_id}")))?;
        Ok(fs::read(
            self.home
                .join("sessions")
                .join(session_id)
                .join(safe_rel(&rel)?),
        )?)
    }

    pub fn checkpoints(&self, session_id: &str) -> Result<Vec<Checkpoint>> {
        let mut st = self.conn.prepare(
            "SELECT id,name,event_seq,message_count,files,created_at FROM checkpoints
             WHERE session_id=?1 ORDER BY rowid",
        )?;
        let rows = st.query_map([session_id], |r| {
            Ok(Checkpoint {
                id: r.get(0)?,
                name: r.get(1)?,
                event_seq: r.get::<_, i64>(2)? as u64,
                message_count: r.get::<_, i64>(3)? as u64,
                files: serde_json::from_str(&r.get::<_, String>(4)?).map_err(bad)?,
                created_at: r.get(5)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn checkpoint(&self, session_id: &str, name: &str) -> Result<Checkpoint> {
        self.checkpoints(session_id)?
            .into_iter()
            .find(|c| c.name == name)
            .ok_or_else(|| Error::NotFound(format!("checkpoint {name}")))
    }

    /// `(attempt, status, report)` for a task, oldest first.
    pub fn verification_reports(&self, task_id: &str) -> Result<Vec<(i64, String, Value)>> {
        let mut st = self.conn.prepare(
            "SELECT attempt,status,report FROM verification_reports WHERE task_id=?1 ORDER BY attempt, rowid",
        )?;
        let rows = st.query_map([task_id], |r| {
            Ok((
                r.get(0)?,
                r.get(1)?,
                serde_json::from_str(&r.get::<_, String>(2)?).map_err(bad)?,
            ))
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }
}

/// `sessions/<id>/lock`: an OS advisory lock, released by the kernel when the holder dies.
fn lock_session(home: &Path, id: &str) -> Result<File> {
    let dir = home.join("sessions").join(id);
    private_dir(&dir)?;
    let f = private_file(OpenOptions::new().create(true).truncate(false).write(true))
        .open(dir.join("lock"))?;
    // A concurrent fork/exec in this process can hold a copy of a just-released lock fd until
    // its exec, so a "held" answer is retried briefly before it is believed.
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(2000);
    loop {
        match f.try_lock() {
            Ok(()) => return Ok(f),
            Err(fs::TryLockError::WouldBlock) if std::time::Instant::now() < deadline => {
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            Err(fs::TryLockError::WouldBlock) => return Err(Error::Locked(id.to_string())),
            Err(fs::TryLockError::Error(e)) => return Err(e.into()),
        }
    }
}

fn file_stat(path: &Path) -> Option<(i64, i64)> {
    let m = fs::metadata(path).ok()?;
    let mt = m
        .modified()
        .ok()?
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_nanos();
    Some((m.len() as i64, mt as i64))
}

fn ends_mid_line(path: &Path) -> bool {
    use std::io::{Read, Seek, SeekFrom};
    let Ok(mut f) = File::open(path) else {
        return false;
    };
    let mut b = [0u8; 1];
    f.seek(SeekFrom::End(-1)).is_ok() && f.read_exact(&mut b).is_ok() && b[0] != b'\n'
}

fn transcript_changed(conn: &Connection, id: &str, path: &Path) -> Result<bool> {
    let rec: (Option<i64>, Option<i64>) = conn.query_row(
        "SELECT legacy_size, legacy_mtime_ns FROM sessions WHERE id=?1",
        [id],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    Ok(match (file_stat(path), rec) {
        (None, _) => false,
        (Some(now), (Some(s), Some(m))) => now != (s, m),
        (Some(_), _) => true, // a transcript exists that we never wrote or imported
    })
}

fn insert_message(
    tx: &Transaction<'_>,
    sid: &str,
    role: &str,
    content: &Value,
    meta: &Value,
    compact: bool,
) -> Result<u64> {
    let content = serde_json::to_string(content)?;
    if content.len() > MAX_MESSAGE_BYTES {
        return Err(Error::TooLarge {
            what: "message",
            size: content.len(),
            max: MAX_MESSAGE_BYTES,
        });
    }
    let idx: i64 = tx.query_row(
        "SELECT COALESCE(MAX(idx)+1,0) FROM messages WHERE session_id=?1",
        [sid],
        |r| r.get(0),
    )?;
    if compact {
        tx.execute("UPDATE messages SET compacted=1 WHERE session_id=?1", [sid])?;
    }
    tx.execute(
        "INSERT INTO messages(session_id,idx,role,content,meta) VALUES (?1,?2,?3,?4,?5)",
        params![sid, idx, role, content, serde_json::to_string(meta)?],
    )?;
    Ok(idx as u64)
}

/// sha256 of a regular file's bytes; `absent`, `not-a-regular-file` (symlink, directory ...) or
/// `unreadable` otherwise. Compared, never trusted as a digest.
pub fn file_state_hash(p: &Path) -> String {
    match fs::symlink_metadata(p) {
        Ok(m) if m.is_file() => fs::read(p).map_or_else(|_| "unreadable".into(), |b| sha_hex(&b)),
        Ok(_) => "not-a-regular-file".into(),
        Err(_) => "absent".into(),
    }
}

fn sha_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn ext_for(mime: &str) -> &'static str {
    match mime {
        "text/plain" => "txt",
        "application/json" => "json",
        "image/png" => "png",
        "image/jpeg" => "jpg",
        _ => "bin",
    }
}

/// temp file (create_new, 0600) -> fsync -> rename -> row (session-store B.3). The row is only
/// inserted once the bytes are durable; a rolled-back transaction leaves an unreferenced file,
/// never a dangling row, and [`SessionWriter::sweep_orphan_artifacts`] removes those.
#[allow(clippy::too_many_arguments)]
fn put_artifact_tx(
    conn: &Connection,
    home: &Path,
    limits: &Limits,
    sid: &str,
    kind: &str,
    mime: &str,
    bytes: &[u8],
    retention: &Retention,
) -> Result<ArtifactRef> {
    if bytes.len() > MAX_ARTIFACT_BYTES {
        return Err(Error::TooLarge {
            what: "artifact",
            size: bytes.len(),
            max: MAX_ARTIFACT_BYTES,
        });
    }
    if let Some(max) = limits.max_artifact_bytes {
        let used: i64 = conn.query_row(
            "SELECT COALESCE(SUM(size),0) FROM artifacts WHERE session_id=?1",
            [sid],
            |r| r.get(0),
        )?;
        if used as u64 + bytes.len() as u64 > max {
            return Err(Error::QuotaExceeded {
                what: "artifact bytes",
                limit: max,
            });
        }
    }
    let id = uuid::Uuid::new_v4().to_string();
    let rel = format!("artifacts/{id}.{}", ext_for(mime));
    let dir = home.join("sessions").join(sid).join("artifacts");
    private_dir(&dir)?;
    let tmp = dir.join(format!(".{id}.tmp"));
    let mut f = private_file(OpenOptions::new().write(true).create_new(true)).open(&tmp)?;
    f.write_all(bytes)?;
    f.sync_all()?;
    drop(f);
    fs::rename(&tmp, home.join("sessions").join(sid).join(&rel))?;
    let sha = sha_hex(bytes);
    let (policy, expires) = match retention {
        Retention::Session => ("session", None),
        Retention::Keep => ("keep", None),
        Retention::Until(t) => ("ttl", Some(t.clone())),
    };
    conn.execute(
        "INSERT INTO artifacts(id,session_id,kind,path,mime,size,sha256,retention_policy,expires_at,created_at)
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)",
        params![id, sid, kind, rel, mime, bytes.len() as i64, sha, policy, expires, now()],
    )?;
    Ok(ArtifactRef {
        artifact_id: id,
        path: rel,
        size: bytes.len() as u64,
        sha256: sha,
        mime: mime.to_string(),
    })
}

/// The writer of one session. Holds the session's OS lock for its whole life.
pub struct SessionWriter {
    reader: Reader,
    id: String,
    #[allow(dead_code)] // held for its Drop: releases the OS lock
    lock: File,
    pub(crate) redactor: Arc<Redactor>,
    limits: Limits,
    mirror: Option<PathBuf>,
    cwd: String,
    roots: Vec<PathBuf>,
    mirror_failures: u32,
    /// Stamped on events appended from now on (like `synthesis_events::EventWriter`).
    pub task_id: Option<String>,
    pub turn_id: Option<String>,
    pub agent_id: Option<String>,
}

impl Deref for SessionWriter {
    type Target = Reader;
    fn deref(&self) -> &Reader {
        &self.reader
    }
}

impl SessionWriter {
    pub fn id(&self) -> &str {
        &self.id
    }

    /// The Node JSONL this session mirrors to, if mirroring is on.
    pub fn mirror_path(&self) -> Option<&Path> {
        self.mirror.as_deref()
    }

    /// Mirror writes are best effort (as in Node); this counts the ones that failed.
    pub fn mirror_failures(&self) -> u32 {
        self.mirror_failures
    }

    /// Stamps, redacts and persists `event`; returns it as persisted. `seq` is allocated inside
    /// the transaction that inserts the row and bumps `event_cursor`, so it is gapless and
    /// monotonic across crashes. Call this before streaming the event to any client.
    pub fn append_event(&mut self, event: Event) -> Result<Envelope> {
        let Value::Object(mut m) = serde_json::to_value(&event)? else {
            unreachable!("tagged enum")
        };
        let kind = m
            .remove("type")
            .and_then(|t| t.as_str().map(str::to_string))
            .unwrap_or_default();
        let payload = m.remove("payload").unwrap_or(Value::Null);
        let raw = self.append_parts(&kind, payload)?;
        Ok(serde_json::from_value(serde_json::to_value(raw)?)?)
    }

    /// Untyped append, used for store-owned types such as `legacy.imported`.
    pub(crate) fn append_parts(&mut self, kind: &str, payload: Value) -> Result<RawEnvelope> {
        // A busy/locked write (another peer on the same synt.db, slow Windows disk) rolled back
        // whole, so it is safe to run again; the event sink would otherwise break the task.
        let r = retry_busy(Duration::from_secs(15), || {
            self.append_parts_inner(kind, payload.clone())
        });
        if r.is_err() {
            self.sweep_orphan_artifacts(); // the failed transaction is rolled back by now
        }
        r
    }

    /// Byte cap, checked before the append so the delta collapse it triggers is committed even
    /// when the append is then refused.
    fn check_event_bytes(&mut self) -> Result<()> {
        let Some(max) = self.limits.max_event_bytes else {
            return Ok(());
        };
        let cursor = self.reader.get_session(&self.id)?.event_cursor;
        if (cursor + 1) % BYTE_CHECK_EVERY != 0 || event_bytes(&self.reader.conn, &self.id)? <= max
        {
            return Ok(());
        }
        self.compact_deltas()?;
        if event_bytes(&self.reader.conn, &self.id)? > max {
            return Err(Error::QuotaExceeded {
                what: "event bytes",
                limit: max,
            });
        }
        Ok(())
    }

    fn append_parts_inner(&mut self, kind: &str, mut payload: Value) -> Result<RawEnvelope> {
        self.check_event_bytes()?;
        self.redactor.redact_value(&mut payload);
        let (task, turn, agent) = (
            self.task_id.clone(),
            self.turn_id.clone(),
            self.agent_id.clone(),
        );
        let tx = self
            .reader
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let ts = now();
        let seq = append_tx(
            &tx,
            &self.reader.home,
            &self.limits,
            &self.id,
            &ts,
            (&task, &turn, &agent),
            kind,
            &payload,
        )?;
        tx.commit()?;
        Ok(RawEnvelope {
            v: SCHEMA_VERSION,
            seq,
            ts,
            session_id: self.id.clone(),
            task_id: task,
            turn_id: turn,
            agent_id: agent,
            kind: kind.to_string(),
            payload,
        })
    }

    /// Validated against the `synthesis_core` state machine; emits `session.status_changed` in the
    /// same transaction. Returns the new event cursor.
    pub fn update_status(&mut self, to: SessionStatus, reason: Option<&str>) -> Result<u64> {
        retry_busy(Duration::from_secs(15), || self.set_status(to, reason))
    }

    /// `validate = false` skips the state machine; only [`Self::repair_cancelling`] uses it.
    fn set_status(&mut self, to: SessionStatus, reason: Option<&str>) -> Result<u64> {
        let (task, turn, agent) = (
            self.task_id.clone(),
            self.turn_id.clone(),
            self.agent_id.clone(),
        );
        let tx = self
            .reader
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let cur: String =
            tx.query_row("SELECT status FROM sessions WHERE id=?1", [&self.id], |r| {
                r.get(0)
            })?;
        let from: SessionStatus = enum_from(&cur)?;
        from.transition(to)?;
        tx.execute(
            "UPDATE sessions SET status=?2 WHERE id=?1",
            params![self.id, enum_str(&to)],
        )?;
        let mut payload = json!({"from": enum_str(&from), "to": enum_str(&to)});
        if let Some(r) = reason {
            payload["reason"] = json!(r);
        }
        self.redactor.redact_value(&mut payload);
        let seq = append_tx(
            &tx,
            &self.reader.home,
            &self.limits,
            &self.id,
            &now(),
            (&task, &turn, &agent),
            "session.status_changed",
            &payload,
        )?;
        tx.commit()?;
        Ok(seq)
    }

    pub fn update_usage(&mut self, usage: &BudgetUsage) -> Result<()> {
        self.reader.conn.execute(
            "UPDATE sessions SET usage=?2, updated_at=?3 WHERE id=?1",
            params![self.id, serde_json::to_string(usage)?, now()],
        )?;
        Ok(())
    }

    /// Appends a provider-visible message and mirrors it to the Node JSONL. Returns its `idx`.
    pub fn append_message(&mut self, m: NewMessage) -> Result<u64> {
        let mut content = m.content;
        self.redactor.redact_value(&mut content);
        let mut meta = json!({"ts": now(), "top": m.top, "msg": m.msg});
        self.redactor.redact_value(&mut meta);
        let tx = self
            .reader
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let idx = insert_message(
            &tx,
            &self.id,
            m.role.as_str(),
            &content,
            &meta,
            m.compacts_previous,
        )?;
        tx.execute(
            "UPDATE sessions SET updated_at=?2 WHERE id=?1",
            params![self.id, now()],
        )?;
        tx.commit()?;
        let line = node::node_line(&self.id, &self.cwd, m.role.as_str(), &content, &meta, None);
        self.mirror_line(&line);
        Ok(idx)
    }

    /// `/rename` title: row plus a Node `summary` line.
    pub fn set_title(&mut self, title: &str) -> Result<()> {
        let title = self.redactor.redact_str(title);
        self.reader.conn.execute(
            "UPDATE sessions SET title=?2, updated_at=?3 WHERE id=?1",
            params![self.id, title, now()],
        )?;
        self.mirror_line(&self.stamped(json!({"type": "summary", "summary": title})));
        Ok(())
    }

    /// `/model`: the model the session continues with (`resume` uses it unless `--model` is given).
    pub fn set_model(&mut self, label: &str) -> Result<()> {
        self.reader.conn.execute(
            "UPDATE sessions SET model_config=?2, updated_at=?3 WHERE id=?1",
            params![self.id, serde_json::to_string(label)?, now()],
        )?;
        Ok(())
    }

    /// Row plus a Node `permission-mode` line.
    pub fn set_permission_mode(&mut self, mode: PermissionMode) -> Result<()> {
        self.reader.conn.execute(
            "UPDATE sessions SET permission_mode=?2, updated_at=?3 WHERE id=?1",
            params![self.id, enum_str(&mode), now()],
        )?;
        self.mirror_line(
            &self.stamped(json!({"type": "permission-mode", "permissionMode": enum_str(&mode)})),
        );
        Ok(())
    }

    /// Mirror-only Node `attachment` line for a hook result (`kind`: `hook_success`, ...).
    pub fn mirror_hook_attachment(
        &mut self,
        kind: &str,
        hook_name: &str,
        hook_event: &str,
        exit_code: Option<i64>,
    ) {
        let a = json!({"type": kind, "hookName": hook_name, "hookEvent": hook_event, "exitCode": exit_code});
        self.mirror_line(&self.stamped(json!({"type": "attachment", "attachment": a})));
    }

    fn stamped(&self, mut line: Value) -> Value {
        if let Some(o) = line.as_object_mut() {
            o.insert("timestamp".into(), json!(now()));
            o.insert("sessionId".into(), json!(self.id));
            o.insert("cwd".into(), json!(self.cwd));
        }
        line
    }

    /// Best-effort append of one line (A.2: one write, no fsync), then record size+mtime.
    fn mirror_line(&mut self, line: &Value) {
        let Some(path) = self.mirror.clone() else {
            return;
        };
        let ok = (|| -> Result<()> {
            if let Some(dir) = path.parent() {
                private_dir(dir)?;
            }
            let mut buf = serde_json::to_vec(line)?;
            buf.push(b'\n');
            // A file cut off mid-line (crash) must not swallow our line.
            if ends_mid_line(&path) {
                buf.insert(0, b'\n');
            }
            private_file(OpenOptions::new().create(true).append(true))
                .open(&path)?
                .write_all(&buf)?;
            self.record_transcript_stat(&path)
        })();
        if ok.is_err() {
            self.mirror_failures += 1;
        }
    }

    pub(crate) fn record_transcript_stat(&self, path: &Path) -> Result<()> {
        if let Some((size, mtime)) = file_stat(path) {
            self.reader.conn.execute(
                "UPDATE sessions SET legacy_size=?2, legacy_mtime_ns=?3 WHERE id=?1",
                params![self.id, size, mtime],
            )?;
        }
        Ok(())
    }

    /// Rust -> Node export (C.2/C.4): every message as a Node line into a new file.
    pub fn export_node_transcript(&self, dest: &Path) -> Result<usize> {
        if let Some(dir) = dest.parent() {
            fs::create_dir_all(dir)?;
        }
        let mut f = private_file(OpenOptions::new().write(true).create_new(true))
            .open(dest)
            .map_err(|e| {
                if e.kind() == std::io::ErrorKind::AlreadyExists {
                    Error::AlreadyExists(dest.display().to_string())
                } else {
                    e.into()
                }
            })?;
        let rows = self.read_messages(&self.id)?;
        for r in &rows {
            let mut buf = serde_json::to_vec(&node::node_line(
                &self.id, &self.cwd, &r.role, &r.content, &r.meta, None,
            ))?;
            buf.push(b'\n');
            f.write_all(&buf)?;
        }
        Ok(rows.len())
    }

    pub fn put_artifact(
        &mut self,
        kind: &str,
        mime: &str,
        bytes: &[u8],
        retention: Retention,
    ) -> Result<ArtifactRef> {
        // Persisted copies are redacted; snapshots must restore byte-exact and are not.
        let textual =
            kind != "snapshot" && (mime.starts_with("text/") || mime == "application/json");
        let redacted;
        let bytes = match std::str::from_utf8(bytes) {
            Ok(s) if textual => {
                redacted = self.redactor.redact_str(s).into_bytes();
                &redacted[..]
            }
            _ => bytes,
        };
        put_artifact_tx(
            &self.reader.conn,
            &self.reader.home,
            &self.limits,
            &self.id,
            kind,
            mime,
            bytes,
            &retention,
        )
    }

    /// Removes files in `artifacts/` that no row references (left by a rolled-back transaction or
    /// a crash between rename and insert). Best effort; only safe while we hold the session lock.
    pub(crate) fn sweep_orphan_artifacts(&self) {
        let dir = self
            .reader
            .home
            .join("sessions")
            .join(&self.id)
            .join("artifacts");
        let Ok(rd) = fs::read_dir(&dir) else { return };
        let known: HashSet<String> = self
            .reader
            .conn
            .prepare("SELECT path FROM artifacts WHERE session_id=?1")
            .and_then(|mut st| {
                st.query_map([&self.id], |r| r.get(0))?
                    .collect::<rusqlite::Result<_>>()
            })
            .unwrap_or_default();
        for e in rd.flatten() {
            let rel = format!("artifacts/{}", e.file_name().to_string_lossy());
            if !known.contains(&rel) && e.file_type().is_ok_and(|t| t.is_file()) {
                let _ = fs::remove_file(e.path());
            }
        }
    }

    /// Store-level repair for a crash during `cancelling` (`cancelling -> interrupted`).
    pub(crate) fn repair_cancelling(&mut self, reason: &str) -> Result<()> {
        self.set_status(SessionStatus::Interrupted, Some(reason))
            .map(|_| ())
    }

    /// Collapses runs of consecutive `model.delta` rows of completed messages (all but the newest
    /// `keep_recent_deltas`): the first row of a run carries the joined text (<= 4 KiB), the others
    /// keep their seq with empty text. seq stays gapless and every row still deserialises.
    /// Returns the number of rows rewritten.
    pub fn compact_deltas(&mut self) -> Result<u64> {
        let tx = self
            .reader
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let n = compact_deltas_tx(&tx, &self.id, self.limits.keep_deltas)?;
        tx.commit()?;
        Ok(n)
    }

    pub fn begin_task(&mut self, request: &str, criteria: &[Criterion]) -> Result<String> {
        let id = uuid::Uuid::new_v4().to_string();
        self.reader.conn.execute(
            "INSERT INTO tasks(id,session_id,request,acceptance_criteria,status,started_at) VALUES (?1,?2,?3,?4,'running',?5)",
            params![id, self.id, self.redactor.redact_str(request), self.redactor.redact_str(&serde_json::to_string(criteria)?), now()],
        )?;
        self.task_id = Some(id.clone());
        Ok(id)
    }

    pub fn end_task(&mut self, task_id: &str, status: TaskStatus, reason: &str) -> Result<()> {
        let n = self.reader.conn.execute(
            "UPDATE tasks SET status=?3, reason=?4, ended_at=?5 WHERE id=?1 AND session_id=?2",
            params![
                task_id,
                self.id,
                enum_str(&status),
                self.redactor.redact_str(reason),
                now()
            ],
        )?;
        if n == 0 {
            return Err(Error::NotFound(format!("task {task_id}")));
        }
        if self.task_id.as_deref() == Some(task_id) {
            self.task_id = None;
        }
        Ok(())
    }

    pub fn record_verification_report(
        &mut self,
        task_id: &str,
        attempt: i64,
        status: &str,
        report: &Value,
    ) -> Result<String> {
        let id = uuid::Uuid::new_v4().to_string();
        let mut report = report.clone();
        self.redactor.redact_value(&mut report);
        self.reader
            .conn
            .execute(
                "INSERT INTO verification_reports(id,task_id,attempt,status,report,created_at)
             SELECT ?1,id,?3,?4,?5,?6 FROM tasks WHERE id=?2 AND session_id=?7",
                params![
                    id,
                    task_id,
                    attempt,
                    status,
                    serde_json::to_string(&report)?,
                    now(),
                    self.id
                ],
            )
            .map_err(Error::from)
            .and_then(|n| {
                if n == 0 {
                    Err(Error::NotFound(format!("task {task_id}")))
                } else {
                    Ok(())
                }
            })?;
        Ok(id)
    }

    /// Insert or update by tool-use id (unique per session).
    pub fn upsert_tool_call(&mut self, c: &ToolCallRecord) -> Result<()> {
        self.reader.conn.execute(
            "INSERT INTO tool_calls(id,session_id,turn_id,tool_name,input_ref,result_ref,status,decision,duration_ms)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)
             ON CONFLICT(session_id,id) DO UPDATE SET turn_id=excluded.turn_id, tool_name=excluded.tool_name,
               input_ref=excluded.input_ref, result_ref=excluded.result_ref, status=excluded.status,
               decision=excluded.decision, duration_ms=excluded.duration_ms",
            params![c.id, self.id, c.turn_id, c.tool_name, c.input_ref, c.result_ref, c.status, c.decision, c.duration_ms],
        )?;
        Ok(())
    }

    pub fn upsert_process(&mut self, p: &ProcessRecord) -> Result<()> {
        self.reader.conn.execute(
            "INSERT INTO processes(id,session_id,name,pid,pid_start,cwd,command,started_at,ended_at,status,exit_code,log_artifact_id,log_cursor)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13)
             ON CONFLICT(id) DO UPDATE SET name=excluded.name, pid=excluded.pid, pid_start=excluded.pid_start,
               ended_at=excluded.ended_at, status=excluded.status, exit_code=excluded.exit_code,
               log_artifact_id=excluded.log_artifact_id, log_cursor=excluded.log_cursor
             WHERE processes.session_id=excluded.session_id",
            params![
                p.id, self.id, p.name, p.pid, p.pid_start, p.cwd, self.redactor.redact_str(&p.command),
                p.started_at, p.ended_at, p.status, p.exit_code, p.log_artifact_id, p.log_cursor
            ],
        )?;
        Ok(())
    }

    pub fn finish_process(&mut self, id: &str, status: &str, exit_code: Option<i64>) -> Result<()> {
        self.reader.conn.execute(
            "UPDATE processes SET status=?3, exit_code=?4, ended_at=?5 WHERE id=?1 AND session_id=?2",
            params![id, self.id, status, exit_code, now()],
        )?;
        Ok(())
    }

    /// Records a pending approval (`decision` = `pending` until resolved).
    pub fn request_approval(&mut self, id: &str, tool_call_id: &str, dialog: &Value) -> Result<()> {
        let mut dialog = dialog.clone();
        self.redactor.redact_value(&mut dialog);
        self.reader.conn.execute(
            "INSERT INTO approvals(id,session_id,tool_call_id,dialog,decision,reviewer,requested_at)
             VALUES (?1,?2,?3,?4,'pending','pending',?5)",
            params![id, self.id, tool_call_id, serde_json::to_string(&dialog)?, now()],
        )?;
        Ok(())
    }

    pub fn resolve_approval(&mut self, id: &str, decision: &str, reviewer: &str) -> Result<()> {
        let n = self.reader.conn.execute(
            "UPDATE approvals SET decision=?3, reviewer=?4, resolved_at=?5 WHERE id=?1 AND session_id=?2",
            params![id, self.id, decision, reviewer, now()],
        )?;
        if n == 0 {
            Err(Error::NotFound(format!("approval {id}")))
        } else {
            Ok(())
        }
    }

    /// What to store for a snapshot: the bytes as read when `redact_snapshots` is off; otherwise a
    /// placeholder for secret files and a redacted copy for text that contains secrets. The second
    /// value is the sha256 of the real content whenever the stored bytes differ from it.
    fn snapshot_bytes(&self, real: &Path, bytes: Vec<u8>) -> (Vec<u8>, Option<String>) {
        if !self.limits.redact_snapshots {
            return (bytes, None);
        }
        let sha = sha_hex(&bytes);
        if synthesis_core::secrets::is_secret_path(real) {
            let note = format!("[synt: secret file withheld from snapshot; sha256={sha}]");
            return (note.into_bytes(), Some(sha));
        }
        match std::str::from_utf8(&bytes).map(|t| self.redactor.redact_str(t)) {
            Ok(r) if r.as_bytes() != bytes => (r.into_bytes(), Some(sha)),
            _ => (bytes, None),
        }
    }

    /// Notes the state of `p` after a write of the run on every checkpoint entry that names it.
    pub fn record_post_run_hash(&mut self, p: &Path) -> Result<()> {
        let abs = lexical_normalize(&if p.is_relative() {
            Path::new(&self.cwd).join(p)
        } else {
            p.to_path_buf()
        });
        let (path, sha) = (utf8(&abs)?, file_state_hash(&abs));
        let conn = &self.reader.conn;
        let rows: Vec<(String, String)> = conn
            .prepare("SELECT id,files FROM checkpoints WHERE session_id=?1")?
            .query_map([&self.id], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<rusqlite::Result<_>>()?;
        for (id, files) in rows {
            let mut files: Vec<FileSnapshot> = serde_json::from_str(&files)?;
            let mut hit = false;
            for f in files.iter_mut().filter(|f| f.path == path) {
                f.post_sha256 = Some(sha.clone());
                hit = true;
            }
            if hit {
                conn.execute(
                    "UPDATE checkpoints SET files=?2 WHERE id=?1",
                    params![id, serde_json::to_string(&files)?],
                )?;
            }
        }
        Ok(())
    }

    /// Named restore point: current event seq + message count + a snapshot artifact per changed
    /// file. Files must lie inside the session's workspace roots (checked on realpaths).
    pub fn create_checkpoint(
        &mut self,
        name: &str,
        changed_files: &[PathBuf],
    ) -> Result<Checkpoint> {
        let mut files = Vec::new();
        for p in changed_files {
            let abs = lexical_normalize(&if p.is_relative() {
                Path::new(&self.cwd).join(p)
            } else {
                p.clone()
            });
            let real =
                realpath_lenient(&abs).ok_or_else(|| Error::OutsideWorkspace(abs.clone()))?;
            if !self
                .roots
                .iter()
                .any(|r| contains(r, &real, PathPolicy::NATIVE))
            {
                return Err(Error::OutsideWorkspace(real));
            }
            let mut lossy_sha = None;
            let artifact_id = match fs::metadata(&real) {
                Ok(m) if m.is_file() => {
                    if m.len() > MAX_SNAPSHOT_BYTES {
                        return Err(Error::TooLarge {
                            what: "snapshot",
                            size: m.len() as usize,
                            max: MAX_SNAPSHOT_BYTES as usize,
                        });
                    }
                    let bytes = fs::read(&real)?;
                    let (stored, lossy) = self.snapshot_bytes(&real, bytes);
                    lossy_sha = lossy;
                    Some(
                        self.put_artifact(
                            "snapshot",
                            "application/octet-stream",
                            &stored,
                            Retention::Keep,
                        )?
                        .artifact_id,
                    )
                }
                Ok(_) => return Err(Error::Invalid(format!("{} is not a file", real.display()))),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
                Err(e) => return Err(e.into()),
            };
            files.push(FileSnapshot {
                path: utf8(&abs)?,
                artifact_id,
                lossy_sha256: lossy_sha,
                post_sha256: None,
            });
        }
        let id = uuid::Uuid::new_v4().to_string();
        let ts = now();
        let tx = self
            .reader
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let seq: i64 = tx.query_row(
            "SELECT event_cursor FROM sessions WHERE id=?1",
            [&self.id],
            |r| r.get(0),
        )?;
        let count: i64 = tx.query_row(
            "SELECT COUNT(*) FROM messages WHERE session_id=?1",
            [&self.id],
            |r| r.get(0),
        )?;
        tx.execute(
            "INSERT INTO checkpoints(id,session_id,name,event_seq,message_count,files,created_at) VALUES (?1,?2,?3,?4,?5,?6,?7)",
            params![id, self.id, name, seq, count, serde_json::to_string(&files)?, ts],
        )?;
        tx.execute(
            "UPDATE sessions SET checkpoint_id=?2 WHERE id=?1",
            params![self.id, id],
        )?;
        tx.commit()?;
        Ok(Checkpoint {
            id,
            name: name.to_string(),
            event_seq: seq as u64,
            message_count: count as u64,
            files,
            created_at: ts,
        })
    }
}

/// Allocates the next seq from the authoritative `event_cursor` and inserts the row, in the
/// caller's IMMEDIATE transaction. Payloads over 8 KiB are stored as an artifact (`payload_ref`).
#[allow(clippy::too_many_arguments)]
fn append_tx(
    tx: &Transaction<'_>,
    home: &Path,
    limits: &Limits,
    sid: &str,
    ts: &str,
    (task, turn, agent): (&Option<String>, &Option<String>, &Option<String>),
    kind: &str,
    payload: &Value,
) -> Result<u64> {
    let body = serde_json::to_string(payload)?;
    if body.len() > MAX_PAYLOAD_BYTES {
        return Err(Error::TooLarge {
            what: "event payload",
            size: body.len(),
            max: MAX_PAYLOAD_BYTES,
        });
    }
    let cursor: i64 = tx.query_row(
        "SELECT event_cursor FROM sessions WHERE id=?1",
        [sid],
        |r| r.get(0),
    )?;
    let seq = cursor + 1;
    if let Some(max) = limits.max_events
        && cursor as u64 >= max
    {
        return Err(Error::QuotaExceeded {
            what: "events",
            limit: max,
        });
    }
    let (inline, pref) = if body.len() <= MAX_INLINE_PAYLOAD {
        (Some(body), None)
    } else {
        let a = put_artifact_tx(
            tx,
            home,
            limits,
            sid,
            "event_payload",
            "application/json",
            body.as_bytes(),
            &Retention::Session,
        )?;
        (None, Some(a.artifact_id))
    };
    tx.execute(
        "INSERT INTO events(session_id,seq,v,ts,task_id,turn_id,agent_id,type,payload,payload_ref)
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)",
        params![
            sid,
            seq,
            SCHEMA_VERSION,
            ts,
            task,
            turn,
            agent,
            kind,
            inline,
            pref
        ],
    )?;
    tx.execute(
        "UPDATE sessions SET event_cursor=?2, updated_at=?3 WHERE id=?1",
        params![sid, seq, ts],
    )?;
    Ok(seq as u64)
}

fn event_bytes(conn: &Connection, sid: &str) -> Result<u64> {
    Ok(conn.query_row(
        "SELECT COALESCE(SUM(LENGTH(payload)),0) FROM events WHERE session_id=?1",
        [sid],
        |r| r.get::<_, i64>(0),
    )? as u64)
}

fn compact_deltas_tx(tx: &Transaction<'_>, sid: &str, keep: u64) -> Result<u64> {
    let last_complete: Option<i64> = tx.query_row(
        "SELECT MAX(seq) FROM events WHERE session_id=?1 AND type='assistant.message'",
        [sid],
        |r| r.get(0),
    )?;
    let Some(last_complete) = last_complete else {
        return Ok(0);
    };
    let mut rows: Vec<(i64, String)> = tx
        .prepare(
            "SELECT seq,payload FROM events WHERE session_id=?1 AND type='model.delta'
             AND seq<=?2 AND payload IS NOT NULL ORDER BY seq",
        )?
        .query_map(params![sid, last_complete], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<rusqlite::Result<_>>()?;
    rows.truncate(rows.len().saturating_sub(keep as usize));
    let mut changed = 0;
    let mut run: Option<DeltaRun> = None;
    let mut flush = |run: &mut Option<DeltaRun>| -> Result<()> {
        if let Some(r) = run.take()
            && !r.tail.is_empty()
        {
            let body = |t: &str| json!({"block_index": r.key.0, "kind": r.key.1, "text": t});
            changed += set_payload(tx, sid, r.head, &body(&r.text).to_string())?;
            for t in &r.tail {
                changed += set_payload(tx, sid, *t, &body("").to_string())?;
            }
        }
        Ok(())
    };
    for (seq, raw) in rows {
        let v: Value = serde_json::from_str(&raw)?;
        let Some(text) = v.get("text").and_then(Value::as_str) else {
            continue;
        };
        let key = (v["block_index"].clone(), v["kind"].clone());
        match &mut run {
            Some(r)
                if r.key == key
                    && r.last + 1 == seq
                    && r.text.len() + text.len() <= MAX_BATCH_TEXT =>
            {
                r.text.push_str(text);
                r.tail.push(seq);
                r.last = seq;
            }
            _ => {
                flush(&mut run)?;
                run = Some(DeltaRun {
                    head: seq,
                    key,
                    last: seq,
                    text: text.to_string(),
                    tail: Vec::new(),
                });
            }
        }
    }
    flush(&mut run)?;
    Ok(changed)
}

/// Consecutive same-block deltas being joined: `head` keeps the text, `tail` rows are emptied.
struct DeltaRun {
    head: i64,
    key: (Value, Value),
    last: i64,
    text: String,
    tail: Vec<i64>,
}

/// 1 when the row changed, 0 when it already held `body`.
fn set_payload(tx: &Transaction<'_>, sid: &str, seq: i64, body: &str) -> Result<u64> {
    Ok(tx.execute(
        "UPDATE events SET payload=?3 WHERE session_id=?1 AND seq=?2 AND payload<>?3",
        params![sid, seq, body],
    )? as u64)
}

/// What [`Store::purge_images`] did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PurgeReport {
    /// Images replaced.
    pub images: usize,
    /// The write-ahead log was truncated afterwards. `false` after a purge that removed images
    /// means another connection held the log: stale copies stay in it until the next checkpoint.
    pub wal_truncated: bool,
}

impl Default for PurgeReport {
    fn default() -> Self {
        Self {
            images: 0,
            wal_truncated: true,
        }
    }
}

/// Images at any depth (for `meta`, which is not model-visible content).
fn replace_images_deep(v: &mut Value, text: &str) -> usize {
    match v {
        Value::Object(o) if o.get("type").and_then(Value::as_str) == Some("image") => {
            *v = serde_json::json!({"type": "text", "text": text});
            1
        }
        Value::Object(o) => o.values_mut().map(|x| replace_images_deep(x, text)).sum(),
        Value::Array(a) => a.iter_mut().map(|x| replace_images_deep(x, text)).sum(),
        _ => 0,
    }
}

/// Image bytes in a stored message: the message's own image blocks, and what a `tool_result` block
/// carries (a screenshot a tool returned, at any depth inside that content). Nothing else: a
/// `tool_use` input or any other JSON that happens to say `"type": "image"` is left alone.
fn replace_images(content: &mut Value, text: &str) -> usize {
    let block = || serde_json::json!({"type": "text", "text": text});
    // images anywhere inside a tool result's content
    fn deep(v: &mut Value, block: &dyn Fn() -> Value) -> usize {
        match v {
            Value::Object(o) if o.get("type").and_then(Value::as_str) == Some("image") => {
                *v = block();
                1
            }
            Value::Object(o) => o.values_mut().map(|x| deep(x, block)).sum(),
            Value::Array(a) => a.iter_mut().map(|x| deep(x, block)).sum(),
            _ => 0,
        }
    }
    let Value::Array(blocks) = content else {
        return 0;
    };
    let mut n = 0;
    for b in blocks {
        match b.get("type").and_then(Value::as_str) {
            Some("image") => {
                *b = block();
                n += 1;
            }
            Some("tool_result") => {
                if let Some(c) = b.get_mut("content") {
                    n += deep(c, &block);
                }
            }
            _ => {}
        }
    }
    n
}
