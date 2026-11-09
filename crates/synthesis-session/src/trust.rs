//! Remembered project trust, kept in the USER's session store (`synt.db`), never in a project file.
//!
//! One table, `trusted_roots`, created on first use with `CREATE TABLE IF NOT EXISTS` and NOT part of the
//! versioned migrations: `PRAGMA user_version` does not move, so an older binary opens the same database
//! (it ignores a table it does not know) instead of failing with `SchemaTooNew`.
//!
//! A record names a canonical root, its scope (`folder`: that root; `parent`: that root and everything
//! under it), when it was given and a fingerprint of the project's trusted configuration at that time.
//! The fingerprint (kept for `folder` records; a `parent` record covers many projects and keeps none) is only a
//! change warning: trust is not withdrawn when the files change.

use std::fs;
use std::io::Read as _;
use std::path::Path;

use rusqlite::{Connection, OptionalExtension as _};
use sha2::{Digest as _, Sha256};

use crate::error::Result;
use crate::store::{connect, now};

pub const FOLDER: &str = "folder";
pub const PARENT: &str = "parent";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrustRecord {
    pub root: String,
    /// [`FOLDER`] or [`PARENT`].
    pub scope: String,
    pub trusted_at: String,
    pub fingerprint: String,
}

fn ensure(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS trusted_roots(
           root TEXT PRIMARY KEY, scope TEXT NOT NULL, trusted_at TEXT NOT NULL, fingerprint TEXT NOT NULL);",
    )?;
    Ok(())
}

fn has_table(conn: &Connection) -> Result<bool> {
    Ok(conn
        .query_row(
            "SELECT 1 FROM sqlite_master WHERE type='table' AND name='trusted_roots'",
            [],
            |_| Ok(()),
        )
        .optional()?
        .is_some())
}

/// Every remembered root, oldest first. No store or no table yet: none (nothing is created).
pub fn list(home: &Path) -> Result<Vec<TrustRecord>> {
    if !home.join("synt.db").exists() {
        return Ok(vec![]);
    }
    let conn = connect(home, true)?;
    if !has_table(&conn)? {
        return Ok(vec![]);
    }
    let mut st = conn.prepare(
        "SELECT root, scope, trusted_at, fingerprint FROM trusted_roots ORDER BY trusted_at, root",
    )?;
    let rows = st.query_map([], |r| {
        Ok(TrustRecord {
            root: r.get(0)?,
            scope: r.get(1)?,
            trusted_at: r.get(2)?,
            fingerprint: r.get(3)?,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// Remembers `root` (a canonical path) with `scope` and the fingerprint of its configuration now.
pub fn add(home: &Path, root: &Path, scope: &str, fingerprint: &str) -> Result<()> {
    debug_assert!(scope == FOLDER || scope == PARENT);
    let conn = connect(home, false)?;
    ensure(&conn)?;
    conn.execute(
        "INSERT INTO trusted_roots(root, scope, trusted_at, fingerprint) VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT(root) DO UPDATE SET scope=?2, trusted_at=?3, fingerprint=?4",
        (root.to_string_lossy().as_ref(), scope, now(), fingerprint),
    )?;
    Ok(())
}

/// Forgets `root` (the exact string of its record). Whether there was one.
pub fn revoke(home: &Path, root: &str) -> Result<bool> {
    if !home.join("synt.db").exists() {
        return Ok(false);
    }
    let conn = connect(home, false)?;
    if !has_table(&conn)? {
        return Ok(false);
    }
    Ok(conn.execute("DELETE FROM trusted_roots WHERE root=?1", [root])? > 0)
}

/// The record that trusts `project_root` (its canonical project root), if any: a `folder` record for exactly
/// that root, or a `parent` record for it or a folder above it. Components are compared (`/a/b` does not
/// cover `/a/bc`), exactly and case-sensitively, and the project path is resolved first so a link inside a
/// trusted folder that leads elsewhere is not covered. The records are not resolved: a recorded path that
/// has since become a link to another place trusts nothing.
pub fn covers<'a>(records: &'a [TrustRecord], project_root: &Path) -> Option<&'a TrustRecord> {
    let real = fs::canonicalize(project_root).unwrap_or_else(|_| project_root.to_path_buf());
    records.iter().find(|r| {
        let root = Path::new(&r.root);
        match r.scope.as_str() {
            FOLDER => real == root,
            PARENT => real.starts_with(root),
            _ => false,
        }
    })
}

const MAX_FILE: u64 = 1 << 20;
const MAX_FILES: usize = 200;

fn walk(dir: &Path, rel: &str, depth: u8, out: &mut Vec<(String, std::path::PathBuf)>) {
    let Ok(rd) = fs::read_dir(dir) else { return };
    let mut entries: Vec<_> = rd.filter_map(|e| e.ok()).collect();
    entries.sort_by_key(|e| e.file_name());
    for e in entries {
        let Ok(t) = e.file_type() else { continue };
        let name = format!("{rel}/{}", e.file_name().to_string_lossy());
        if t.is_symlink() {
            continue;
        }
        if t.is_dir() && depth < 4 {
            walk(&e.path(), &name, depth + 1, out);
        } else if t.is_file() && out.len() < MAX_FILES {
            out.push((name, e.path()));
        }
    }
}

/// A SHA-256 (hex) of the configuration a trusted project can switch on: `.synt/settings*.json` (permission
/// rules, hooks, MCP), `.synt/config.json`, `.mcp.json`, and the files under `.synt/agents` and
/// `.synt/skills` (names and contents; links are not followed, at most 200 files of 1 MiB). Missing files
/// count as absent, so an untouched project always gives the same value.
pub fn fingerprint(root: &Path) -> String {
    let mut files: Vec<(String, std::path::PathBuf)> = vec![];
    for rel in [
        ".mcp.json",
        ".synt/config.json",
        ".synt/settings.json",
        ".synt/settings.local.json",
    ] {
        let p = root.join(rel);
        if p.is_file() {
            files.push((rel.to_string(), p));
        }
    }
    for d in ["agents", "skills"] {
        walk(
            &root.join(".synt").join(d),
            &format!(".synt/{d}"),
            0,
            &mut files,
        );
    }
    let mut h = Sha256::new();
    for (name, path) in files {
        h.update(name.as_bytes());
        h.update([0]);
        let mut buf = Vec::new();
        if let Ok(f) = fs::File::open(&path) {
            let _ = f.take(MAX_FILE).read_to_end(&mut buf);
        }
        h.update((buf.len() as u64).to_le_bytes());
        h.update(&buf);
    }
    h.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Store, StoreOptions};

    fn rec(root: &str, scope: &str) -> TrustRecord {
        TrustRecord {
            root: root.into(),
            scope: scope.into(),
            trusted_at: "t".into(),
            fingerprint: "f".into(),
        }
    }

    #[test]
    fn a_folder_grant_covers_that_root_only_and_a_parent_grant_what_is_inside() {
        let rs = [rec("/w/app", FOLDER), rec("/p/all", PARENT)];
        let c = |p: &str| covers(&rs, Path::new(p)).is_some();
        assert!(c("/w/app"));
        assert!(
            !c("/w/app/sub"),
            "a folder grant is not inherited by a subfolder"
        );
        assert!(
            !c("/w/app2") && !c("/w") && !c("/w/other"),
            "siblings and parents are not covered"
        );
        assert!(!c("/W/APP"), "exact, case-sensitive");
        assert!(c("/p/all") && c("/p/all/x") && c("/p/all/x/y"));
        assert!(
            !c("/p/allx") && !c("/p") && !c("/p/other"),
            "a parent grant stops at its folder"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_link_that_leaves_the_trusted_parent_is_not_covered() {
        let d = tempfile::tempdir().unwrap();
        let base = fs::canonicalize(d.path()).unwrap();
        let (parent, outside) = (base.join("parent"), base.join("outside"));
        fs::create_dir_all(parent.join("real")).unwrap();
        fs::create_dir_all(&outside).unwrap();
        std::os::unix::fs::symlink(&outside, parent.join("link")).unwrap();
        let rs = [rec(&parent.to_string_lossy(), PARENT)];
        assert!(covers(&rs, &parent.join("real")).is_some());
        assert!(covers(&rs, &parent.join("link")).is_none());
        // a recorded root that is itself a link to elsewhere trusts nothing under the target
        let rs = [rec(&parent.join("link").to_string_lossy(), FOLDER)];
        assert!(covers(&rs, &outside).is_none());
    }

    #[test]
    fn remember_and_revoke_round_trip_on_a_home_and_user_version_does_not_move() {
        let d = tempfile::tempdir().unwrap();
        let home = d.path();
        assert_eq!(
            list(home).unwrap(),
            vec![],
            "no store: nothing remembered, nothing created"
        );
        assert!(!home.join("synt.db").exists());
        let store = Store::open(home, StoreOptions::default()).unwrap();
        let v = |h: &Path| -> i64 {
            connect(h, true)
                .unwrap()
                .query_row("PRAGMA user_version", [], |r| r.get(0))
                .unwrap()
        };
        let before = v(home);
        assert_eq!(list(home).unwrap(), vec![]);
        add(home, Path::new("/w/app"), FOLDER, "fp1").unwrap();
        add(home, Path::new("/p/all"), PARENT, "fp2").unwrap();
        add(home, Path::new("/w/app"), FOLDER, "fp3").unwrap(); // again: updated, not duplicated
        let l = list(home).unwrap();
        assert_eq!(l.len(), 2);
        assert_eq!(
            l.iter().find(|r| r.root == "/w/app").unwrap().fingerprint,
            "fp3"
        );
        assert_eq!(
            v(home),
            before,
            "no schema bump: older binaries still open the store"
        );
        assert_eq!(before, crate::SCHEMA_VERSION_LATEST);
        assert!(revoke(home, "/w/app").unwrap());
        assert!(!revoke(home, "/w/app").unwrap());
        assert_eq!(list(home).unwrap().len(), 1);
        // the store keeps working with the extra table, as an older binary would see it
        drop(store);
        Store::open(home, StoreOptions::default()).unwrap();
    }

    #[test]
    fn a_store_at_an_older_schema_still_opens_and_can_remember() {
        // an older binary's database: user_version below the latest, no `trusted_roots`
        let d = tempfile::tempdir().unwrap();
        {
            let c = connect(d.path(), false).unwrap();
            c.execute_batch("CREATE TABLE schema_migrations(version INTEGER PRIMARY KEY, applied_at TEXT NOT NULL);").unwrap();
        }
        add(d.path(), Path::new("/w/app"), FOLDER, "f").unwrap();
        assert_eq!(list(d.path()).unwrap().len(), 1);
        let v: i64 = connect(d.path(), true)
            .unwrap()
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(v, 0, "remembering never touches the schema version");
    }

    #[test]
    fn the_fingerprint_follows_the_trusted_configuration_only() {
        let d = tempfile::tempdir().unwrap();
        let r = d.path();
        let f0 = fingerprint(r);
        assert_eq!(f0, fingerprint(r));
        fs::write(r.join("README.md"), "x").unwrap();
        assert_eq!(fingerprint(r), f0, "ordinary files do not count");
        fs::create_dir_all(r.join(".synt/agents")).unwrap();
        fs::write(r.join(".synt/settings.json"), "{}").unwrap();
        let f1 = fingerprint(r);
        assert_ne!(f1, f0);
        fs::write(r.join(".synt/agents/a.md"), "one").unwrap();
        let f2 = fingerprint(r);
        assert_ne!(f2, f1);
        fs::write(r.join(".synt/agents/a.md"), "two").unwrap();
        assert_ne!(fingerprint(r), f2);
        fs::write(r.join(".mcp.json"), "{}").unwrap();
        let f3 = fingerprint(r);
        fs::write(r.join(".mcp.json"), "{ }").unwrap();
        assert_ne!(fingerprint(r), f3);
    }
}
