//! Remembered project trust, kept in the USER's session store (`synt.db`), never in a project file.
//!
//! One table, `trusted_roots`, created on first use with `CREATE TABLE IF NOT EXISTS` and NOT part of the
//! versioned migrations: `PRAGMA user_version` does not move, so an older binary opens the same database
//! (it ignores a table it does not know) instead of failing with `SchemaTooNew`.
//!
//! The database can be written by anything that can run a shell as the user, so every row carries an
//! HMAC-SHA256 over `root|scope|trusted_at|fingerprint` under a random 32-byte key in `<home>/run/trust.key`
//! (mode 0600, created with `O_EXCL`, never a link, never printed; `run/` is hidden from sandboxed commands
//! and the permission rules like the peer key). A row whose MAC does not verify is ignored and counted.
//!
//! A record names a canonical root, its scope (`folder`: exactly that root; `parent`: that root and what is
//! inside it; `child`: the baseline of one project first started under a parent grant), when it was given and
//! the manifest (per-file hashes) of the project's trusted configuration then. A changed manifest means the
//! remembered trust is NOT honoured: the project starts untrusted and the person is asked again.

use std::collections::BTreeMap;
use std::fs;
use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};

use hmac::{Hmac, Mac as _};
use rusqlite::{Connection, OptionalExtension as _};
use sha2::{Digest as _, Sha256};

use crate::error::{Error, Result};
use crate::store::{connect, now};

pub const FOLDER: &str = "folder";
pub const PARENT: &str = "parent";
pub const CHILD: &str = "child";

/// Rows kept, rows read, and the longest root accepted.
pub const MAX_ROWS: usize = 200;
pub const MAX_ROOT_LEN: usize = 4096;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrustRecord {
    pub root: String,
    /// [`FOLDER`], [`PARENT`] or [`CHILD`].
    pub scope: String,
    pub trusted_at: String,
    /// The manifest of [`fingerprint`] (empty for a parent grant).
    pub fingerprint: String,
}

/// The verified rows and how many were ignored (forged, tampered, or no key to check them with).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Listing {
    pub records: Vec<TrustRecord>,
    pub unverified: usize,
}

fn io(e: std::io::Error) -> Error {
    Error::Io(e)
}

// ---- policy: what may never be trusted

/// Where trust is refused, whatever a record says: the user's home folder (or a parent of it), Synthesis's own
/// config folder, a filesystem or drive root, a relative or empty root; and, for a parent grant, the generic
/// containers (temp folders, Downloads, Desktop, Documents, world-writable and root-owned system folders).
#[derive(Debug, Clone)]
pub struct Policy {
    pub home: Option<PathBuf>,
    pub synt_home: PathBuf,
}

fn canon(p: &Path) -> PathBuf {
    synthesis_core::canonicalize(p).unwrap_or_else(|_| p.to_path_buf())
}

impl Policy {
    pub fn root_refusal(&self, root: &Path) -> Option<&'static str> {
        if root.as_os_str().is_empty() || !root.is_absolute() {
            return Some("a relative or empty path");
        }
        let root = canon(root);
        if root.parent().is_none() {
            return Some("a filesystem root");
        }
        if self
            .home
            .as_deref()
            .is_some_and(|h| canon(h).starts_with(&root))
        {
            return Some("your home folder (or a parent of it)");
        }
        let synt = canon(&self.synt_home);
        if synt.starts_with(&root) || root.starts_with(&synt) {
            return Some("Synthesis's own config folder (or a parent of it)");
        }
        None
    }

    pub fn parent_refusal(&self, parent: &Path) -> Option<&'static str> {
        if let Some(why) = self.root_refusal(parent) {
            return Some(why);
        }
        let p = canon(parent);
        let tmp = canon(&std::env::temp_dir());
        if p == tmp
            || tmp.starts_with(&p)
            || [Path::new("/tmp"), Path::new("/var/tmp")]
                .iter()
                .any(|t| canon(t) == p)
        {
            return Some("a temporary folder");
        }
        if let Some(h) = &self.home {
            let h = canon(h);
            if ["Downloads", "Desktop", "Documents"]
                .iter()
                .any(|n| canon(&h.join(n)) == p)
            {
                return Some("a catch-all folder (Downloads, Desktop or Documents)");
            }
        }
        #[cfg(unix)]
        if let Ok(m) = fs::metadata(&p) {
            use std::os::unix::fs::MetadataExt as _;
            if m.mode() & 0o002 != 0 {
                return Some("a world-writable folder");
            }
            if m.uid() == 0 && rustix::process::geteuid().as_raw() != 0 {
                return Some("a system folder (owned by root)");
            }
        }
        None
    }

    fn allows(&self, r: &TrustRecord) -> bool {
        let root = Path::new(&r.root);
        match r.scope.as_str() {
            PARENT => self.parent_refusal(root).is_none(),
            FOLDER | CHILD => self.root_refusal(root).is_none(),
            _ => false,
        }
    }
}

// ---- the key and the MAC

fn key_path(home: &Path) -> PathBuf {
    home.join("run").join("trust.key")
}

/// The key, if the file is a regular file (not a link) of exactly 32 bytes.
fn read_key(home: &Path) -> Option<[u8; 32]> {
    let p = key_path(home);
    let m = fs::symlink_metadata(&p).ok()?;
    if !m.is_file() || m.len() != 32 {
        return None;
    }
    let mut b = [0u8; 32];
    fs::File::open(&p).ok()?.read_exact(&mut b).ok()?;
    Some(b)
}

/// Creates the key atomically (`O_EXCL`: a link or a file already there makes it fail), 0600.
fn create_key(home: &Path) -> Result<[u8; 32]> {
    let dir = home.join("run");
    fs::create_dir_all(&dir).map_err(io)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let _ = fs::set_permissions(&dir, fs::Permissions::from_mode(0o700));
    }
    let mut key = [0u8; 32];
    key[..16].copy_from_slice(uuid::Uuid::new_v4().as_bytes());
    key[16..].copy_from_slice(uuid::Uuid::new_v4().as_bytes());
    let mut o = fs::OpenOptions::new();
    o.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        o.mode(0o600)
            .custom_flags(rustix::fs::OFlags::NOFOLLOW.bits() as i32);
    }
    o.open(key_path(home))
        .and_then(|mut f| f.write_all(&key))
        .map_err(io)?;
    Ok(key)
}

fn mac_hex(key: &[u8; 32], r: &TrustRecord) -> String {
    let mut m = Hmac::<Sha256>::new_from_slice(key).expect("any key length");
    for f in [&r.root, &r.scope, &r.trusted_at, &r.fingerprint] {
        m.update(&(f.len() as u64).to_le_bytes());
        m.update(f.as_bytes());
    }
    m.finalize()
        .into_bytes()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn mac_ok(key: &[u8; 32], r: &TrustRecord, mac: &str) -> bool {
    // constant-time compare through the hmac verifier
    let Some(bytes) = (mac.len() == 64)
        .then(|| {
            (0..32)
                .map(|i| u8::from_str_radix(&mac[2 * i..2 * i + 2], 16).ok())
                .collect::<Option<Vec<u8>>>()
        })
        .flatten()
    else {
        return false;
    };
    let mut m = Hmac::<Sha256>::new_from_slice(key).expect("any key length");
    for f in [&r.root, &r.scope, &r.trusted_at, &r.fingerprint] {
        m.update(&(f.len() as u64).to_le_bytes());
        m.update(f.as_bytes());
    }
    m.verify_slice(&bytes).is_ok()
}

// ---- the table

fn ensure(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS trusted_roots(
           root TEXT PRIMARY KEY, scope TEXT NOT NULL, trusted_at TEXT NOT NULL, fingerprint TEXT NOT NULL,
           mac TEXT NOT NULL DEFAULT '');",
    )?;
    let has_mac = conn
        .prepare("PRAGMA table_info(trusted_roots)")?
        .query_map([], |r| r.get::<_, String>(1))?
        .any(|n| n.as_deref() == Ok("mac"));
    if !has_mac {
        conn.execute_batch("ALTER TABLE trusted_roots ADD COLUMN mac TEXT NOT NULL DEFAULT '';")?;
    }
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

/// Every remembered root whose MAC verifies, oldest first (at most [`MAX_ROWS`] rows are read), and how many
/// rows were ignored. No store, no table or no key: nothing is trusted (nothing is created).
pub fn list(home: &Path) -> Result<Listing> {
    if !home.join("synt.db").exists() {
        return Ok(Listing::default());
    }
    let conn = connect(home, true)?;
    if !has_table(&conn)? {
        return Ok(Listing::default());
    }
    let key = read_key(home);
    let mut st = conn.prepare(
        "SELECT root, scope, trusted_at, fingerprint, mac FROM trusted_roots ORDER BY trusted_at, root LIMIT ?1",
    )?;
    let rows = st.query_map([MAX_ROWS as i64 + 1], |r| {
        Ok((
            TrustRecord {
                root: r.get(0)?,
                scope: r.get(1)?,
                trusted_at: r.get(2)?,
                fingerprint: r.get(3)?,
            },
            r.get::<_, String>(4)?,
        ))
    })?;
    let mut out = Listing::default();
    for row in rows.take(MAX_ROWS + 1) {
        let (rec, mac) = row?;
        if out.records.len() + out.unverified >= MAX_ROWS {
            out.unverified += 1; // over the cap: not read
        } else if key.as_ref().is_some_and(|k| mac_ok(k, &rec, &mac)) {
            out.records.push(rec);
        } else {
            out.unverified += 1;
        }
    }
    Ok(out)
}

/// Remembers `root` (a canonical path) with `scope` and the manifest of its configuration now. Refused for a
/// root the [`Policy`] forbids, a root longer than [`MAX_ROOT_LEN`], or when [`MAX_ROWS`] rows exist already.
/// A missing key is made only when no row exists; rows that cannot be verified without it are removed first.
pub fn add(
    home: &Path,
    root: &Path,
    scope: &str,
    fingerprint: &str,
    policy: &Policy,
) -> Result<()> {
    let refusal = match scope {
        PARENT => policy.parent_refusal(root),
        FOLDER | CHILD => policy.root_refusal(root),
        _ => Some("an unknown scope"),
    };
    if let Some(why) = refusal {
        return Err(Error::Invalid(format!("trust refused for {why}")));
    }
    let root_s = root.to_string_lossy().into_owned();
    if root_s.len() > MAX_ROOT_LEN {
        return Err(Error::Invalid("trust refused: the path is too long".into()));
    }
    let conn = connect(home, false)?;
    ensure(&conn)?;
    let key = match read_key(home) {
        Some(k) => k,
        None => {
            // no key: any row cannot be checked, so none counts; start over rather than trust them
            conn.execute("DELETE FROM trusted_roots", [])?;
            create_key(home)?
        }
    };
    let n: i64 = conn.query_row("SELECT count(*) FROM trusted_roots", [], |r| r.get(0))?;
    let exists: bool = conn
        .query_row(
            "SELECT 1 FROM trusted_roots WHERE root=?1",
            [&root_s],
            |_| Ok(()),
        )
        .optional()?
        .is_some();
    if !exists && n as usize >= MAX_ROWS {
        return Err(Error::Invalid(format!(
            "trust refused: {MAX_ROWS} folders are remembered already (revoke some)"
        )));
    }
    let rec = TrustRecord {
        root: root_s,
        scope: scope.into(),
        trusted_at: now(),
        fingerprint: fingerprint.into(),
    };
    conn.execute(
        "INSERT INTO trusted_roots(root, scope, trusted_at, fingerprint, mac) VALUES (?1, ?2, ?3, ?4, ?5)
         ON CONFLICT(root) DO UPDATE SET scope=?2, trusted_at=?3, fingerprint=?4, mac=?5",
        (&rec.root, &rec.scope, &rec.trusted_at, &rec.fingerprint, mac_hex(&key, &rec)),
    )?;
    Ok(())
}

/// Forgets `root` (the exact string of its record), and the baseline rows of projects under it. Whether there
/// was a row. Needs no key: forgetting is always allowed.
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
/// that root, else a `parent` record for it or a folder above it. Records the [`Policy`] forbids are skipped (so
/// an empty root with the parent scope covers nothing). Components are compared (`/a/b` does not cover `/a/bc`),
/// exactly and case-sensitively, with the project path resolved first (so a link inside a trusted folder that
/// leads elsewhere is not covered, and a Windows verbatim prefix does not defeat the match); the records are not
/// resolved.
pub fn covers<'a>(
    records: &'a [TrustRecord],
    project_root: &Path,
    policy: &Policy,
) -> Option<&'a TrustRecord> {
    let real = canon(project_root);
    let ok = |r: &&TrustRecord| policy.allows(r);
    records
        .iter()
        .filter(ok)
        .find(|r| r.scope == FOLDER && real == Path::new(&r.root))
        .or_else(|| {
            records
                .iter()
                .filter(ok)
                .find(|r| r.scope == PARENT && real.starts_with(Path::new(&r.root)))
        })
}

/// The baseline of one project started under a parent grant.
pub fn child_of<'a>(
    records: &'a [TrustRecord],
    project_root: &Path,
    policy: &Policy,
) -> Option<&'a TrustRecord> {
    let real = canon(project_root);
    records
        .iter()
        .find(|r| r.scope == CHILD && real == Path::new(&r.root) && policy.allows(r))
}

// ---- the fingerprint

const MAX_FILE: u64 = 8 << 20;
const MAX_TOTAL: u64 = 64 << 20;
const MAX_FILES: usize = 500;
const MAX_DEPTH: u8 = 8;

/// Why a project's configuration cannot be fingerprinted: trust by fingerprint is then refused (and a
/// remembered trust counts as changed).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Unfit {
    TooBig,
    LinkOutside(String),
}

impl std::fmt::Display for Unfit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TooBig => write!(f, "the project configuration is too large to fingerprint"),
            Self::LinkOutside(n) => write!(f, "{n} is a link that leaves the project"),
        }
    }
}

struct Walk {
    root: PathBuf,
    out: BTreeMap<String, String>,
    bytes: u64,
}

fn hex(h: impl AsRef<[u8]>) -> String {
    h.as_ref().iter().map(|b| format!("{b:02x}")).collect()
}

impl Walk {
    fn entry(&mut self, name: String, path: &Path) -> std::result::Result<(), Unfit> {
        let Ok(m) = fs::symlink_metadata(path) else {
            return Ok(());
        };
        if self.out.len() >= MAX_FILES {
            return Err(Unfit::TooBig);
        }
        if m.file_type().is_symlink() {
            // not followed: the target path is what is hashed, and it must stay inside the project
            let target = fs::read_link(path).map_err(|_| Unfit::LinkOutside(name.clone()))?;
            let real = canon(path);
            if !real.starts_with(&self.root) {
                return Err(Unfit::LinkOutside(name));
            }
            self.out.insert(
                name,
                format!(
                    "link:{}",
                    hex(Sha256::digest(target.to_string_lossy().as_bytes()))
                ),
            );
        } else if m.is_dir() {
            // reached only through `dir`
        } else if m.is_file() {
            if m.len() > MAX_FILE || self.bytes + m.len() > MAX_TOTAL {
                return Err(Unfit::TooBig);
            }
            self.bytes += m.len();
            let mut h = Sha256::new();
            let mut f = fs::File::open(path).map_err(|_| Unfit::TooBig)?;
            let mut buf = [0u8; 64 * 1024];
            let mut left = MAX_FILE + 1;
            loop {
                let n = f.read(&mut buf).map_err(|_| Unfit::TooBig)?;
                if n == 0 {
                    break;
                }
                left = left.saturating_sub(n as u64);
                if left == 0 {
                    return Err(Unfit::TooBig);
                }
                h.update(&buf[..n]);
            }
            self.out.insert(name, hex(h.finalize()));
        }
        Ok(())
    }

    fn dir(&mut self, dir: &Path, rel: &str, depth: u8) -> std::result::Result<(), Unfit> {
        let Ok(rd) = fs::read_dir(dir) else {
            return Ok(());
        };
        let mut entries: Vec<_> = rd.filter_map(|e| e.ok()).collect();
        entries.sort_by_key(|e| e.file_name());
        for e in entries {
            let name = format!("{rel}/{}", e.file_name().to_string_lossy());
            let Ok(t) = e.file_type() else { continue };
            if t.is_dir() {
                if depth >= MAX_DEPTH {
                    return Err(Unfit::TooBig);
                }
                self.dir(&e.path(), &name, depth + 1)?;
            } else {
                self.entry(name, &e.path())?;
            }
        }
        Ok(())
    }
}

/// The manifest (`name\thash\n` lines, sorted) of what a trusted project can switch on: `.synt/settings*.json`
/// (permission rules, hooks, MCP), `.synt/config.json`, `.mcp.json`, `AGENTS.md`, `.synt/AGENTS.md` and every
/// file under `.synt/agents` and `.synt/skills` (depth 8, at most 500 files, 8 MiB each and 64 MiB in all,
/// hashed in streaming mode). Links are not followed: their target path is hashed and a link that leaves the
/// project is an error, like exceeding a cap. Missing files are absent, so an untouched project always gives
/// the same manifest.
pub fn fingerprint(root: &Path) -> std::result::Result<String, Unfit> {
    let mut w = Walk {
        root: canon(root),
        out: BTreeMap::new(),
        bytes: 0,
    };
    for rel in [
        ".mcp.json",
        "AGENTS.md",
        ".synt/AGENTS.md",
        ".synt/config.json",
        ".synt/settings.json",
        ".synt/settings.local.json",
    ] {
        w.entry(rel.to_string(), &root.join(rel))?;
    }
    for d in ["agents", "skills"] {
        w.dir(&root.join(".synt").join(d), &format!(".synt/{d}"), 0)?;
    }
    Ok(w.out.iter().map(|(k, v)| format!("{k}\t{v}\n")).collect())
}

/// The file names that differ between two manifests (added, removed or changed), sorted.
pub fn changed_files(old: &str, new: &str) -> Vec<String> {
    let parse = |s: &str| -> BTreeMap<String, String> {
        s.lines()
            .filter_map(|l| l.split_once('\t'))
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    };
    let (a, b) = (parse(old), parse(new));
    let mut names: Vec<String> = a
        .keys()
        .chain(b.keys())
        .filter(|k| a.get(*k) != b.get(*k))
        .cloned()
        .collect();
    names.sort();
    names.dedup();
    names
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Store, StoreOptions};

    fn pol(home: &Path) -> Policy {
        Policy {
            home: Some(home.parent().unwrap().join("userhome")),
            synt_home: home.to_path_buf(),
        }
    }

    fn rec(root: &str, scope: &str) -> TrustRecord {
        TrustRecord {
            root: root.into(),
            scope: scope.into(),
            trusted_at: "t".into(),
            fingerprint: "f".into(),
        }
    }

    fn store_home() -> (tempfile::TempDir, PathBuf) {
        let d = tempfile::tempdir().unwrap();
        let home = fs::canonicalize(d.path()).unwrap().join("synthome");
        fs::create_dir_all(&home).unwrap();
        Store::open(&home, StoreOptions::default()).unwrap();
        (d, home)
    }

    fn project(home: &Path, name: &str) -> PathBuf {
        let p = home.parent().unwrap().join("work").join(name);
        fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn a_folder_grant_covers_that_root_only_and_a_parent_grant_what_is_inside() {
        let pl = Policy {
            home: None,
            synt_home: PathBuf::from("/nonexistent/synt"),
        };
        let rs = [rec("/w/app", FOLDER), rec("/p/all", PARENT)];
        let c = |p: &str| covers(&rs, Path::new(p), &pl).is_some();
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

    #[test]
    fn records_with_a_forbidden_root_cover_nothing() {
        let (d, home) = store_home();
        let pl = pol(&home);
        let base = fs::canonicalize(d.path()).unwrap();
        fs::create_dir_all(base.join("userhome")).unwrap();
        // an empty root with the parent scope used to match everything
        let rs = [
            rec("", PARENT),
            rec("relative/path", FOLDER),
            rec("/", PARENT),
            rec(&home.to_string_lossy(), FOLDER), // the config folder
            rec(&base.join("userhome").to_string_lossy(), FOLDER), // $HOME
            rec(&base.to_string_lossy(), PARENT), // an ancestor of $HOME
        ];
        for p in [
            "/anything",
            "/",
            &home.to_string_lossy(),
            &base.join("userhome").to_string_lossy(),
        ] {
            assert!(covers(&rs, Path::new(p), &pl).is_none(), "{p}");
        }
        assert!(add(&home, Path::new(""), PARENT, "", &pl).is_err());
        assert!(add(&home, Path::new("/"), FOLDER, "", &pl).is_err());
        assert!(add(&home, &base.join("userhome"), FOLDER, "", &pl).is_err());
        assert!(add(&home, &home, FOLDER, "", &pl).is_err());
        assert!(add(&home, Path::new("rel"), FOLDER, "", &pl).is_err());
        assert!(list(&home).unwrap().records.is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn a_parent_grant_is_refused_for_generic_containers() {
        use std::os::unix::fs::PermissionsExt as _;
        let (d, home) = store_home();
        let uh = d.path().join("userhome");
        for n in ["Downloads", "Desktop", "Documents", "src"] {
            fs::create_dir_all(uh.join(n)).unwrap();
        }
        let pl = pol(&home);
        let why = |p: &Path| pl.parent_refusal(p);
        assert!(why(Path::new("/tmp")).is_some() && why(Path::new("/var/tmp")).is_some());
        assert!(why(&std::env::temp_dir()).is_some());
        for n in ["Downloads", "Desktop", "Documents"] {
            assert!(why(&uh.join(n)).unwrap().contains("catch-all"), "{n}");
        }
        assert!(why(&uh.join("src")).is_none(), "a project parent is fine");
        let ww = d.path().join("open");
        fs::create_dir_all(&ww).unwrap();
        fs::set_permissions(&ww, fs::Permissions::from_mode(0o777)).unwrap();
        assert!(why(&ww).unwrap().contains("world-writable"));
        // a folder grant in a temp folder is not a container problem
        assert!(pl.root_refusal(&uh.join("src")).is_none());
        // the stored record is skipped too, whatever its MAC says
        let rs = [rec(&ww.to_string_lossy(), PARENT)];
        assert!(covers(&rs, &ww.join("x"), &pl).is_none());
    }

    #[cfg(unix)]
    #[test]
    fn a_link_that_leaves_the_trusted_parent_is_not_covered() {
        let (d, home) = store_home();
        let base = fs::canonicalize(d.path()).unwrap();
        let (parent, outside) = (base.join("parent"), base.join("outside"));
        fs::create_dir_all(parent.join("real")).unwrap();
        fs::create_dir_all(&outside).unwrap();
        std::os::unix::fs::symlink(&outside, parent.join("link")).unwrap();
        let pl = pol(&home);
        let rs = [rec(&parent.to_string_lossy(), PARENT)];
        assert!(covers(&rs, &parent.join("real"), &pl).is_some());
        assert!(covers(&rs, &parent.join("link"), &pl).is_none());
        let rs = [rec(&parent.join("link").to_string_lossy(), FOLDER)];
        assert!(covers(&rs, &outside, &pl).is_none());
    }

    #[test]
    fn remember_and_revoke_round_trip_and_user_version_does_not_move() {
        let (d, home) = store_home();
        let pl = pol(&home);
        let v = |h: &Path| -> i64 {
            connect(h, true)
                .unwrap()
                .query_row("PRAGMA user_version", [], |r| r.get(0))
                .unwrap()
        };
        let before = v(&home);
        assert_eq!(list(&home).unwrap(), Listing::default());
        let (a, b) = (project(&home, "app"), project(&home, "all"));
        add(&home, &a, FOLDER, "fp1", &pl).unwrap();
        add(&home, &b, PARENT, "", &pl).unwrap();
        add(&home, &a, FOLDER, "fp3", &pl).unwrap(); // again: updated, not duplicated
        let l = list(&home).unwrap();
        assert_eq!((l.records.len(), l.unverified), (2, 0));
        assert_eq!(
            l.records
                .iter()
                .find(|r| r.root == a.to_string_lossy())
                .unwrap()
                .fingerprint,
            "fp3"
        );
        assert_eq!(
            v(&home),
            before,
            "no schema bump: older binaries still open the store"
        );
        assert_eq!(before, crate::SCHEMA_VERSION_LATEST);
        assert!(revoke(&home, &a.to_string_lossy()).unwrap());
        assert!(!revoke(&home, &a.to_string_lossy()).unwrap());
        assert_eq!(list(&home).unwrap().records.len(), 1);
        Store::open(&home, StoreOptions::default()).unwrap();
        drop(d);
    }

    #[test]
    fn nothing_is_listed_or_created_without_a_store() {
        let d = tempfile::tempdir().unwrap();
        assert_eq!(list(d.path()).unwrap(), Listing::default());
        assert!(!d.path().join("synt.db").exists());
    }

    #[test]
    fn a_forged_row_and_a_tampered_row_are_ignored_and_counted() {
        let (_d, home) = store_home();
        let pl = pol(&home);
        let (a, forged) = (project(&home, "app"), project(&home, "forged"));
        add(&home, &a, FOLDER, "fp", &pl).unwrap();
        let conn = connect(&home, false).unwrap();
        // a row inserted by something that can write the database but has no key: no MAC, a made-up MAC
        conn.execute(
            "INSERT INTO trusted_roots(root, scope, trusted_at, fingerprint, mac) VALUES (?1,'folder','2026-01-01T00:00:00Z','',?2)",
            (forged.to_string_lossy().as_ref(), "0".repeat(64)),
        )
        .unwrap();
        conn.execute(
            "INSERT INTO trusted_roots(root, scope, trusted_at, fingerprint) VALUES ('/x/nomac','parent','t','')",
            [],
        )
        .unwrap();
        let l = list(&home).unwrap();
        assert_eq!(l.records.len(), 1);
        assert_eq!(l.unverified, 2);
        assert!(covers(&l.records, &forged, &pl).is_none());
        // tampering with a genuine row (scope widened, fingerprint reset) breaks its MAC
        conn.execute(
            "UPDATE trusted_roots SET scope='parent' WHERE root=?1",
            [a.to_string_lossy().as_ref()],
        )
        .unwrap();
        let l = list(&home).unwrap();
        assert_eq!((l.records.len(), l.unverified), (0, 3));
        conn.execute(
            "UPDATE trusted_roots SET scope='folder', fingerprint='x' WHERE root=?1",
            [a.to_string_lossy().as_ref()],
        )
        .unwrap();
        assert_eq!(
            list(&home).unwrap().records.len(),
            0,
            "a changed fingerprint also breaks it"
        );
    }

    #[test]
    fn a_missing_key_is_made_only_when_no_row_exists_otherwise_the_rows_are_invalid() {
        let (_d, home) = store_home();
        let pl = pol(&home);
        let (a, b) = (project(&home, "app"), project(&home, "other"));
        assert!(!key_path(&home).exists());
        add(&home, &a, FOLDER, "fp", &pl).unwrap();
        assert!(key_path(&home).exists(), "made with the first row");
        let k1 = fs::read(key_path(&home)).unwrap();
        add(&home, &b, FOLDER, "fp", &pl).unwrap();
        assert_eq!(
            fs::read(key_path(&home)).unwrap(),
            k1,
            "not rotated while it exists"
        );
        // the key vanishes: the rows cannot be verified, so none is honoured
        fs::remove_file(key_path(&home)).unwrap();
        let l = list(&home).unwrap();
        assert_eq!((l.records.len(), l.unverified), (0, 2));
        // making a new grant never revives them: they are dropped, the new key signs only the new row
        add(&home, &a, FOLDER, "fp", &pl).unwrap();
        let l = list(&home).unwrap();
        assert_eq!((l.records.len(), l.unverified), (1, 0));
        assert_ne!(fs::read(key_path(&home)).unwrap(), k1);
    }

    #[cfg(unix)]
    #[test]
    fn the_key_is_private_never_a_link_and_a_bad_file_is_not_a_key() {
        use std::os::unix::fs::PermissionsExt as _;
        let (d, home) = store_home();
        let pl = pol(&home);
        let a = project(&home, "app");
        add(&home, &a, FOLDER, "fp", &pl).unwrap();
        let m = fs::metadata(key_path(&home)).unwrap();
        assert_eq!(m.permissions().mode() & 0o777, 0o600);
        assert_eq!(m.len(), 32);
        // a link in the key's place is not read, and not created over
        let real = d.path().join("elsewhere.key");
        fs::write(&real, [7u8; 32]).unwrap();
        fs::remove_file(key_path(&home)).unwrap();
        std::os::unix::fs::symlink(&real, key_path(&home)).unwrap();
        assert!(read_key(&home).is_none());
        assert!(
            add(&home, &a, FOLDER, "fp", &pl).is_err(),
            "O_EXCL refuses the link"
        );
        // a short file is not a key either
        fs::remove_file(key_path(&home)).unwrap();
        fs::write(key_path(&home), b"short").unwrap();
        assert!(read_key(&home).is_none());
    }

    #[test]
    fn rows_and_roots_are_capped() {
        let (_d, home) = store_home();
        let pl = pol(&home);
        let long = PathBuf::from(format!("/{}", "a".repeat(MAX_ROOT_LEN)));
        assert!(add(&home, &long, FOLDER, "", &pl).is_err());
        let base = project(&home, "many");
        for i in 0..MAX_ROWS {
            let p = base.join(format!("p{i}"));
            add(&home, &p, FOLDER, "", &pl).unwrap();
        }
        assert!(add(&home, &base.join("one-more"), FOLDER, "", &pl).is_err());
        // an update of an existing row is still fine at the cap
        add(&home, &base.join("p0"), FOLDER, "x", &pl).unwrap();
        assert_eq!(list(&home).unwrap().records.len(), MAX_ROWS);
    }

    #[test]
    fn a_store_at_an_older_schema_still_opens_and_can_remember() {
        let d = tempfile::tempdir().unwrap();
        let home = fs::canonicalize(d.path()).unwrap().join("h");
        fs::create_dir_all(&home).unwrap();
        {
            let c = connect(&home, false).unwrap();
            c.execute_batch("CREATE TABLE schema_migrations(version INTEGER PRIMARY KEY, applied_at TEXT NOT NULL);")
                .unwrap();
        }
        let a = project(&home, "app");
        add(&home, &a, FOLDER, "f", &pol(&home)).unwrap();
        assert_eq!(list(&home).unwrap().records.len(), 1);
        let v: i64 = connect(&home, true)
            .unwrap()
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(v, 0, "remembering never touches the schema version");
    }

    #[test]
    fn the_fingerprint_follows_the_trusted_configuration_and_names_what_changed() {
        let d = tempfile::tempdir().unwrap();
        let r = d.path();
        let f0 = fingerprint(r).unwrap();
        assert_eq!(f0, fingerprint(r).unwrap());
        fs::write(r.join("README.md"), "x").unwrap();
        assert_eq!(fingerprint(r).unwrap(), f0, "ordinary files do not count");
        fs::create_dir_all(r.join(".synt/agents")).unwrap();
        fs::write(r.join(".synt/settings.json"), "{}").unwrap();
        let f1 = fingerprint(r).unwrap();
        assert_eq!(changed_files(&f0, &f1), [".synt/settings.json"]);
        fs::write(r.join("AGENTS.md"), "rules").unwrap();
        let f2 = fingerprint(r).unwrap();
        assert_eq!(
            changed_files(&f1, &f2),
            ["AGENTS.md"],
            "instruction files count"
        );
        fs::write(r.join(".synt/agents/a.md"), "one").unwrap();
        fs::write(r.join(".mcp.json"), "{}").unwrap();
        let f3 = fingerprint(r).unwrap();
        fs::write(r.join(".synt/agents/a.md"), "two").unwrap();
        fs::remove_file(r.join(".mcp.json")).unwrap();
        assert_eq!(
            changed_files(&f3, &fingerprint(r).unwrap()),
            [".mcp.json", ".synt/agents/a.md"]
        );
        // deep files count (to a bounded depth)
        let deep = (0..6).fold(r.join(".synt/skills"), |p, i| p.join(format!("d{i}")));
        fs::create_dir_all(&deep).unwrap();
        let before = fingerprint(r).unwrap();
        fs::write(deep.join("s.md"), "x").unwrap();
        assert_ne!(fingerprint(r).unwrap(), before);
        let too_deep = (0..10).fold(r.join(".synt/skills/z"), |p, i| p.join(format!("d{i}")));
        fs::create_dir_all(&too_deep).unwrap();
        assert_eq!(
            fingerprint(r),
            Err(Unfit::TooBig),
            "beyond the depth bound: no trust by fingerprint"
        );
    }

    #[test]
    fn a_big_file_is_refused_not_truncated() {
        let d = tempfile::tempdir().unwrap();
        fs::create_dir_all(d.path().join(".synt")).unwrap();
        let f = fs::File::create(d.path().join(".synt/settings.json")).unwrap();
        f.set_len(MAX_FILE + 1).unwrap();
        assert_eq!(fingerprint(d.path()), Err(Unfit::TooBig));
    }

    #[cfg(unix)]
    #[test]
    fn a_link_in_the_configuration_is_hashed_by_target_and_one_that_leaves_the_project_is_refused()
    {
        let d = tempfile::tempdir().unwrap();
        let r = fs::canonicalize(d.path()).unwrap();
        fs::create_dir_all(r.join(".synt")).unwrap();
        fs::write(r.join("rules.md"), "a").unwrap();
        std::os::unix::fs::symlink(r.join("rules.md"), r.join("AGENTS.md")).unwrap();
        let f1 = fingerprint(&r).unwrap();
        fs::write(r.join("rules.md"), "b").unwrap();
        assert_eq!(
            fingerprint(&r).unwrap(),
            f1,
            "the link's own target path is hashed, not followed"
        );
        fs::remove_file(r.join("AGENTS.md")).unwrap();
        let out = tempfile::tempdir().unwrap();
        fs::write(out.path().join("x.md"), "x").unwrap();
        std::os::unix::fs::symlink(out.path().join("x.md"), r.join("AGENTS.md")).unwrap();
        assert!(matches!(fingerprint(&r), Err(Unfit::LinkOutside(_))));
    }
}
