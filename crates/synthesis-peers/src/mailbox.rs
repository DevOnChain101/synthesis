//! Durable mailbox for sessions that are offline (messaging.md R10c). A request to a dead session
//! and a reply to a dead asker are stored as one file each under `run/mailbox/<session_id>/`, signed
//! with the shared key (HMAC, like the live handshake), and delivered when that session comes back.
//!
//! Guarantees: written temp + fsync + rename (a crash leaves a whole file or none); an id is
//! stored once per kind (file or `done` ledger) so a retry never duplicates; bounded per session,
//! in total, in directories and by age; a file whose signature, id or size is wrong is deleted
//! unread, but one that merely cannot be read right now is kept for the next scan.

use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::serve::MAX_REPLY;
use crate::{Addr, Entry, Key, net, write_private};

/// Queued envelopes per target session.
pub const MAX_QUEUED: usize = 32;
/// Queued envelopes over all sessions.
pub const MAX_TOTAL: usize = 256;
/// Session directories under the mailbox root.
pub const MAX_DIRS: usize = 1024;
/// An undelivered envelope is dropped after this long.
pub const TTL_S: u64 = 7 * 24 * 3600;
/// An offline session stays addressable for this long after its last start.
pub const CARD_TTL_S: u64 = 30 * 24 * 3600;
/// Clock difference tolerated between two sessions of one machine.
pub const SKEW_S: u64 = 300;
const LEDGER_MAX: usize = 8192;
const MAX_FILE: u64 = 128 * 1024;
const MAX_CHAIN: usize = 8;
/// MAC domains: an envelope's tag is never a valid card tag or handshake proof.
const ENVELOPE_LABEL: &[u8] = b"synthesis-peer-v1-envelope";
const CARD_LABEL: &[u8] = b"synthesis-peer-v1-card";

pub fn now_s() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// One queued message. `kind` is `request` (to the target's main agent) or `reply`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Envelope {
    pub id: String,
    pub kind: String,
    pub from: Addr,
    pub to_session: String,
    pub to_agent: String,
    pub text: String,
    #[serde(default)]
    pub chain: Vec<String>,
    /// Reply status (`answered|declined|failed`); empty for requests.
    #[serde(default)]
    pub status: String,
    pub created: u64,
    #[serde(default)]
    pub mac: String,
}

impl Envelope {
    fn body(&self) -> Vec<u8> {
        let mut c = self.clone();
        c.mac.clear();
        serde_json::to_vec(&c).unwrap_or_default()
    }

    pub fn sign(&mut self, key: &Key) {
        self.mac = net::proof(key, ENVELOPE_LABEL, &self.body(), b"");
    }

    pub fn verified(&self, key: &Key) -> bool {
        net::verify(key, ENVELOPE_LABEL, &self.body(), b"", Some(&self.mac))
    }

    /// Field sizes and the clock window a stored or pushed envelope must stay within.
    fn bounded(&self) -> bool {
        let now = now_s();
        matches!(self.kind.as_str(), "request" | "reply")
            && self.from == self.from.clone().clamped()
            && self.to_agent.chars().count() <= 256
            && self.status.chars().count() <= 256
            && self.chain.len() <= MAX_CHAIN
            && self.chain.iter().all(|c| c.chars().count() <= 64)
            && self.created <= now + SKEW_S
            && now.saturating_sub(self.created) <= TTL_S
    }

    fn file_name(&self) -> String {
        format!("{}.{}.json", self.kind, self.id)
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum Queued {
    Stored,
    /// Already stored or already delivered: nothing was written.
    Duplicate,
}

#[derive(Debug, PartialEq, Eq)]
pub enum MailError {
    Invalid,
    TooLarge,
    /// Per-session, directory or total bound reached.
    Full,
    Io,
}

/// Ids become file names: letters, digits and `-` only.
pub fn valid_id(s: &str) -> bool {
    !s.is_empty() && s.len() <= 64 && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
}

pub struct Mailbox {
    root: PathBuf,
    key: Key,
}

/// Rows of a ledger: `key \t added_at \t extra...`. A missing file is empty; a link, a directory
/// or any read error is an error (callers fail closed).
fn ledger(p: &Path) -> io::Result<Vec<Vec<String>>> {
    match std::fs::symlink_metadata(p) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(vec![]),
        Err(e) => return Err(e),
        Ok(m) if !m.is_file() => return Err(io::Error::other("ledger is not a regular file")),
        Ok(_) => {}
    }
    Ok(std::fs::read_to_string(p)?
        .lines()
        .map(|l| l.split('\t').map(String::from).collect())
        .collect())
}

fn clean(s: &str) -> String {
    s.replace(['\t', '\n', '\r'], " ")
}

/// Appends a row. At the cap, rows too old to matter (their envelope is past its TTL, so it can
/// no longer be accepted) are dropped by an atomic rewrite; if none are, the add fails rather than
/// forgetting an id that could be replayed or is still awaited.
// shortcut: O_NOFOLLOW is not used (no libc dependency); the directory is 0700 and ours, and a
// link is refused by the lstat check; upgrade when a second user can write there.
fn ledger_add(p: &Path, fields: &[&str]) -> io::Result<()> {
    let mut rows = ledger(p)?;
    let now = now_s();
    let mut row = vec![clean(fields[0]), now.to_string()];
    row.extend(fields[1..].iter().map(|f| clean(f)));
    if rows.len() >= LEDGER_MAX {
        rows.retain(|r| {
            r.get(1)
                .and_then(|t| t.parse::<u64>().ok())
                .is_some_and(|t| t + TTL_S + 2 * SKEW_S >= now)
        });
        if rows.len() >= LEDGER_MAX {
            return Err(io::Error::other("ledger is full"));
        }
        rows.push(row);
        let text: Vec<String> = rows.iter().map(|r| r.join("\t")).collect();
        return write_private(p, (text.join("\n") + "\n").as_bytes());
    }
    let mut o = std::fs::OpenOptions::new();
    o.append(true).create(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut o, 0o600);
    let mut f = o.open(p)?;
    f.write_all(format!("{}\n", row.join("\t")).as_bytes())?;
    f.sync_all()
}

fn private_dir(p: &Path, parent: &Path) -> io::Result<()> {
    match std::fs::symlink_metadata(p) {
        Ok(m) if m.is_dir() => {}
        Ok(_) => return Err(io::Error::other("mailbox path is not a directory")),
        Err(_) => {
            #[cfg(unix)]
            {
                let mut b = std::fs::DirBuilder::new();
                std::os::unix::fs::DirBuilderExt::mode(&mut b, 0o700);
                b.create(p)?;
            }
            #[cfg(not(unix))]
            std::fs::DirBuilder::new().create(p)?;
        }
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let m = std::fs::metadata(p)?;
        if m.permissions().mode() & 0o077 != 0 || m.uid() != std::fs::metadata(parent)?.uid() {
            return Err(io::Error::other("mailbox directory is not private to us"));
        }
    }
    let _ = parent;
    Ok(())
}

/// A card: the registry entry of an offline session, signed.
#[derive(Serialize, Deserialize)]
struct Card {
    entry: Entry,
    mac: String,
}

impl Mailbox {
    pub fn new(run_dir: &Path, key: &Key) -> Self {
        Self {
            root: run_dir.join("mailbox"),
            key: key.clone(),
        }
    }

    fn root(&self) -> Result<(), MailError> {
        let run = self.root.parent().unwrap_or(&self.root);
        private_dir(&self.root, run).map_err(|_| MailError::Io)
    }

    fn dir(&self, sid: &str) -> Result<PathBuf, MailError> {
        if !valid_id(sid) {
            return Err(MailError::Invalid);
        }
        let d = self.root.join(sid);
        self.root()?;
        private_dir(&d, &self.root).map_err(|_| MailError::Io)?;
        Ok(d)
    }

    /// The directory when it already exists and is ours; reading never creates anything.
    fn existing(&self, sid: &str) -> Option<PathBuf> {
        let d = self.root.join(sid);
        // `private_dir` would create a missing directory; only look at one that is there.
        (valid_id(sid)
            && d.symlink_metadata().is_ok()
            && private_dir(&d, &self.root).is_ok()
            && d.is_dir())
        .then_some(d)
    }

    fn files(dir: &Path) -> Vec<PathBuf> {
        std::fs::read_dir(dir)
            .into_iter()
            .flatten()
            .filter_map(Result::ok)
            .map(|f| f.path())
            .filter(|p| p.extension().is_some_and(|x| x == "json"))
            .collect()
    }

    fn total(&self) -> usize {
        std::fs::read_dir(&self.root)
            .into_iter()
            .flatten()
            .filter_map(Result::ok)
            .map(|d| Self::files(&d.path()).len())
            .sum()
    }

    fn dirs(&self) -> usize {
        std::fs::read_dir(&self.root).map_or(0, |r| r.filter_map(Result::ok).count())
    }

    /// Stores `env` for `env.to_session`, signed. Idempotent on `(kind, id)`.
    pub fn enqueue(&self, mut env: Envelope) -> Result<Queued, MailError> {
        if !valid_id(&env.id) || !valid_id(&env.to_session) || !env.bounded() {
            return Err(MailError::Invalid);
        }
        if env.text.len() > MAX_REPLY {
            return Err(MailError::TooLarge);
        }
        env.sign(&self.key);
        let bytes = serde_json::to_vec(&env).map_err(|_| MailError::Io)?;
        if bytes.len() as u64 >= MAX_FILE {
            return Err(MailError::TooLarge);
        }
        // Bounds come before anything is created: a flood of new session ids makes no directories.
        self.root()?;
        let existing = self.existing(&env.to_session);
        if let Some(d) = &existing {
            let key = format!("{}:{}", env.kind, env.id);
            let seen = ledger(&d.join("done")).map_err(|_| MailError::Io)?;
            if d.join(env.file_name()).exists() || seen.iter().any(|r| r[0] == key) {
                return Ok(Queued::Duplicate);
            }
        }
        let room = existing.is_some() || self.dirs() < MAX_DIRS;
        if !room
            || self.total() >= MAX_TOTAL
            || (existing.is_some() && self.pending(&env.to_session).len() >= MAX_QUEUED)
        {
            return Err(MailError::Full);
        }
        let dir = self.dir(&env.to_session)?;
        write_private(&dir.join(env.file_name()), &bytes).map_err(|_| MailError::Io)?;
        Ok(Queued::Stored)
    }

    /// Valid envelopes for `sid`, oldest first. Forged, oversized, expired, misnamed and already
    /// delivered files are deleted; a file that cannot be read right now is kept (retry later);
    /// leftover temp files of a crashed write are swept.
    pub fn pending(&self, sid: &str) -> Vec<Envelope> {
        let Some(dir) = self.existing(sid) else {
            return vec![];
        };
        // Cannot dedupe without the ledger: show nothing rather than something twice.
        let Ok(done) = ledger(&dir.join("done")) else {
            return vec![];
        };
        let mut out = vec![];
        for p in Self::files(&dir) {
            let Ok(meta) = std::fs::symlink_metadata(&p) else {
                continue;
            };
            let env = if !meta.is_file() || meta.len() > MAX_FILE {
                None
            } else {
                let Ok(b) = std::fs::read(&p) else { continue };
                serde_json::from_slice::<Envelope>(&b).ok().filter(|e| {
                    e.verified(&self.key)
                        && e.bounded()
                        && e.to_session == sid
                        && p.file_name()
                            .is_some_and(|n| *n.to_string_lossy() == e.file_name())
                        && !done.iter().any(|r| r[0] == format!("{}:{}", e.kind, e.id))
                })
            };
            match env {
                Some(e) => out.push(e),
                None => {
                    let _ = std::fs::remove_file(&p);
                }
            }
        }
        for f in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
            let p = f.path();
            let stale = p
                .extension()
                .is_some_and(|x| x.to_string_lossy().starts_with("tmp"))
                && f.metadata()
                    .and_then(|m| m.modified())
                    .ok()
                    .and_then(|t| t.elapsed().ok())
                    .is_some_and(|a| a > Duration::from_secs(60));
            if stale {
                let _ = std::fs::remove_file(p);
            }
        }
        out.sort_by(|a, b| (a.created, &a.id).cmp(&(b.created, &b.id)));
        out
    }

    /// Marks `(kind, id)` delivered (ledger first, so a crash before the delete cannot redeliver)
    /// and removes the file. If the ledger cannot be written the file stays.
    pub fn done(&self, sid: &str, kind: &str, id: &str) {
        if let Ok(d) = self.dir(sid)
            && valid_id(id)
            && matches!(kind, "request" | "reply")
            && ledger_add(&d.join("done"), &[&format!("{kind}:{id}")]).is_ok()
        {
            let _ = std::fs::remove_file(d.join(format!("{kind}.{id}.json")));
        }
    }

    /// Removes a file without recording it as delivered (a message we refuse to show must not use
    /// up the id of the real one).
    pub fn discard(&self, sid: &str, kind: &str, id: &str) {
        if let Some(d) = self.existing(sid)
            && valid_id(id)
            && matches!(kind, "request" | "reply")
        {
            let _ = std::fs::remove_file(d.join(format!("{kind}.{id}.json")));
        }
    }

    pub fn delivered(&self, sid: &str, kind: &str, id: &str) -> bool {
        self.existing(sid).is_some_and(|d| {
            ledger(&d.join("done"))
                .is_ok_and(|rows| rows.iter().any(|r| r[0] == format!("{kind}:{id}")))
        })
    }

    /// The asker records the id of a request it queued and who it went to; only a reply with that
    /// id from that session and workspace is accepted later.
    pub fn expect(&self, sid: &str, id: &str, target: &Entry) -> Result<(), MailError> {
        let d = self.dir(sid)?;
        if !valid_id(id) {
            return Err(MailError::Invalid);
        }
        ledger_add(
            &d.join("expect"),
            &[id, &target.session_id, &target.workspace_key],
        )
        .map_err(|_| MailError::Io)
    }

    pub fn expected(&self, sid: &str, id: &str, from: &Addr) -> bool {
        self.existing(sid).is_some_and(|d| {
            ledger(&d.join("expect")).is_ok_and(|rows| {
                rows.iter().any(|r| {
                    r[0] == id
                        && r.get(2).is_some_and(|s| *s == from.session_id)
                        && r.get(3).is_some_and(|k| *k == clean(&from.workspace_key))
                })
            })
        })
    }

    /// Makes an offline session addressable (kept after the process exits). Signed.
    pub fn write_card(&self, e: &Entry) -> io::Result<()> {
        let d = self
            .dir(&e.session_id)
            .map_err(|_| io::Error::other("bad session id"))?;
        let entry = Entry {
            endpoint: String::new(),
            ..e.clone()
        };
        let mac = net::proof(
            &self.key,
            CARD_LABEL,
            &serde_json::to_vec(&entry).map_err(io::Error::other)?,
            b"",
        );
        write_private(
            &d.join("card.entry"),
            &serde_json::to_vec(&Card { entry, mac }).map_err(io::Error::other)?,
        )
    }

    /// Cards of sessions that are not in `live` and were started within [`CARD_TTL_S`]. A card
    /// that is a link, oversized, unsigned, or names another session than its directory is ignored.
    pub fn offline_cards(&self, live: &[Entry]) -> Vec<Entry> {
        let mut out = vec![];
        for d in std::fs::read_dir(&self.root)
            .into_iter()
            .flatten()
            .flatten()
        {
            let (p, name) = (d.path().join("card.entry"), d.file_name());
            let Some(meta) = std::fs::symlink_metadata(&p)
                .ok()
                .filter(|m| m.is_file() && m.len() <= MAX_FILE)
            else {
                continue;
            };
            let fresh = meta
                .modified()
                .ok()
                .and_then(|t| t.elapsed().ok())
                .is_some_and(|a| a.as_secs() <= CARD_TTL_S);
            let card = std::fs::read(&p)
                .ok()
                .and_then(|b| serde_json::from_slice::<Card>(&b).ok())
                .filter(|c| {
                    let e = &c.entry;
                    e.v == 1
                        && e.endpoint.is_empty()
                        && *name == *e.session_id
                        && valid_id(&e.session_id)
                        && net::verify(
                            &self.key,
                            CARD_LABEL,
                            &serde_json::to_vec(e).unwrap_or_default(),
                            b"",
                            Some(&c.mac),
                        )
                })
                .map(|c| c.entry);
            match card {
                Some(c) if fresh && !live.iter().any(|l| l.session_id == c.session_id) => {
                    out.push(c);
                }
                Some(_) if !fresh => {
                    let _ = std::fs::remove_file(&p);
                }
                _ => {}
            }
        }
        out.sort_by(|a, b| a.session_id.cmp(&b.session_id));
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mb() -> (tempfile::TempDir, Mailbox) {
        let t = tempfile::tempdir().unwrap();
        let run = crate::run_dir(t.path());
        let key = crate::load_or_create_key(&run).unwrap();
        let m = Mailbox::new(&run, &key);
        (t, m)
    }

    fn env(id: &str, to: &str) -> Envelope {
        Envelope {
            id: id.into(),
            kind: "request".into(),
            from: Addr {
                session_id: "asker".into(),
                instance: "main".into(),
                ..Default::default()
            },
            to_session: to.into(),
            to_agent: "main".into(),
            text: "hello".into(),
            chain: vec![],
            status: String::new(),
            created: now_s(),
            mac: String::new(),
        }
    }

    fn entry(sid: &str) -> Entry {
        Entry {
            v: 1,
            session_id: sid.into(),
            session_name: Some("backend".into()),
            workspace_key: "k".into(),
            workspace_label: "api".into(),
            workspace_root: "/r".into(),
            cwd: "/r".into(),
            pid: 1,
            pid_start: None,
            repo_key: String::new(),
            about: None,
            endpoint: "ep".into(),
            started_at: 0,
        }
    }

    fn reply_from(sid: &str, ws: &str) -> Addr {
        Addr {
            session_id: sid.into(),
            workspace_key: ws.into(),
            ..Default::default()
        }
    }

    #[test]
    fn stores_once_survives_restart_and_never_redelivers() {
        let (t, m) = mb();
        assert_eq!(m.enqueue(env("r1", "s1")), Ok(Queued::Stored));
        assert_eq!(m.enqueue(env("r1", "s1")), Ok(Queued::Duplicate));
        // a restart is a new Mailbox over the same files
        let again = Mailbox::new(&crate::run_dir(t.path()), &m.key);
        let p = again.pending("s1");
        assert_eq!(p.len(), 1);
        assert!(p[0].verified(&m.key));
        again.done("s1", "request", "r1");
        assert!(
            again.pending("s1").is_empty() && again.delivered("s1", "request", "r1"),
            "retired"
        );
        // the sender retrying after delivery writes nothing
        assert_eq!(again.enqueue(env("r1", "s1")), Ok(Queued::Duplicate));
        assert!(again.pending("s1").is_empty());
    }

    #[test]
    fn request_and_reply_ids_are_separate_namespaces() {
        let (_t, m) = mb();
        m.enqueue(env("x1", "s1")).unwrap();
        m.done("s1", "request", "x1");
        let mut r = env("x1", "s1");
        r.kind = "reply".into();
        assert_eq!(m.enqueue(r), Ok(Queued::Stored), "a delivered request id");
        assert!(!m.delivered("s1", "reply", "x1"));
    }

    #[test]
    fn forged_misnamed_expired_and_bad_ids_are_refused() {
        let (_t, m) = mb();
        assert_eq!(m.enqueue(env("../x", "s1")), Err(MailError::Invalid));
        assert_eq!(m.enqueue(env("a", "../s")), Err(MailError::Invalid));
        let mut big = env("big", "s1");
        big.text = "x".repeat(MAX_REPLY + 1);
        assert_eq!(m.enqueue(big), Err(MailError::TooLarge));
        m.enqueue(env("ok", "s1")).unwrap();
        let dir = m.root.join("s1");
        // tampered text: signature no longer matches
        let f = dir.join("request.ok.json");
        let s = std::fs::read_to_string(&f)
            .unwrap()
            .replace("hello", "rm -rf");
        std::fs::write(&f, s).unwrap();
        // signed with another key
        let other = Key::from_bytes_for_test([9; 32]);
        let mut e = env("alien", "s1");
        e.sign(&other);
        std::fs::write(
            dir.join("request.alien.json"),
            serde_json::to_vec(&e).unwrap(),
        )
        .unwrap();
        // a valid envelope under the wrong file name
        let mut e = env("named", "s1");
        e.sign(&m.key);
        std::fs::write(
            dir.join("request.other.json"),
            serde_json::to_vec(&e).unwrap(),
        )
        .unwrap();
        // expired
        let mut e = env("old", "s1");
        e.created = now_s() - TTL_S - 10;
        e.sign(&m.key);
        std::fs::write(
            dir.join("request.old.json"),
            serde_json::to_vec(&e).unwrap(),
        )
        .unwrap();
        // created in the future
        let mut e = env("soon", "s1");
        e.created = now_s() + SKEW_S + 60;
        e.sign(&m.key);
        std::fs::write(
            dir.join("request.soon.json"),
            serde_json::to_vec(&e).unwrap(),
        )
        .unwrap();
        // oversized file
        std::fs::write(
            dir.join("request.huge.json"),
            vec![b' '; MAX_FILE as usize + 1],
        )
        .unwrap();
        assert!(m.pending("s1").is_empty());
        assert!(Mailbox::files(&dir).is_empty(), "every bad file is deleted");
    }

    #[test]
    fn from_chain_and_the_whole_envelope_are_bounded() {
        let (_t, m) = mb();
        let mut e = env("f1", "s1");
        e.from.workspace_root = "r".repeat(300);
        assert_eq!(m.enqueue(e), Err(MailError::Invalid));
        let mut e = env("c1", "s1");
        e.chain = vec!["a".into(); MAX_CHAIN + 1];
        assert_eq!(m.enqueue(e), Err(MailError::Invalid));
        let mut e = env("c2", "s1");
        e.chain = vec!["a".repeat(65)];
        assert_eq!(m.enqueue(e), Err(MailError::Invalid));
        let mut e = env("t1", "s1");
        e.created = now_s() + SKEW_S + 60;
        assert_eq!(m.enqueue(e), Err(MailError::Invalid));
        let mut e = env("t2", "s1");
        e.created = now_s() - TTL_S - 60;
        assert_eq!(m.enqueue(e), Err(MailError::Invalid));
        let mut e = env("k1", "s1");
        e.kind = "other".into();
        assert_eq!(m.enqueue(e), Err(MailError::Invalid));
        // text at the cap whose JSON escapes push the file past 128 KiB
        let mut e = env("big2", "s1");
        e.text = "\u{1}".repeat(MAX_REPLY);
        assert_eq!(m.enqueue(e), Err(MailError::TooLarge));
        assert!(m.pending("s1").is_empty());
    }

    #[test]
    fn queue_is_bounded_per_session_and_in_total() {
        let (_t, m) = mb();
        for i in 0..MAX_QUEUED {
            m.enqueue(env(&format!("r{i}"), "s1")).unwrap();
        }
        assert_eq!(m.enqueue(env("extra", "s1")), Err(MailError::Full));
        // delivery frees room
        m.done("s1", "request", "r0");
        assert_eq!(m.enqueue(env("extra", "s1")), Ok(Queued::Stored));
        for s in 2..=(MAX_TOTAL / MAX_QUEUED + 1) {
            for i in 0..MAX_QUEUED {
                let _ = m.enqueue(env(&format!("q{s}-{i}"), &format!("s{s}")));
            }
        }
        assert!(m.total() <= MAX_TOTAL);
        let dirs = m.dirs();
        assert_eq!(m.enqueue(env("late", "s99")), Err(MailError::Full));
        assert_eq!(m.dirs(), dirs, "a refused message creates no directory");
        assert!(!m.root.join("s99").exists());
    }

    #[test]
    fn a_crashed_write_leaves_no_message_and_is_swept() {
        let (_t, m) = mb();
        m.enqueue(env("a", "s1")).unwrap();
        let dir = m.root.join("s1");
        let tmp = dir.join("request.b.tmp1234abcd");
        std::fs::write(&tmp, b"{\"half").unwrap();
        assert_eq!(m.pending("s1").len(), 1, "a temp file is not a message");
        assert!(tmp.exists(), "a fresh temp may belong to a live writer");
    }

    #[cfg(unix)]
    #[test]
    fn an_unreadable_file_is_kept_for_a_retry_not_deleted() {
        use std::os::unix::fs::PermissionsExt;
        let (_t, m) = mb();
        m.enqueue(env("a", "s1")).unwrap();
        let f = m.root.join("s1").join("request.a.json");
        std::fs::set_permissions(&f, std::fs::Permissions::from_mode(0o000)).unwrap();
        if std::fs::read(&f).is_err() {
            assert!(m.pending("s1").is_empty());
            assert!(
                f.exists(),
                "a transient read error must not delete the message"
            );
        }
        std::fs::set_permissions(&f, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(m.pending("s1").len(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn a_linked_or_unreadable_ledger_fails_closed() {
        let (t, m) = mb();
        m.enqueue(env("a", "s1")).unwrap();
        m.done("s1", "request", "a");
        let dir = m.root.join("s1");
        let real = t.path().join("elsewhere");
        std::fs::write(&real, "request:a\t1\n").unwrap();
        std::fs::remove_file(dir.join("done")).unwrap();
        std::os::unix::fs::symlink(&real, dir.join("done")).unwrap();
        // the dedupe ledger cannot be trusted: nothing is stored or shown, nothing is written through
        assert_eq!(m.enqueue(env("b", "s1")), Err(MailError::Io));
        assert!(m.pending("s1").is_empty());
        m.done("s1", "request", "b");
        assert_eq!(std::fs::read_to_string(&real).unwrap(), "request:a\t1\n");
        // the expect ledger likewise
        std::os::unix::fs::symlink(&real, dir.join("expect")).unwrap();
        assert!(m.expect("s1", "x", &entry("t")).is_err());
        assert!(!m.expected("s1", "a", &reply_from("t", "k")));
    }

    #[test]
    fn cards_make_a_dead_session_addressable_for_a_while() {
        let (_t, m) = mb();
        let e = entry("s1");
        m.write_card(&e).unwrap();
        let cards = m.offline_cards(&[]);
        assert_eq!(cards.len(), 1);
        assert!(
            cards[0].endpoint.is_empty() && cards[0].session_name.as_deref() == Some("backend")
        );
        assert!(
            m.offline_cards(std::slice::from_ref(&e)).is_empty(),
            "live wins"
        );
    }

    #[test]
    fn forged_oversized_and_misplaced_cards_are_ignored() {
        let (_t, m) = mb();
        m.write_card(&entry("s1")).unwrap();
        let card = m.root.join("s1").join("card.entry");
        // tampered: the name changes, the signature no longer fits
        let s = std::fs::read_to_string(&card)
            .unwrap()
            .replace("backend", "evil");
        std::fs::write(&card, s).unwrap();
        assert!(m.offline_cards(&[]).is_empty(), "unsigned change");
        // a card for s2 copied into s3's directory
        m.write_card(&entry("s2")).unwrap();
        m.write_card(&entry("s3")).unwrap();
        std::fs::copy(m.root.join("s2/card.entry"), m.root.join("s3/card.entry")).unwrap();
        let ids: Vec<_> = m
            .offline_cards(&[])
            .into_iter()
            .map(|c| c.session_id)
            .collect();
        assert_eq!(ids, ["s2"], "s3 holds s2's card");
        // signed by another key
        let other = Mailbox::new(m.root.parent().unwrap(), &Key::from_bytes_for_test([5; 32]));
        other.write_card(&entry("s4")).unwrap();
        assert!(!m.offline_cards(&[]).iter().any(|c| c.session_id == "s4"));
        // oversized
        std::fs::write(
            m.root.join("s2/card.entry"),
            vec![b' '; MAX_FILE as usize + 1],
        )
        .unwrap();
        assert!(m.offline_cards(&[]).is_empty());
    }

    #[test]
    fn expected_replies_only_from_who_was_asked() {
        let (_t, m) = mb();
        let target = entry("t1");
        let from = |s: &str, ws: &str| reply_from(s, ws);
        assert!(!m.expected("s1", "r1", &from("t1", "k")));
        m.expect("s1", "r1", &target).unwrap();
        assert!(m.expected("s1", "r1", &from("t1", "k")));
        assert!(!m.expected("s1", "r2", &from("t1", "k")));
        assert!(
            !m.expected("s1", "r1", &from("other", "k")),
            "another session"
        );
        assert!(
            !m.expected("s1", "r1", &from("t1", "x")),
            "another workspace"
        );
    }

    #[test]
    fn a_full_ledger_refuses_instead_of_forgetting() {
        let (_t, m) = mb();
        let dir = m.dir("s1").unwrap();
        let p = dir.join("expect");
        let now = now_s().to_string();
        let rows: String = (0..LEDGER_MAX)
            .map(|i| format!("id{i}\t{now}\tt\tk\n"))
            .collect();
        std::fs::write(&p, rows).unwrap();
        assert!(
            ledger_add(&p, &["new", "t", "k"]).is_err(),
            "pending ids are kept"
        );
        // rows past their TTL make room
        let old = (now_s() - TTL_S - 4 * SKEW_S).to_string();
        let rows: String = (0..LEDGER_MAX)
            .map(|i| format!("id{i}\t{old}\tt\tk\n"))
            .collect();
        std::fs::write(&p, rows).unwrap();
        ledger_add(&p, &["new", "t", "k"]).unwrap();
        assert_eq!(ledger(&p).unwrap().len(), 1);
    }
}
