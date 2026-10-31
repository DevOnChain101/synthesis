//! Durable mailbox for sessions that are offline (messaging.md R10c). A request to a dead session
//! and a reply to a dead asker are stored as one file each under `run/mailbox/<session_id>/`, signed
//! with the shared key (HMAC, like the live handshake), and delivered when that session comes back.
//!
//! Guarantees: written temp + fsync + rename (a crash leaves a whole file or none); an id is
//! stored once (file or `done` ledger) so a retry never duplicates; bounded per session, in total
//! and by age; a file whose signature, id or size is wrong is deleted unread.

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
/// An undelivered envelope is dropped after this long.
pub const TTL_S: u64 = 7 * 24 * 3600;
/// An offline session stays addressable for this long after its last start.
pub const CARD_TTL_S: u64 = 30 * 24 * 3600;
const LEDGER_MAX: usize = 1024;
const MAX_FILE: u64 = 128 * 1024;

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
        self.mac = net::proof(key, b"m", &self.body(), b"");
    }

    pub fn verified(&self, key: &Key) -> bool {
        net::verify(key, b"m", &self.body(), b"", Some(&self.mac))
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
    /// Per-session or total bound reached.
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

fn lines(p: &Path) -> Vec<String> {
    std::fs::read_to_string(p)
        .unwrap_or_default()
        .lines()
        .map(String::from)
        .collect()
}

/// Appends `id`; past the cap the older half is dropped (rewritten whole, never truncated in place).
fn ledger_add(p: &Path, id: &str) -> io::Result<()> {
    let mut v = lines(p);
    if v.len() >= LEDGER_MAX {
        v.drain(..v.len() / 2);
        v.push(id.into());
        return write_private(p, v.join("\n").as_bytes());
    }
    let mut o = std::fs::OpenOptions::new();
    o.append(true).create(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut o, 0o600);
    let mut f = o.open(p)?;
    f.write_all(format!("{id}\n").as_bytes())?;
    f.sync_all()
}

fn private_dir(p: &Path, parent: &Path) -> io::Result<()> {
    match std::fs::symlink_metadata(p) {
        Ok(m) if m.is_dir() => {}
        Ok(_) => return Err(io::Error::other("mailbox path is not a directory")),
        Err(_) => {
            let mut b = std::fs::DirBuilder::new();
            #[cfg(unix)]
            std::os::unix::fs::DirBuilderExt::mode(&mut b, 0o700);
            b.create(p)?;
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

impl Mailbox {
    pub fn new(run_dir: &Path, key: &Key) -> Self {
        Self {
            root: run_dir.join("mailbox"),
            key: key.clone(),
        }
    }

    fn dir(&self, sid: &str) -> Result<PathBuf, MailError> {
        if !valid_id(sid) {
            return Err(MailError::Invalid);
        }
        let d = self.root.join(sid);
        let run = self.root.parent().unwrap_or(&self.root);
        private_dir(&self.root, run).map_err(|_| MailError::Io)?;
        private_dir(&d, &self.root).map_err(|_| MailError::Io)?;
        Ok(d)
    }

    /// The directory when it already exists and is ours; reading never creates anything.
    fn existing(&self, sid: &str) -> Option<PathBuf> {
        let d = self.root.join(sid);
        (valid_id(sid) && private_dir(&d, &self.root).is_ok() && d.is_dir()).then_some(d)
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

    /// Stores `env` for `env.to_session`, signed. Idempotent on `env.id`.
    pub fn enqueue(&self, mut env: Envelope) -> Result<Queued, MailError> {
        if !valid_id(&env.id) || !valid_id(&env.to_session) {
            return Err(MailError::Invalid);
        }
        if env.text.len() > MAX_REPLY {
            return Err(MailError::TooLarge);
        }
        let dir = self.dir(&env.to_session)?;
        let file = dir.join(format!("{}.json", env.id));
        if file.exists() || lines(&dir.join("done")).contains(&env.id) {
            return Ok(Queued::Duplicate);
        }
        if self.pending(&env.to_session).len() >= MAX_QUEUED || self.total() >= MAX_TOTAL {
            return Err(MailError::Full);
        }
        env.sign(&self.key);
        let bytes = serde_json::to_vec(&env).map_err(|_| MailError::Io)?;
        write_private(&file, &bytes).map_err(|_| MailError::Io)?;
        Ok(Queued::Stored)
    }

    /// Valid envelopes for `sid`, oldest first. Forged, oversized, expired, misnamed and already
    /// delivered files are deleted; leftover temp files of a crashed write are swept.
    pub fn pending(&self, sid: &str) -> Vec<Envelope> {
        let Some(dir) = self.existing(sid) else {
            return vec![];
        };
        let done = lines(&dir.join("done"));
        let mut out = vec![];
        for p in Self::files(&dir) {
            let env = std::fs::metadata(&p)
                .ok()
                .filter(|m| m.len() <= MAX_FILE)
                .and_then(|_| std::fs::read(&p).ok())
                .and_then(|b| serde_json::from_slice::<Envelope>(&b).ok())
                .filter(|e| {
                    e.verified(&self.key)
                        && e.to_session == sid
                        && p.file_stem().is_some_and(|s| *s == *e.id)
                        && now_s().saturating_sub(e.created) <= TTL_S
                        && !done.contains(&e.id)
                });
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

    /// Marks `id` delivered (ledger first, so a crash before the delete cannot redeliver) and
    /// removes the file.
    pub fn done(&self, sid: &str, id: &str) {
        if let Ok(d) = self.dir(sid)
            && valid_id(id)
            && ledger_add(&d.join("done"), id).is_ok()
        {
            let _ = std::fs::remove_file(d.join(format!("{id}.json")));
        }
    }

    pub fn delivered(&self, sid: &str, id: &str) -> bool {
        self.existing(sid)
            .is_some_and(|d| lines(&d.join("done")).iter().any(|l| l == id))
    }

    /// The asker records the id of a request it queued; only such replies are accepted later.
    pub fn expect(&self, sid: &str, id: &str) -> Result<(), MailError> {
        let d = self.dir(sid)?;
        if !valid_id(id) {
            return Err(MailError::Invalid);
        }
        ledger_add(&d.join("expect"), id).map_err(|_| MailError::Io)
    }

    pub fn expected(&self, sid: &str, id: &str) -> bool {
        self.existing(sid)
            .is_some_and(|d| lines(&d.join("expect")).iter().any(|l| l == id))
    }

    /// Makes an offline session addressable (kept after the process exits).
    pub fn write_card(&self, e: &Entry) -> io::Result<()> {
        let d = self
            .dir(&e.session_id)
            .map_err(|_| io::Error::other("bad session id"))?;
        let card = Entry {
            endpoint: String::new(),
            ..e.clone()
        };
        write_private(
            &d.join("card.entry"),
            &serde_json::to_vec(&card).map_err(io::Error::other)?,
        )
    }

    /// Cards of sessions that are not in `live` and were started within [`CARD_TTL_S`].
    pub fn offline_cards(&self, live: &[Entry]) -> Vec<Entry> {
        let mut out = vec![];
        for d in std::fs::read_dir(&self.root)
            .into_iter()
            .flatten()
            .flatten()
        {
            let (p, name) = (d.path().join("card.entry"), d.file_name());
            let fresh = std::fs::metadata(&p)
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| t.elapsed().ok())
                .is_some_and(|a| a.as_secs() <= CARD_TTL_S);
            let card = std::fs::read(&p)
                .ok()
                .and_then(|b| serde_json::from_slice::<Entry>(&b).ok())
                .filter(|e| {
                    e.v == 1
                        && e.endpoint.is_empty()
                        && *name == *e.session_id
                        && valid_id(&e.session_id)
                        && !live.iter().any(|l| l.session_id == e.session_id)
                });
            match card {
                Some(c) if fresh => out.push(c),
                Some(_) => {
                    let _ = std::fs::remove_file(&p);
                }
                None => {}
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
        again.done("s1", "r1");
        assert!(again.pending("s1").is_empty() && again.delivered("s1", "r1"));
        // the sender retrying after delivery writes nothing
        assert_eq!(again.enqueue(env("r1", "s1")), Ok(Queued::Duplicate));
        assert!(again.pending("s1").is_empty());
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
        let f = dir.join("ok.json");
        let s = std::fs::read_to_string(&f)
            .unwrap()
            .replace("hello", "rm -rf");
        std::fs::write(&f, s).unwrap();
        // signed with another key
        let other = Key::from_bytes_for_test([9; 32]);
        let mut e = env("alien", "s1");
        e.sign(&other);
        std::fs::write(dir.join("alien.json"), serde_json::to_vec(&e).unwrap()).unwrap();
        // a valid envelope under the wrong file name
        let mut e = env("named", "s1");
        e.sign(&m.key);
        std::fs::write(dir.join("other.json"), serde_json::to_vec(&e).unwrap()).unwrap();
        // expired
        let mut e = env("old", "s1");
        e.created = now_s() - TTL_S - 10;
        e.sign(&m.key);
        std::fs::write(dir.join("old.json"), serde_json::to_vec(&e).unwrap()).unwrap();
        // oversized file
        std::fs::write(dir.join("huge.json"), vec![b' '; MAX_FILE as usize + 1]).unwrap();
        assert!(m.pending("s1").is_empty());
        assert!(Mailbox::files(&dir).is_empty(), "every bad file is deleted");
    }

    #[test]
    fn queue_is_bounded_per_session_and_in_total() {
        let (_t, m) = mb();
        for i in 0..MAX_QUEUED {
            m.enqueue(env(&format!("r{i}"), "s1")).unwrap();
        }
        assert_eq!(m.enqueue(env("extra", "s1")), Err(MailError::Full));
        // delivery frees room
        m.done("s1", "r0");
        assert_eq!(m.enqueue(env("extra", "s1")), Ok(Queued::Stored));
        for s in 2..=(MAX_TOTAL / MAX_QUEUED + 1) {
            for i in 0..MAX_QUEUED {
                let _ = m.enqueue(env(&format!("q{s}-{i}"), &format!("s{s}")));
            }
        }
        assert!(m.total() <= MAX_TOTAL);
        assert_eq!(m.enqueue(env("late", "s99")), Err(MailError::Full));
    }

    #[test]
    fn a_crashed_write_leaves_no_message_and_is_swept() {
        let (_t, m) = mb();
        m.enqueue(env("a", "s1")).unwrap();
        let dir = m.root.join("s1");
        let tmp = dir.join("b.tmp1234abcd");
        std::fs::write(&tmp, b"{\"half").unwrap();
        assert_eq!(m.pending("s1").len(), 1, "a temp file is not a message");
        assert!(tmp.exists(), "a fresh temp may belong to a live writer");
    }

    #[test]
    fn cards_make_a_dead_session_addressable_for_a_while() {
        let (_t, m) = mb();
        let e = Entry {
            v: 1,
            session_id: "s1".into(),
            session_name: Some("backend".into()),
            workspace_key: "k".into(),
            workspace_label: "api".into(),
            workspace_root: "/r".into(),
            cwd: "/r".into(),
            pid: 1,
            pid_start: None,
            endpoint: "ep".into(),
            started_at: 0,
        };
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
    fn expected_replies_only() {
        let (_t, m) = mb();
        assert!(!m.expected("s1", "r1"));
        m.expect("s1", "r1").unwrap();
        assert!(m.expected("s1", "r1") && !m.expected("s1", "r2"));
    }
}
