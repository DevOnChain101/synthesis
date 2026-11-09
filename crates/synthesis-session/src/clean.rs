//! `/clean`: permanent deletion of stored sessions (rows, events, checkpoints, per-session
//! directory). Conservative on purpose: the sessions the caller names in `keep`, every session
//! whose `lock` is held by a live process (this one included) and the whole parent/child tree of
//! either are never touched; a directory that is not a real directory directly under
//! `$SYNT_HOME/sessions` is never followed. Only the session store is touched: settings, auth,
//! memory, trust records, the model cache and the Node transcript copies stay where they are.

use std::collections::{HashMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::path::{Path, PathBuf};
use std::time::Duration;

use rusqlite::{OptionalExtension, TransactionBehavior, params};
use time::OffsetDateTime;

use crate::error::{Error, Result};
use crate::store::{
    Store, connect, delete_session_rows, private_dir, private_file, utf8, valid_id,
};

/// "Sessions inactive for more than N days".
pub const INACTIVE_DAYS: i64 = 30;
/// An empty session younger than this may still be starting in another process.
const EMPTY_GRACE: time::Duration = time::Duration::hours(1);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CleanScope {
    /// Closed (`/close`) sessions, and sessions with no message and no task older than an hour.
    ClosedEmpty,
    /// Not updated for [`INACTIVE_DAYS`] days.
    Inactive,
    /// Every session.
    All,
}

impl CleanScope {
    fn pred(self) -> &'static str {
        match self {
            Self::ClosedEmpty => {
                "(s.closed_at IS NOT NULL OR (s.updated_at < ?1 \
                  AND NOT EXISTS (SELECT 1 FROM messages WHERE session_id=s.id) \
                  AND NOT EXISTS (SELECT 1 FROM tasks WHERE session_id=s.id)))"
            }
            Self::Inactive => "s.updated_at < ?1",
            Self::All => "?1 IS NOT NULL",
        }
    }
    fn cutoff(self, now: OffsetDateTime) -> String {
        let t = match self {
            Self::ClosedEmpty => now - EMPTY_GRACE,
            Self::Inactive => now - time::Duration::days(INACTIVE_DAYS),
            Self::All => now,
        };
        ts(t)
    }
}

/// `updated_at` is always written by the store's own `now()` (fixed-width RFC 3339 UTC, millis, `Z`;
/// a Node import goes through `create_session` too), so the text comparison in the scope predicates is
/// chronological. A NULL or foreign-format value does not match (`NULL < x` is NULL), so it is kept.
fn ts(t: OffsetDateTime) -> String {
    let fmt = time::macros::format_description!(
        "[year]-[month]-[day]T[hour]:[minute]:[second].[subsecond digits:3]Z"
    );
    t.format(&fmt).expect("format utc timestamp")
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CleanCounts {
    pub sessions: usize,
    /// Workspace folders that lose their last stored session.
    pub workspaces: usize,
    pub messages: u64,
    pub events: u64,
    /// Bytes of the per-session directories (nothing followed).
    pub disk_bytes: u64,
    /// Of the artifact rows, by kind.
    pub screenshot_bytes: u64,
    pub snapshot_bytes: u64,
    pub log_bytes: u64,
    /// In scope but running here or in another process (or linked to one): kept.
    pub skipped_running: usize,
    /// In scope but kept for another reason: an unusual directory, a kept child session.
    pub skipped_other: usize,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CleanPlan {
    /// Children before parents.
    pub ids: Vec<String>,
    pub counts: CleanCounts,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CleanResult {
    pub deleted: usize,
    pub events: u64,
    pub skipped_running: usize,
    /// Changed since it was shown (no longer in scope, or got a child).
    pub skipped_other: usize,
    /// `id: reason`
    pub failed: Vec<String>,
    pub freed_bytes: u64,
    pub backup: Option<PathBuf>,
}

enum Probe {
    /// Nobody holds it; the guard keeps it so until dropped.
    Free(Option<File>),
    Held,
    Unusual(String),
}

/// A held `sessions/<id>/lock` is a live process. Neither the directory nor the lock file may be a
/// link (nothing is followed); a missing directory or lock means nobody holds it.
fn probe(dir: &Path) -> Probe {
    match fs::symlink_metadata(dir) {
        Ok(m) if m.is_dir() => {}
        Ok(_) => return Probe::Unusual("not a real directory (a link?): not followed".into()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Probe::Free(None),
        Err(e) => return Probe::Unusual(e.to_string()),
    }
    let lock = dir.join("lock");
    match fs::symlink_metadata(&lock) {
        Ok(m) if m.is_file() => {}
        Ok(_) => return Probe::Unusual("lock is not a regular file".into()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Probe::Free(None),
        Err(e) => return Probe::Unusual(e.to_string()),
    }
    match File::options().write(true).open(&lock) {
        Ok(f) => match f.try_lock() {
            Ok(()) => Probe::Free(Some(f)),
            Err(fs::TryLockError::WouldBlock) => Probe::Held,
            Err(fs::TryLockError::Error(e)) => Probe::Unusual(e.to_string()),
        },
        Err(e) => Probe::Unusual(e.to_string()),
    }
}

/// Steps from `id` up to its root (`cap` stops a cycle).
fn depth_of<'a>(parent: &HashMap<&'a str, &'a str>, mut id: &'a str, cap: usize) -> usize {
    let mut d = 0;
    while let Some(&p) = parent.get(id) {
        id = p;
        d += 1;
        if d > cap {
            break;
        }
    }
    d
}

/// Bytes of the regular files under `dir`; links are neither followed nor counted.
fn dir_size(dir: &Path) -> u64 {
    let mut total = 0;
    let mut todo = vec![dir.to_path_buf()];
    while let Some(d) = todo.pop() {
        let Ok(rd) = fs::read_dir(&d) else { continue };
        for e in rd.flatten() {
            match e.file_type() {
                Ok(t) if t.is_dir() => todo.push(e.path()),
                Ok(t) if t.is_file() => total += e.metadata().map_or(0, |m| m.len()),
                _ => {}
            }
        }
    }
    total
}

impl Store {
    /// What `scope` would delete now. Read-only. `keep`: sessions that must stay (the open one).
    pub fn clean_plan(
        &self,
        scope: CleanScope,
        keep: &[String],
        now: OffsetDateTime,
    ) -> Result<CleanPlan> {
        let conn = connect(self.home(), true)?;
        let all: Vec<(String, String, Option<String>)> = conn
            .prepare("SELECT id, workspace_root, parent_session_id FROM sessions")?
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
            .collect::<std::result::Result<_, _>>()?;
        let cand: HashSet<String> = conn
            .prepare(&format!(
                "SELECT s.id FROM sessions s WHERE {}",
                scope.pred()
            ))?
            .query_map([scope.cutoff(now)], |r| r.get(0))?
            .collect::<std::result::Result<_, _>>()?;
        let parent: HashMap<&str, &str> = all
            .iter()
            .filter_map(|(id, _, p)| Some((id.as_str(), p.as_deref()?)))
            .collect();

        let sessions = self.home().join("sessions");
        let mut counts = CleanCounts::default();
        let mut prot: HashSet<&str> = keep.iter().map(String::as_str).collect();
        let mut unusual = HashSet::new();
        // every session is probed, not only the candidates: a running parent or child that is
        // not in scope still protects its tree
        for (id, _, _) in &all {
            if !valid_id(id) {
                if cand.contains(id) {
                    unusual.insert(id.as_str());
                }
                continue;
            }
            match probe(&sessions.join(id)) {
                Probe::Free(_) => {}
                Probe::Held => {
                    prot.insert(id);
                }
                Probe::Unusual(_) if cand.contains(id) => {
                    unusual.insert(id.as_str());
                }
                Probe::Unusual(_) => {}
            }
        }
        // a running session protects its whole tree (parents and sub-agent children)
        loop {
            let mut grew = false;
            for (id, _, p) in &all {
                let Some(p) = p.as_deref() else { continue };
                if prot.contains(id.as_str()) != prot.contains(p) {
                    prot.insert(id);
                    prot.insert(p);
                    grew = true;
                }
            }
            if !grew {
                break;
            }
        }
        let mut sel: HashSet<&str> = HashSet::new();
        for id in &cand {
            if prot.contains(id.as_str()) {
                counts.skipped_running += 1;
            } else if unusual.contains(id.as_str()) {
                counts.skipped_other += 1;
            } else {
                sel.insert(id);
            }
        }
        // a session with a child that stays cannot go (the child's foreign key)
        loop {
            let stay: Vec<&str> = all
                .iter()
                .filter(|(id, _, p)| {
                    !sel.contains(id.as_str()) && p.as_deref().is_some_and(|p| sel.contains(p))
                })
                .filter_map(|(_, _, p)| p.as_deref())
                .collect();
            if stay.is_empty() {
                break;
            }
            for p in stay {
                if sel.remove(p) {
                    counts.skipped_other += 1;
                }
            }
        }
        let depth = |id: &str| depth_of(&parent, id, all.len());
        let mut ids: Vec<String> = sel.iter().map(|s| s.to_string()).collect();
        ids.sort_by(|a, b| depth(b).cmp(&depth(a)).then_with(|| a.cmp(b)));

        counts.sessions = ids.len();
        let roots: HashSet<&str> = all
            .iter()
            .filter(|(id, _, _)| sel.contains(id.as_str()))
            .map(|(_, w, _)| w.as_str())
            .collect();
        counts.workspaces = roots
            .iter()
            .filter(|w| {
                all.iter()
                    .filter(|(_, ww, _)| ww == *w)
                    .all(|(id, _, _)| sel.contains(id.as_str()))
            })
            .count();
        let mut events = conn.prepare("SELECT COUNT(*) FROM events WHERE session_id=?1")?;
        let mut msgs = conn.prepare("SELECT COUNT(*) FROM messages WHERE session_id=?1")?;
        let mut arts =
            conn.prepare("SELECT kind, mime, size FROM artifacts WHERE session_id=?1")?;
        for id in &ids {
            counts.events += events
                .query_row([id], |r| r.get::<_, i64>(0))?
                .unsigned_abs();
            counts.messages += msgs.query_row([id], |r| r.get::<_, i64>(0))?.unsigned_abs();
            let rows: Vec<(String, String, i64)> = arts
                .query_map([id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
                .collect::<std::result::Result<_, _>>()?;
            for (kind, mime, size) in rows {
                let size = size.unsigned_abs();
                if kind.contains("screenshot") || mime.starts_with("image/") {
                    counts.screenshot_bytes += size;
                } else if kind == "snapshot" {
                    counts.snapshot_bytes += size;
                } else if kind == "log" {
                    counts.log_bytes += size;
                }
            }
            counts.disk_bytes += dir_size(&sessions.join(id));
        }
        Ok(CleanPlan { ids, counts })
    }

    /// Deletes the sessions of `scope` that are still in the plan AND in `shown` (what the person
    /// confirmed). Each one is a transaction taken while its lock is held; its directory is removed
    /// only after the rows are gone. With `backup`, `synt.db` is copied first and a failed copy
    /// stops everything. Idempotent: a second run finds nothing.
    pub fn clean(
        &self,
        scope: CleanScope,
        keep: &[String],
        now: OffsetDateTime,
        shown: &[String],
        backup: bool,
    ) -> Result<CleanResult> {
        let sessions = self.home().join("sessions");
        if let Ok(m) = fs::symlink_metadata(&sessions)
            && !m.is_dir()
        {
            return Err(Error::Invalid(format!(
                "{} is not a real directory",
                sessions.display()
            )));
        }
        let plan = self.clean_plan(scope, keep, now)?;
        let shown: HashSet<&str> = shown.iter().map(String::as_str).collect();
        let ids: Vec<String> = plan
            .ids
            .into_iter()
            .filter(|i| shown.contains(i.as_str()))
            .collect();
        let mut res = CleanResult {
            skipped_running: plan.counts.skipped_running,
            skipped_other: plan.counts.skipped_other,
            ..CleanResult::default()
        };
        if ids.is_empty() {
            return Ok(res);
        }
        if backup {
            res.backup = Some(self.backup_db(now)?);
        }
        let mut conn = connect(self.home(), false)?;
        conn.busy_timeout(Duration::from_millis(5000))?;
        // freed pages are zeroed, not just unlinked
        conn.pragma_update(None, "secure_delete", true)?;
        let cutoff = scope.cutoff(now);
        let keep: HashSet<&str> = keep.iter().map(String::as_str).collect();
        for id in ids {
            let dir = sessions.join(&id);
            if keep.contains(id.as_str()) {
                res.skipped_running += 1;
                continue;
            }
            let lock = match probe(&dir) {
                Probe::Free(l) => l,
                Probe::Held => {
                    res.skipped_running += 1;
                    continue;
                }
                Probe::Unusual(why) => {
                    res.failed.push(format!("{id}: {why}"));
                    continue;
                }
            };
            let tx = match conn.transaction_with_behavior(TransactionBehavior::Immediate) {
                Ok(t) => t,
                Err(e) => {
                    res.failed.push(format!("{id}: {e}"));
                    continue;
                }
            };
            // a directory or lock that appeared since the probe may be a session starting now
            if lock.is_none() && !matches!(probe(&dir), Probe::Free(None)) {
                res.skipped_other += 1;
                continue;
            }
            // re-checked inside the write transaction
            let still: bool = tx.query_row(
                &format!(
                    "SELECT EXISTS (SELECT 1 FROM sessions s WHERE s.id=?2 AND {}) \
                     AND NOT EXISTS (SELECT 1 FROM sessions WHERE parent_session_id=?2)",
                    scope.pred()
                ),
                params![cutoff, id],
                |r| r.get(0),
            )?;
            if !still {
                res.skipped_other += 1;
                continue;
            }
            match delete_session_rows(&tx, &id)
                .map_err(|e| e.to_string())
                .and_then(|n| tx.commit().map(|()| n).map_err(|e| e.to_string()))
            {
                Ok(n) => res.events += n as u64,
                Err(e) => {
                    res.failed.push(format!("{id}: {e}"));
                    continue;
                }
            }
            res.deleted += 1;
            let bytes = dir_size(&dir);
            drop(lock); // Windows cannot remove an open file
            // remove_dir_all unlinks links inside; it never follows them
            match fs::remove_dir_all(&dir) {
                Ok(()) => res.freed_bytes += bytes,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => res.failed.push(format!(
                    "{id}: rows deleted, directory {} not removed: {e}",
                    dir.display()
                )),
            }
        }
        // fold the write-ahead log in so deleted pages are not left in it (best effort)
        let _ = conn
            .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(()))
            .optional();
        Ok(res)
    }

    /// A consistent copy of `synt.db` (rows only, no artifact files) at
    /// `$SYNT_HOME/backups/clean-<time>/synt.db`, mode 0600 in a 0700 directory. The copy is
    /// checked with `quick_check` before the path is returned.
    pub fn backup_db(&self, now: OffsetDateTime) -> Result<PathBuf> {
        let fmt = time::macros::format_description!(
            "[year][month][day]T[hour][minute][second][subsecond digits:3]Z"
        );
        let base = self.home().join("backups");
        let dir = base.join(format!(
            "clean-{}",
            now.format(&fmt).expect("format utc timestamp")
        ));
        private_dir(&base)?;
        #[cfg(unix)]
        {
            // an existing `backups/` must be ours (same owner as the home) and private
            use std::os::unix::fs::{MetadataExt, PermissionsExt};
            if fs::metadata(&base)?.uid() != fs::metadata(self.home())?.uid() {
                return Err(Error::Invalid(format!(
                    "{} is not owned by you",
                    base.display()
                )));
            }
            fs::set_permissions(&base, fs::Permissions::from_mode(0o700))?;
        }
        private_dir(&dir)?;
        let dest = dir.join("synt.db");
        // create_new: an existing file (or link) is an error, never written through
        private_file(OpenOptions::new().write(true).create_new(true)).open(&dest)?;
        let copy = (|| -> Result<()> {
            connect(self.home(), true)?.execute("VACUUM INTO ?1", [utf8(&dest)?])?;
            let c = rusqlite::Connection::open_with_flags(
                &dest,
                rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
            )?;
            let ok: String = c.query_row("PRAGMA quick_check", [], |r| r.get(0))?;
            if ok == "ok" {
                Ok(())
            } else {
                Err(Error::Invalid(format!(
                    "backup failed its integrity check: {ok}"
                )))
            }
        })();
        if let Err(e) = copy {
            let _ = fs::remove_file(&dest);
            return Err(e);
        }
        Ok(dest)
    }
}
