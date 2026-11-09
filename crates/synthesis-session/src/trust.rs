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

/// Equal paths; case-insensitively on macOS and Windows, whose volumes are case-insensitive by default.
fn same(a: &Path, b: &Path) -> bool {
    if cfg!(any(target_os = "macos", windows)) {
        a.to_string_lossy().to_lowercase() == b.to_string_lossy().to_lowercase()
    } else {
        a == b
    }
}

/// The real place of `p` (a path that does not exist yet keeps its missing tail after the real place of its
/// nearest existing folder), without the Windows verbatim prefix, so two spellings of one place compare equal.
fn canon(p: &Path) -> PathBuf {
    synthesis_core::canonicalize(p)
        .ok()
        .or_else(|| synthesis_core::realpath_lenient(p))
        .unwrap_or_else(|| p.to_path_buf())
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
        // an unknown home folder cannot be told apart from a folder inside it: refuse
        let Some(home) = &self.home else {
            return Some("a folder while the home folder is unknown");
        };
        if canon(home).starts_with(&root) {
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
        if same(&p, &tmp)
            || tmp.starts_with(&p)
            || [Path::new("/tmp"), Path::new("/var/tmp")]
                .iter()
                .any(|t| same(&canon(t), &p))
        {
            return Some("a temporary folder");
        }
        if let Some(h) = &self.home {
            let h = canon(h);
            if ["Downloads", "Desktop", "Documents"]
                .iter()
                .any(|n| same(&canon(&h.join(n)), &p))
            {
                return Some("a catch-all folder (Downloads, Desktop or Documents)");
            }
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt as _;
            if let Ok(m) = fs::metadata(&p) {
                // others can add entries here, sticky bit or not
                if m.mode() & 0o002 != 0 {
                    return Some("a world-writable folder");
                }
                if m.uid() == 0 && rustix::process::geteuid().as_raw() != 0 {
                    return Some("a system folder (owned by root)");
                }
            }
            // a few levels up: another user could swap this folder if an ancestor lets them rename it
            // (world-writable without the sticky bit)
            for a in p.ancestors().skip(1).take(4) {
                if let Ok(m) = fs::metadata(a)
                    && m.mode() & 0o002 != 0
                    && m.mode() & 0o1000 == 0
                {
                    return Some("a folder under a world-writable folder");
                }
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
    open_regular(&p)?.read_exact(&mut b).ok()?;
    Some(b)
}

/// Opens `p` for reading only if it is a regular file: opened without blocking and checked on the open
/// handle, so a path swapped for a FIFO (or a device) between a check and the open cannot hang the reader.
fn open_regular(p: &Path) -> Option<fs::File> {
    let mut o = fs::OpenOptions::new();
    o.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        o.custom_flags(rustix::fs::OFlags::NONBLOCK.bits() as i32);
    }
    let f = o.open(p).ok()?;
    f.metadata().ok()?.is_file().then_some(f)
}

/// Creates the key without ever replacing one: a 0600 temporary file is written and synced, then hard-linked
/// to the key's name (the link fails if the name exists, a link included), then the temporary is removed.
/// `Ok(Some)`: this call made the key; `Ok(None)`: another one was there first (read it).
fn create_key(home: &Path) -> Result<Option<[u8; 32]>> {
    let dir = home.join("run");
    fs::create_dir_all(&dir).map_err(io)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let _ = fs::set_permissions(&dir, fs::Permissions::from_mode(0o700));
    }
    let mut key = [0u8; 32];
    getrandom::getrandom(&mut key).map_err(|e| Error::Invalid(format!("no random bytes: {e}")))?;
    let tmp = dir.join(format!(".trust.key.{}", uuid::Uuid::new_v4().simple()));
    let mut o = fs::OpenOptions::new();
    o.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        o.mode(0o600)
            .custom_flags(rustix::fs::OFlags::NOFOLLOW.bits() as i32);
    }
    let written = o
        .open(&tmp)
        .and_then(|mut f| f.write_all(&key).and_then(|()| f.sync_all()));
    let linked = written.and_then(|()| fs::hard_link(&tmp, key_path(home)));
    let _ = fs::remove_file(&tmp);
    match linked {
        Ok(()) => Ok(Some(key)),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(None),
        Err(e) => Err(io(e)),
    }
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
/// A missing key is made atomically (never over an existing one); only when this call made it are the rows
/// from before it, which cannot be verified, dropped.
pub fn add(
    home: &Path,
    root: &Path,
    scope: &str,
    fingerprint: &str,
    policy: &Policy,
) -> Result<()> {
    let rec = TrustRecord {
        root: root.to_string_lossy().into_owned(),
        scope: scope.into(),
        trusted_at: now(),
        fingerprint: fingerprint.into(),
    };
    put(home, &rec, policy)
}

/// Writes `rec` as it is (its own date): the undo of an answer that replaced a row.
pub fn restore(home: &Path, rec: &TrustRecord, policy: &Policy) -> Result<()> {
    put(home, rec, policy)
}

fn put(home: &Path, rec: &TrustRecord, policy: &Policy) -> Result<()> {
    let refusal = match rec.scope.as_str() {
        PARENT => policy.parent_refusal(Path::new(&rec.root)),
        FOLDER | CHILD => policy.root_refusal(Path::new(&rec.root)),
        _ => Some("an unknown scope"),
    };
    if let Some(why) = refusal {
        return Err(Error::Invalid(format!("trust refused for {why}")));
    }
    if rec.root.len() > MAX_ROOT_LEN {
        return Err(Error::Invalid("trust refused: the path is too long".into()));
    }
    let mut conn = connect(home, false)?;
    ensure(&conn)?;
    let key = match read_key(home) {
        Some(k) => k,
        None => match create_key(home)? {
            // this call made a new key: rows that do not verify under it (everything from before it) are
            // dropped, in one transaction, and rows another process already signed with it stay
            Some(k) => {
                let tx =
                    conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
                let rows: Vec<(TrustRecord, String)> = {
                    let mut st = tx.prepare(
                        "SELECT root, scope, trusted_at, fingerprint, mac FROM trusted_roots",
                    )?;
                    st.query_map([], |r| {
                        Ok((
                            TrustRecord {
                                root: r.get(0)?,
                                scope: r.get(1)?,
                                trusted_at: r.get(2)?,
                                fingerprint: r.get(3)?,
                            },
                            r.get::<_, String>(4)?,
                        ))
                    })?
                    .collect::<rusqlite::Result<_>>()?
                };
                for (rec, mac) in rows {
                    if !mac_ok(&k, &rec, &mac) {
                        // `AND mac=?`: a row rewritten meanwhile (and so signed) is not the one that failed
                        tx.execute(
                            "DELETE FROM trusted_roots WHERE root=?1 AND mac=?2",
                            [&rec.root, &mac],
                        )?;
                    }
                }
                tx.commit()?;
                k
            }
            None => read_key(home)
                .ok_or_else(|| Error::Invalid("the trust key is unreadable".into()))?,
        },
    };
    let n: i64 = conn.query_row("SELECT count(*) FROM trusted_roots", [], |r| r.get(0))?;
    let exists: bool = conn
        .query_row(
            "SELECT 1 FROM trusted_roots WHERE root=?1",
            [&rec.root],
            |_| Ok(()),
        )
        .optional()?
        .is_some();
    if !exists && n as usize >= MAX_ROWS {
        return Err(Error::Invalid(format!(
            "trust refused: {MAX_ROWS} folders are remembered already (revoke some)"
        )));
    }
    conn.execute(
        "INSERT INTO trusted_roots(root, scope, trusted_at, fingerprint, mac) VALUES (?1, ?2, ?3, ?4, ?5)
         ON CONFLICT(root) DO UPDATE SET scope=?2, trusted_at=?3, fingerprint=?4, mac=?5",
        (&rec.root, &rec.scope, &rec.trusted_at, &rec.fingerprint, mac_hex(&key, rec)),
    )?;
    Ok(())
}

/// Forgets `root` (the exact string of its record) and the baseline rows (`child`) of the projects under it.
/// Whether there was a row for `root`. Needs no key: forgetting is always allowed.
pub fn revoke(home: &Path, root: &str) -> Result<bool> {
    if !home.join("synt.db").exists() {
        return Ok(false);
    }
    let conn = connect(home, false)?;
    if !has_table(&conn)? {
        return Ok(false);
    }
    let n = conn.execute("DELETE FROM trusted_roots WHERE root=?1", [root])?;
    for sep in ["/", "\\"] {
        let prefix = format!("{root}{sep}");
        conn.execute(
            "DELETE FROM trusted_roots WHERE scope='child' AND substr(root,1,length(?1))=?1",
            [&prefix],
        )?;
    }
    Ok(n > 0)
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

/// Entries (files, directories, links) one manifest may visit: a web of links costs visits, not only hashes.
const MAX_VISITS: usize = 5000;

struct Walk {
    root: PathBuf,
    out: BTreeMap<String, String>,
    bytes: u64,
    visits: usize,
    /// Canonical directories already walked (a link back or a diamond is not walked twice).
    seen: std::collections::BTreeSet<PathBuf>,
}

fn hex(h: impl AsRef<[u8]>) -> String {
    h.as_ref().iter().map(|b| format!("{b:02x}")).collect()
}

impl Walk {
    /// One path of the manifest, under `name`. A link is followed to its real target, which must lie inside the
    /// project: the CONTENT of the target is what is hashed (a link to a directory is walked), so editing the
    /// target changes the manifest. A link that leaves the project is refused.
    fn visit(&mut self, name: &str, path: &Path, depth: u8) -> std::result::Result<(), Unfit> {
        if fs::symlink_metadata(path).is_err() {
            return Ok(());
        }
        self.visits += 1;
        if self.visits > MAX_VISITS {
            return Err(Unfit::TooBig);
        }
        // the real place, whether the entry or a folder on the way to it is a link: it must stay inside
        let real = canon(path);
        if !real.starts_with(&self.root) {
            return Err(Unfit::LinkOutside(name.to_string()));
        }
        let Ok(m) = fs::metadata(&real) else {
            // a link that dangles has no content to hash: refuse (a missing plain file never gets here)
            return if fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_symlink()) {
                Err(Unfit::LinkOutside(name.to_string()))
            } else {
                Ok(())
            };
        };
        if m.is_dir() {
            if depth >= MAX_DEPTH {
                return Err(Unfit::TooBig);
            }
            if !self.seen.insert(real.clone()) {
                return Ok(());
            }
            return self.dir(&real, name, depth + 1);
        }
        if !m.is_file() {
            return Ok(());
        }
        if self.out.len() >= MAX_FILES || m.len() > MAX_FILE || self.bytes + m.len() > MAX_TOTAL {
            return Err(Unfit::TooBig);
        }
        self.bytes += m.len();
        let mut h = Sha256::new();
        let mut f = open_regular(&real).ok_or(Unfit::TooBig)?;
        let mut buf = [0u8; 64 * 1024];
        let mut left = MAX_FILE + 1;
        loop {
            let n = f.read(&mut buf).map_err(|_| Unfit::TooBig)?;
            if n == 0 {
                break;
            }
            left = left.saturating_sub(n as u64);
            if left == 0 {
                return Err(Unfit::TooBig); // grew past the cap while reading
            }
            h.update(&buf[..n]);
        }
        self.out.insert(name.to_string(), hex(h.finalize()));
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
            self.visit(&name, &e.path(), depth)?;
        }
        Ok(())
    }
}

/// The instruction files each directory contributes (`synthesis-context/src/instructions.rs`
/// `DEFAULT_NAMES`), and the `@import` depth its loader follows.
const INSTRUCTION_NAMES: [&str; 5] = [
    "AGENTS.md",
    "CLAUDE.md",
    ".claude/CLAUDE.md",
    ".synt/AGENTS.md",
    "CLAUDE.local.md",
];
const IMPORT_DEPTH: u8 = 4;

/// Project configuration that is read for the person's settings, hooks, MCP servers and permission rules, per
/// directory that can hold it (the project root, and the session's folder when it is another).
const CONFIG_FILES: [&str; 4] = [
    ".mcp.json",
    ".synt/config.json",
    ".synt/settings.json",
    ".synt/settings.local.json",
];

/// Directories whose every file is loaded: sub-agents, skills, commands and rules.
const CONFIG_DIRS: [&str; 5] = [
    ".synt/agents",
    ".synt/skills",
    ".synt/commands",
    ".synt/rules",
    ".claude/rules",
];

/// The `@path` imports of an instruction file's text (the loader follows them from the file's folder).
fn imports_of(text: &str) -> Vec<String> {
    text.split_whitespace()
        .filter_map(|t| t.strip_prefix('@'))
        .map(|t| t.trim_end_matches(|c: char| ",.;:)".contains(c)))
        .filter(|t| !t.is_empty() && !t.starts_with('~') && !t.starts_with('/'))
        .map(String::from)
        .collect()
}

/// The words a hook or MCP server launches: every whitespace-separated word of the strings under `command`
/// keys and of the `args` arrays in the project's settings and `.mcp.json`, with `$SYNT_PROJECT_DIR`,
/// `${SYNT_PROJECT_DIR}`, `$CLAUDE_PROJECT_DIR` and `${CLAUDE_PROJECT_DIR}` (what the hooks crate sets) replaced
/// by the project root. Which of them name a file is decided by the caller. Too many words, or one that is
/// too long, is an error (a repository must not hide a script behind padding): no trust by fingerprint.
/// BEST EFFORT only: shell operators, `--opt=path`, directories and what a script runs in turn are not followed.
fn command_words(
    v: &serde_json::Value,
    root: &Path,
    out: &mut Vec<String>,
) -> std::result::Result<(), Unfit> {
    use serde_json::Value;
    fn take(s: &str, root: &Path, out: &mut Vec<String>) -> std::result::Result<(), Unfit> {
        let root = root.to_string_lossy();
        for tok in s.split_whitespace() {
            let tok = tok.trim_matches(|c| c == '"' || c == '\'');
            let tok = [
                "${SYNT_PROJECT_DIR}",
                "$SYNT_PROJECT_DIR",
                "${CLAUDE_PROJECT_DIR}",
                "$CLAUDE_PROJECT_DIR",
            ]
            .iter()
            .fold(tok.to_string(), |t, v| t.replace(v, &root));
            // the shell would strip quotes, wherever they sit
            let tok = tok.replace(['"', '\''], "");
            if tok.is_empty() || tok.starts_with('-') {
                continue;
            }
            if tok.len() > 4096 || out.len() >= MAX_WORDS {
                return Err(Unfit::TooBig);
            }
            out.push(tok);
        }
        Ok(())
    }
    match v {
        Value::Object(o) => {
            for (k, v) in o {
                match (k.as_str(), v) {
                    ("command", Value::String(s)) => take(s, root, out)?,
                    ("args", Value::Array(a)) => {
                        for s in a.iter().filter_map(Value::as_str) {
                            take(s, root, out)?;
                        }
                    }
                    _ => {}
                }
                command_words(v, root, out)?;
            }
        }
        Value::Array(a) => {
            for v in a {
                command_words(v, root, out)?;
            }
        }
        _ => {}
    }
    Ok(())
}

/// Words one manifest looks at.
const MAX_WORDS: usize = 2000;

/// The manifest (`name\thash\n` lines, sorted) of what a trusted project can switch on:
/// - settings, config and `.mcp.json` of the root and of `cwd` (`cwd/...` names) when it is another folder;
/// - the instruction files (`AGENTS.md`, `CLAUDE.md`, `.claude/CLAUDE.md`, `.synt/AGENTS.md`,
///   `CLAUDE.local.md`) of every folder from the root down to `cwd`, and what they `@import`;
/// - every file under `.synt/agents|skills|commands|rules` and `.claude/rules`;
/// - the in-project files that hooks and MCP servers name as their command or arguments (the code THOSE run
///   is not covered).
///
/// Hashing is streaming (depth 8, at most 500 files, 8 MiB each, 64 MiB in all). A link is followed when its
/// target is inside the project (the target's content is hashed); one that leaves it, or any cap exceeded, is an
/// error: no trust by fingerprint. Missing files are absent, so an untouched project gives the same manifest.
pub fn fingerprint(root: &Path, cwd: &Path) -> std::result::Result<String, Unfit> {
    let mut w = Walk {
        root: canon(root),
        out: BTreeMap::new(),
        bytes: 0,
        visits: 0,
        seen: Default::default(),
    };
    let cwd_real = canon(cwd);
    let mut bases: Vec<(String, PathBuf)> = vec![(String::new(), root.to_path_buf())];
    if cwd_real != w.root && cwd_real.starts_with(&w.root) {
        bases.push(("cwd/".into(), cwd.to_path_buf()));
    }
    for (prefix, base) in &bases {
        for rel in CONFIG_FILES {
            w.visit(&format!("{prefix}{rel}"), &base.join(rel), 0)?;
        }
    }
    for d in CONFIG_DIRS {
        w.visit(d, &root.join(d), 0)?;
    }
    // instruction files from the root down to the session's folder, and their imports
    let mut dirs = vec![root.to_path_buf()];
    if let Ok(rest) = cwd_real.strip_prefix(&w.root) {
        let mut cur = root.to_path_buf();
        for c in rest.components() {
            cur.push(c);
            dirs.push(cur.clone());
        }
    }
    let mut pending: Vec<(PathBuf, u8)> = vec![];
    for d in &dirs {
        for n in INSTRUCTION_NAMES {
            let p = d.join(n);
            let shown = p
                .strip_prefix(root)
                .unwrap_or(&p)
                .to_string_lossy()
                .replace('\\', "/");
            w.visit(&shown, &p, 0)?;
            if fs::metadata(&p).is_ok_and(|m| m.is_file()) {
                pending.push((p, 0));
            }
        }
    }
    let mut seen = std::collections::BTreeSet::new();
    while let Some((file, depth)) = pending.pop() {
        if depth >= IMPORT_DEPTH || !seen.insert(canon(&file)) {
            continue;
        }
        let mut text = String::new();
        if let Some(f) = open_regular(&file) {
            let _ = f.take(32 * 1024).read_to_string(&mut text);
        }
        for imp in imports_of(&text) {
            let target = file.parent().unwrap_or(root).join(&imp);
            let real = canon(&target);
            if !real.starts_with(&w.root) {
                continue; // the loader refuses an import that leaves the workspace
            }
            let shown = real
                .strip_prefix(&w.root)
                .unwrap_or(&real)
                .to_string_lossy()
                .replace('\\', "/");
            w.visit(&format!("@{shown}"), &target, 0)?;
            if fs::metadata(&real).is_ok_and(|m| m.is_file()) {
                pending.push((real, depth + 1));
            }
        }
    }
    // scripts that hooks and MCP servers launch: every word, tried against the root and the session's folder
    let mut words = vec![];
    for (_, base) in &bases {
        for rel in CONFIG_FILES {
            if let Some(f) = open_regular(&base.join(rel)) {
                let mut text = String::new();
                if f.take(MAX_FILE).read_to_string(&mut text).is_ok()
                    && let Ok(v) = serde_json::from_str(&text)
                {
                    command_words(&v, &w.root, &mut words)?;
                }
            }
        }
    }
    words.sort();
    words.dedup();
    let mut found = std::collections::BTreeSet::new();
    for tok in words {
        for base in [root, cwd] {
            let p = base.join(&tok);
            // a link that dangles or leaves the project cannot be trusted by content: refuse, do not skip
            if fs::symlink_metadata(&p).is_ok_and(|m| m.file_type().is_symlink()) {
                let real = canon(&p);
                if !real.starts_with(&w.root) || fs::metadata(&real).is_err() {
                    return Err(Unfit::LinkOutside(tok));
                }
            }
            let real = canon(&p);
            if real.starts_with(&w.root) && fs::metadata(&real).is_ok_and(|m| m.is_file()) {
                found.insert((real, p));
            }
        }
    }
    for (real, p) in found {
        let shown = real
            .strip_prefix(&w.root)
            .unwrap_or(&real)
            .to_string_lossy()
            .replace('\\', "/");
        w.visit(&format!("script:{shown}"), &p, 0)?;
    }
    Ok(w.out.iter().map(|(k, v)| format!("{k}\t{v}\n")).collect())
}

/// [`fingerprint`] of a project started in its own root.
pub fn fingerprint_root(root: &Path) -> std::result::Result<String, Unfit> {
    fingerprint(root, root)
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
        // real temp folders and the platform's own absolute spelling (no `/w/app` literals: not absolute on Windows)
        let d = tempfile::tempdir().unwrap();
        let base = synthesis_core::canonicalize(d.path()).unwrap();
        let pl = Policy {
            home: Some(base.join("nohome")),
            synt_home: base.join("nosynt"),
        };
        let at = |rel: &str| base.join(rel);
        let s = |rel: &str| at(rel).to_string_lossy().into_owned();
        let rs = [rec(&s("w/app"), FOLDER), rec(&s("p/all"), PARENT)];
        let c = |rel: &str| covers(&rs, &at(rel), &pl).is_some();
        assert!(c("w/app"));
        assert!(
            !c("w/app/sub"),
            "a folder grant is not inherited by a subfolder"
        );
        assert!(
            !c("w/app2") && !c("w") && !c("w/other"),
            "siblings and parents are not covered"
        );
        assert!(!c("W/APP"), "exact, case-sensitive");
        assert!(c("p/all") && c("p/all/x") && c("p/all/x/y"));
        assert!(
            !c("p/allx") && !c("p") && !c("p/other"),
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

    fn fp(r: &Path) -> String {
        fingerprint_root(r).unwrap()
    }

    #[test]
    fn the_fingerprint_follows_the_trusted_configuration_and_names_what_changed() {
        let d = tempfile::tempdir().unwrap();
        let r = &fs::canonicalize(d.path()).unwrap();
        let f0 = fp(r);
        assert_eq!(f0, fp(r));
        fs::write(r.join("README.md"), "x").unwrap();
        assert_eq!(fp(r), f0, "ordinary files do not count");
        fs::create_dir_all(r.join(".synt/agents")).unwrap();
        fs::write(r.join(".synt/settings.json"), "{}").unwrap();
        let f1 = fp(r);
        assert_eq!(changed_files(&f0, &f1), [".synt/settings.json"]);
        fs::write(r.join("AGENTS.md"), "rules").unwrap();
        let f2 = fp(r);
        assert_eq!(
            changed_files(&f1, &f2),
            ["AGENTS.md"],
            "instruction files count"
        );
        fs::write(r.join(".synt/agents/a.md"), "one").unwrap();
        fs::write(r.join(".mcp.json"), "{}").unwrap();
        let f3 = fp(r);
        fs::write(r.join(".synt/agents/a.md"), "two").unwrap();
        fs::remove_file(r.join(".mcp.json")).unwrap();
        assert_eq!(
            changed_files(&f3, &fp(r)),
            [".mcp.json", ".synt/agents/a.md"]
        );
        // deep files count (to a bounded depth)
        let deep = (0..6).fold(r.join(".synt/skills"), |p, i| p.join(format!("d{i}")));
        fs::create_dir_all(&deep).unwrap();
        let before = fp(r);
        fs::write(deep.join("s.md"), "x").unwrap();
        assert_ne!(fp(r), before);
        let too_deep = (0..10).fold(r.join(".synt/skills/z"), |p, i| p.join(format!("d{i}")));
        fs::create_dir_all(&too_deep).unwrap();
        assert_eq!(
            fingerprint_root(r),
            Err(Unfit::TooBig),
            "beyond the depth bound: no trust by fingerprint"
        );
    }

    #[test]
    fn every_kind_of_project_configuration_a_loader_reads_is_in_the_manifest() {
        // one file of each kind the loaders read (see CONFIG_FILES, CONFIG_DIRS, INSTRUCTION_NAMES); a new kind
        // must be added to those lists AND here, so a loader cannot read a file the manifest ignores unnoticed
        let kinds = [
            ".mcp.json",
            ".synt/config.json",
            ".synt/settings.json",
            ".synt/settings.local.json",
            ".synt/agents/a.md",
            ".synt/skills/s/SKILL.md",
            ".synt/commands/c.md",
            ".synt/rules/r.md",
            ".claude/rules/r.md",
            "AGENTS.md",
            "CLAUDE.md",
            ".claude/CLAUDE.md",
            ".synt/AGENTS.md",
            "CLAUDE.local.md",
        ];
        let d = tempfile::tempdir().unwrap();
        let r = &fs::canonicalize(d.path()).unwrap();
        let mut last = fp(r);
        for k in kinds {
            let p = r.join(k);
            fs::create_dir_all(p.parent().unwrap()).unwrap();
            fs::write(&p, "{}").unwrap();
            let now = fp(r);
            assert_eq!(
                changed_files(&last, &now),
                [k],
                "{k} is not in the manifest"
            );
            last = now;
            fs::write(&p, "{\"x\":1}").unwrap();
            assert_ne!(fp(r), last, "{k}: content must count");
            last = fp(r);
        }
    }

    #[test]
    fn nested_instruction_files_imports_and_the_session_folders_config_count() {
        let d = tempfile::tempdir().unwrap();
        let r = &fs::canonicalize(d.path()).unwrap();
        let cwd = r.join("pkg/app");
        fs::create_dir_all(&cwd).unwrap();
        fs::write(r.join("AGENTS.md"), "see @docs/guide.md").unwrap();
        fs::create_dir_all(r.join("docs")).unwrap();
        fs::write(r.join("docs/guide.md"), "v1").unwrap();
        let f = |c: &Path| fingerprint(r, c).unwrap();
        let base = f(&cwd);
        // an import is followed
        fs::write(r.join("docs/guide.md"), "v2").unwrap();
        assert_eq!(changed_files(&base, &f(&cwd)), ["@docs/guide.md"]);
        // a nested AGENTS.md between the root and the session's folder counts, one elsewhere does not
        let base = f(&cwd);
        fs::create_dir_all(r.join("pkg/other")).unwrap();
        fs::write(r.join("pkg/other/AGENTS.md"), "x").unwrap();
        assert_eq!(f(&cwd), base);
        fs::write(r.join("pkg/AGENTS.md"), "x").unwrap();
        assert_eq!(changed_files(&base, &f(&cwd)), ["pkg/AGENTS.md"]);
        // the session folder's own .synt/config.json (the loader reads cwd's, not the root's)
        let base = f(&cwd);
        fs::create_dir_all(cwd.join(".synt")).unwrap();
        fs::write(cwd.join(".synt/config.json"), "{}").unwrap();
        assert_eq!(changed_files(&base, &f(&cwd)), ["cwd/.synt/config.json"]);
        // an import that leaves the project is not followed (the loader refuses it too)
        let out = tempfile::tempdir().unwrap();
        fs::write(out.path().join("x.md"), "x").unwrap();
        fs::write(
            r.join("CLAUDE.md"),
            format!("@../../../..{}", out.path().display()),
        )
        .unwrap();
        let _ = f(&cwd);
    }

    #[test]
    fn scripts_named_by_hooks_and_mcp_servers_are_hashed_with_or_without_a_path() {
        let d = tempfile::tempdir().unwrap();
        let r = &fs::canonicalize(d.path()).unwrap();
        fs::create_dir_all(r.join("scripts")).unwrap();
        fs::create_dir_all(r.join(".synt")).unwrap();
        fs::create_dir_all(r.join("pkg")).unwrap();
        for (f, c) in [
            ("scripts/hook.sh", "echo 1"),
            ("srv.js", "1"),
            ("run.sh", "1"),
            ("unrelated.txt", "1"),
            ("pkg/tool.py", "1"),
        ] {
            fs::write(r.join(f), c).unwrap();
        }
        fs::write(
            r.join(".synt/settings.json"),
            r#"{"hooks":{"PreToolUse":[{"hooks":[{"type":"command","command":"./scripts/hook.sh --flag"},
               {"type":"command","command":"bash run.sh"}]}]}}"#,
        )
        .unwrap();
        fs::write(
            r.join(".mcp.json"),
            r#"{"mcpServers":{"s":{"command":"node","args":["srv.js","--port","80"]},
                "t":{"command":"python3","args":["tool.py"]}}}"#,
        )
        .unwrap();
        let cwd = r.join("pkg");
        let f = || fingerprint(r, &cwd).unwrap();
        for (file, shown) in [
            ("scripts/hook.sh", "script:scripts/hook.sh"),
            ("srv.js", "script:srv.js"),
            ("run.sh", "script:run.sh"),
            ("pkg/tool.py", "script:pkg/tool.py"), // a bare word that names a file under the session's folder
        ] {
            let base = f();
            fs::write(r.join(file), "changed").unwrap();
            assert_eq!(changed_files(&base, &f()), [shown], "{file}");
        }
        // words that name no file (node, bash, --flag, 80) are skipped, and so is a file nobody names
        let base = f();
        fs::write(r.join("unrelated.txt"), "changed").unwrap();
        assert_eq!(f(), base);
    }

    #[cfg(unix)]
    #[test]
    fn a_web_of_links_is_bounded_by_visits_and_a_config_folder_link_out_is_refused() {
        let d = tempfile::tempdir().unwrap();
        let r = &fs::canonicalize(d.path()).unwrap();
        // a diamond of directory links: every level links twice to the next, no file is ever hashed
        let skills = r.join(".synt/skills");
        fs::create_dir_all(&skills).unwrap();
        for lvl in 0..7 {
            let dir = r.join(format!("lvl{lvl}"));
            fs::create_dir_all(&dir).unwrap();
            for k in 0..30 {
                let target = r.join(format!("lvl{}", lvl + 1));
                fs::create_dir_all(&target).unwrap();
                std::os::unix::fs::symlink(&target, dir.join(format!("l{k}"))).unwrap();
            }
        }
        std::os::unix::fs::symlink(r.join("lvl0"), skills.join("web")).unwrap();
        let t0 = std::time::Instant::now();
        let got = fingerprint_root(r);
        assert!(
            t0.elapsed() < std::time::Duration::from_secs(5),
            "{:?}",
            t0.elapsed()
        );
        assert!(got.is_ok() || got == Err(Unfit::TooBig));
        // `.synt` itself a link to a folder outside the project: refused
        let d2 = tempfile::tempdir().unwrap();
        let r2 = &fs::canonicalize(d2.path()).unwrap().join("proj");
        let out = d2.path().join("outside");
        fs::create_dir_all(out.join("agents")).unwrap();
        fs::write(out.join("agents/a.md"), "x").unwrap();
        fs::create_dir_all(r2).unwrap();
        std::os::unix::fs::symlink(&out, r2.join(".synt")).unwrap();
        assert!(matches!(fingerprint_root(r2), Err(Unfit::LinkOutside(_))));
    }

    #[cfg(unix)]
    #[test]
    fn a_fifo_in_the_configuration_cannot_hang_the_walk() {
        let d = tempfile::tempdir().unwrap();
        let r = &fs::canonicalize(d.path()).unwrap();
        fs::create_dir_all(r.join(".synt")).unwrap();
        let fifo = r.join(".synt/settings.json");
        // the mkfifo binary the platform ships (no unsafe code here)
        let st = std::process::Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .unwrap();
        assert!(st.success());
        let t0 = std::time::Instant::now();
        let _ = fingerprint_root(r); // an error or a manifest, but it returns
        assert!(t0.elapsed() < std::time::Duration::from_secs(5));
    }

    #[test]
    fn a_new_key_drops_only_rows_that_do_not_verify_under_it() {
        let (_d, home) = store_home();
        let pl = pol(&home);
        let (a, b) = (project(&home, "app"), project(&home, "other"));
        add(&home, &a, FOLDER, "fp", &pl).unwrap();
        // the key is lost; another writer made the new key and signed a row with it before our add ran
        fs::remove_file(key_path(&home)).unwrap();
        let k = create_key(&home).unwrap().unwrap();
        let theirs = TrustRecord {
            root: b.to_string_lossy().into_owned(),
            scope: FOLDER.into(),
            trusted_at: now(),
            fingerprint: "mine".into(),
        };
        let conn = connect(&home, false).unwrap();
        conn.execute(
            "INSERT INTO trusted_roots(root, scope, trusted_at, fingerprint, mac) VALUES (?1,?2,?3,?4,?5)",
            (&theirs.root, &theirs.scope, &theirs.trusted_at, &theirs.fingerprint, mac_hex(&k, &theirs)),
        )
        .unwrap();
        // a later add finds a key in force: nothing is dropped, and the old row still does not verify
        let c = project(&home, "third");
        add(&home, &c, FOLDER, "fp", &pl).unwrap();
        let l = list(&home).unwrap();
        let roots: Vec<_> = l.records.iter().map(|r| r.root.clone()).collect();
        assert!(
            roots.contains(&theirs.root) && roots.contains(&c.to_string_lossy().into_owned()),
            "{roots:?}"
        );
        assert_eq!(
            l.unverified, 1,
            "the row from before the new key is ignored"
        );
    }

    #[test]
    fn a_big_file_is_refused_not_truncated() {
        let d = tempfile::tempdir().unwrap();
        fs::create_dir_all(d.path().join(".synt")).unwrap();
        let f = fs::File::create(d.path().join(".synt/settings.json")).unwrap();
        f.set_len(MAX_FILE + 1).unwrap();
        assert_eq!(fingerprint_root(d.path()), Err(Unfit::TooBig));
    }

    #[cfg(unix)]
    #[test]
    fn an_in_project_link_is_hashed_by_its_content_and_one_that_leaves_the_project_is_refused() {
        let d = tempfile::tempdir().unwrap();
        let r = &fs::canonicalize(d.path()).unwrap();
        fs::create_dir_all(r.join(".synt")).unwrap();
        fs::write(r.join("rules.md"), "a").unwrap();
        std::os::unix::fs::symlink(r.join("rules.md"), r.join("AGENTS.md")).unwrap();
        let f1 = fp(r);
        // editing the target changes what the link provides: the manifest must change
        fs::write(r.join("rules.md"), "b").unwrap();
        assert_eq!(changed_files(&f1, &fp(r)), ["AGENTS.md"]);
        // a linked directory under skills is walked, not hashed as a link
        fs::create_dir_all(r.join("shared/skill")).unwrap();
        fs::write(r.join("shared/skill/SKILL.md"), "1").unwrap();
        std::os::unix::fs::symlink(r.join("shared"), r.join(".synt/skills")).unwrap();
        let f2 = fp(r);
        fs::write(r.join("shared/skill/SKILL.md"), "2").unwrap();
        assert_eq!(changed_files(&f2, &fp(r)), [".synt/skills/skill/SKILL.md"]);
        // a link that leaves the project
        fs::remove_file(r.join("AGENTS.md")).unwrap();
        let out = tempfile::tempdir().unwrap();
        fs::write(out.path().join("x.md"), "x").unwrap();
        std::os::unix::fs::symlink(out.path().join("x.md"), r.join("AGENTS.md")).unwrap();
        assert!(matches!(fingerprint_root(r), Err(Unfit::LinkOutside(_))));
    }

    #[test]
    fn an_unknown_home_fails_closed() {
        let d = tempfile::tempdir().unwrap();
        let base = fs::canonicalize(d.path()).unwrap();
        let pl = Policy {
            home: None,
            synt_home: base.join("synthome"),
        };
        assert!(
            pl.root_refusal(&base.join("proj"))
                .unwrap()
                .contains("home folder is unknown")
        );
        assert!(pl.parent_refusal(&base.join("proj")).is_some());
        let rs = [rec(&base.join("proj").to_string_lossy(), FOLDER)];
        assert!(covers(&rs, &base.join("proj"), &pl).is_none());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn container_names_compare_case_insensitively_on_macos() {
        let (d, home) = store_home();
        let uh = d.path().join("userhome");
        fs::create_dir_all(uh.join("Downloads")).unwrap();
        let pl = pol(&home);
        assert!(pl.parent_refusal(&uh.join("downloads")).is_some());
        assert!(same(Path::new("/Tmp/X"), Path::new("/tmp/x")));
    }

    #[cfg(unix)]
    #[test]
    fn a_world_writable_ancestor_without_the_sticky_bit_refuses_a_parent_grant() {
        use std::os::unix::fs::PermissionsExt as _;
        let (d, home) = store_home();
        let pl = pol(&home);
        let open = d.path().join("open");
        let inner = open.join("proj");
        fs::create_dir_all(&inner).unwrap();
        fs::set_permissions(&open, fs::Permissions::from_mode(0o777)).unwrap();
        assert!(
            pl.parent_refusal(&inner)
                .unwrap()
                .contains("world-writable")
        );
        fs::set_permissions(&open, fs::Permissions::from_mode(0o1777)).unwrap();
        // with the sticky bit the ancestor cannot be used to swap the folder (the folder itself is closed)
        assert!(
            pl.parent_refusal(&inner).is_none()
                || pl.parent_refusal(&inner).unwrap().contains("temporary")
        );
    }

    #[test]
    fn a_failed_key_creation_drops_no_rows_and_revoke_removes_the_baselines_below() {
        let (_d, home) = store_home();
        let pl = pol(&home);
        let (a, b) = (project(&home, "app"), project(&home, "other"));
        add(&home, &a, FOLDER, "fp", &pl).unwrap();
        add(&home, &b, FOLDER, "fp", &pl).unwrap();
        // the key is gone and cannot be made (run is a file): the rows are untouched, the add fails
        fs::remove_dir_all(home.join("run")).unwrap();
        fs::write(home.join("run"), b"in the way").unwrap();
        assert!(add(&home, &a, FOLDER, "fp", &pl).is_err());
        let n: i64 = connect(&home, true)
            .unwrap()
            .query_row("SELECT count(*) FROM trusted_roots", [], |r| r.get(0))
            .unwrap();
        assert_eq!(
            n, 2,
            "rows are dropped only when a new key was actually made"
        );
        fs::remove_file(home.join("run")).unwrap();
        // baselines under a parent go with it
        let parent = project(&home, "group");
        let kid = parent.join("kid");
        fs::create_dir_all(&kid).unwrap();
        add(&home, &parent, PARENT, "", &pl).unwrap();
        add(&home, &kid, CHILD, "m", &pl).unwrap();
        assert!(revoke(&home, &parent.to_string_lossy()).unwrap());
        let l = list(&home).unwrap();
        assert!(
            l.records
                .iter()
                .all(|r| r.scope != CHILD && r.root != parent.to_string_lossy()),
            "{l:?}"
        );
    }

    #[test]
    fn project_dir_variables_are_expanded_padding_fails_closed_and_a_bad_link_is_refused() {
        let d = tempfile::tempdir().unwrap();
        let r = &fs::canonicalize(d.path()).unwrap();
        fs::create_dir_all(r.join("hooks")).unwrap();
        fs::create_dir_all(r.join(".synt")).unwrap();
        fs::write(r.join("hooks/x.sh"), "1").unwrap();
        fs::write(r.join("hooks/y.sh"), "1").unwrap();
        let write = |c: &str| fs::write(r.join(".synt/settings.json"), c).unwrap();
        write(
            r#"{"hooks":{"E":[{"hooks":[{"command":"$CLAUDE_PROJECT_DIR/hooks/x.sh"},{"command":"\"${SYNT_PROJECT_DIR}\"/hooks/y.sh"}]}]}}"#,
        );
        for (file, shown) in [
            ("hooks/x.sh", "script:hooks/x.sh"),
            ("hooks/y.sh", "script:hooks/y.sh"),
        ] {
            let base = fp(r);
            fs::write(r.join(file), "changed").unwrap();
            assert_eq!(changed_files(&base, &fp(r)), [shown], "{file}");
        }
        // 2000+ words of padding before the real script: refused, not truncated
        let pad = (0..MAX_WORDS + 5)
            .map(|i| format!("w{i}"))
            .collect::<Vec<_>>()
            .join(" ");
        write(&format!(
            r#"{{"hooks":{{"E":[{{"hooks":[{{"command":"{pad} ./hooks/x.sh"}}]}}]}}}}"#
        ));
        assert_eq!(fingerprint_root(r), Err(Unfit::TooBig));
        // one very long word as well
        write(&format!(
            r#"{{"hooks":{{"E":[{{"hooks":[{{"command":"{}"}}]}}]}}}}"#,
            "a".repeat(5000)
        ));
        assert_eq!(fingerprint_root(r), Err(Unfit::TooBig));
    }

    #[cfg(unix)]
    #[test]
    fn a_dangling_or_outside_link_named_as_a_script_is_refused_not_skipped() {
        let d = tempfile::tempdir().unwrap();
        let r = &fs::canonicalize(d.path()).unwrap();
        fs::create_dir_all(r.join(".synt")).unwrap();
        let out = tempfile::tempdir().unwrap();
        fs::write(out.path().join("evil.sh"), "x").unwrap();
        let hook = |name: &str| {
            fs::write(
                r.join(".synt/settings.json"),
                format!(r#"{{"hooks":{{"E":[{{"hooks":[{{"command":"./{name}"}}]}}]}}}}"#),
            )
            .unwrap();
        };
        std::os::unix::fs::symlink(out.path().join("evil.sh"), r.join("out.sh")).unwrap();
        hook("out.sh");
        assert!(
            matches!(fingerprint_root(r), Err(Unfit::LinkOutside(_))),
            "a link out"
        );
        std::os::unix::fs::symlink(r.join("nowhere.sh"), r.join("dangling.sh")).unwrap();
        hook("dangling.sh");
        assert!(
            matches!(fingerprint_root(r), Err(Unfit::LinkOutside(_))),
            "a dangling link"
        );
        // a dangling link inside a config folder is refused too
        fs::write(r.join(".synt/settings.json"), "{}").unwrap();
        fs::create_dir_all(r.join(".synt/agents")).unwrap();
        std::os::unix::fs::symlink(r.join("nowhere.md"), r.join(".synt/agents/a.md")).unwrap();
        assert!(matches!(fingerprint_root(r), Err(Unfit::LinkOutside(_))));
    }

    #[test]
    fn restore_writes_a_row_back_with_its_own_date() {
        let (_d, home) = store_home();
        let pl = pol(&home);
        let a = project(&home, "app");
        add(&home, &a, FOLDER, "old", &pl).unwrap();
        let before = list(&home).unwrap().records[0].clone();
        add(&home, &a, FOLDER, "new", &pl).unwrap();
        restore(&home, &before, &pl).unwrap();
        assert_eq!(list(&home).unwrap().records, vec![before]);
    }
}
