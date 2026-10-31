//! Cross-session peer messaging transport (docs/specs/messaging.md Part B). OFF unless
//! `synthesis.peers.enabled` in user settings. Same OS user, same machine; no TCP listener.
//!
//! This crate is the transport, registry, policy and protocol. The model-facing parts (the
//! read-only responder, `SendMessage`, inbox) plug in through [`serve::Responder`].

pub mod address;
pub mod net;
pub mod serve;

use std::io::{self, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// 32-byte shared key (`run/peer.key`). Debug never prints it.
#[derive(Clone)]
pub struct Key(pub(crate) [u8; 32]);

impl std::fmt::Debug for Key {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Key(..)")
    }
}

impl Key {
    /// For tests and in-memory keys; production keys come from [`load_or_create_key`].
    pub fn from_bytes_for_test(b: [u8; 32]) -> Self {
        Self(b)
    }
}

/// `$SYNTHESIS_HOME/run`; the home comes from the caller's `Env`, never from `HOME`.
pub fn run_dir(synthesis_home: &Path) -> PathBuf {
    synthesis_home.join("run")
}

/// On Windows peers stay off unless the home is under the user profile (ACL inheritance).
pub fn platform_ok(home: &Path, user_profile: Option<&Path>) -> bool {
    !cfg!(windows) || user_profile.is_some_and(|u| components_start_with(home, u))
}

/// Component-wise prefix test, folded: `C:\Users\bob2` is not under `C:\Users\bob`.
fn components_start_with(path: &Path, prefix: &Path) -> bool {
    let parts = |p: &Path| -> Vec<String> {
        fold(&p.to_string_lossy())
            .split('/')
            .filter(|s| !s.is_empty())
            .map(String::from)
            .collect()
    };
    let (p, x) = (parts(path), parts(prefix));
    !x.is_empty() && p.len() >= x.len() && p[..x.len()] == x[..]
}

/// An endpoint a registry entry names must be ours to connect to: a `.sock` under the run dir
/// (unix) or a `synthesis-peer-` pipe (Windows). Anything else is a planted entry.
pub fn endpoint_ok(run_dir: &Path, endpoint: &str) -> bool {
    if cfg!(windows) {
        endpoint.starts_with(r"\\.\pipe\synthesis-peer-")
            && endpoint
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || r"\.-".contains(c))
    } else {
        let e = Path::new(endpoint);
        e.starts_with(run_dir)
            && e.extension().is_some_and(|x| x == "sock")
            && !e
                .components()
                .any(|c| matches!(c, std::path::Component::ParentDir))
    }
}

/// Write `bytes` 0600 (unix) via temp + rename.
fn write_private(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let tmp = path.with_extension(format!("tmp{}", net::hex(&net::random::<4>()?)));
    let mut o = std::fs::OpenOptions::new();
    o.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut o, 0o600);
    o.open(&tmp)?.write_all(bytes)?;
    std::fs::rename(&tmp, path).inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })
}

/// Read `run/peer.key`, creating it when absent: 32 random bytes written to a 0600 temp file and
/// hard-linked into place, so a reader never sees a partial key and two creators cannot clobber
/// each other. The key must be a regular file of ours (not a link), mode 0600 on unix.
pub fn load_or_create_key(run_dir: &Path) -> io::Result<Key> {
    net::prepare_run_dir(run_dir)?;
    let path = run_dir.join("peer.key");
    if std::fs::symlink_metadata(&path).is_err() {
        let tmp = run_dir.join(format!(".key{}", net::hex(&net::random::<8>()?)));
        write_private(&tmp, &net::random::<32>()?)?;
        let linked = std::fs::hard_link(&tmp, &path);
        let _ = std::fs::remove_file(&tmp);
        match linked {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e),
        }
    }
    let bad = |m: &'static str| io::Error::new(io::ErrorKind::InvalidData, m);
    let meta = std::fs::symlink_metadata(&path)?;
    if !meta.is_file() {
        return Err(bad("peer.key is not a regular file"));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        if meta.permissions().mode() & 0o077 != 0 {
            return Err(bad(
                "peer.key is accessible to other users (expected mode 0600)",
            ));
        }
        if meta.uid() != std::fs::metadata(run_dir)?.uid() {
            return Err(bad(
                "peer.key is not owned by the owner of the run directory",
            ));
        }
    }
    std::fs::read(&path)?
        .try_into()
        .map(Key)
        .map_err(|_| bad("peer.key is not 32 bytes"))
}

/// Case/separator folding used by workspace rules (messaging.md section 1).
pub fn fold(s: &str) -> String {
    if cfg!(windows) {
        s.replace('\\', "/").to_lowercase()
    } else if cfg!(target_os = "macos") {
        s.to_lowercase()
    } else {
        s.to_string()
    }
}

/// Who an agent is (messaging.md B5 `Addr`).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Addr {
    pub workspace_key: String,
    pub workspace_label: String,
    pub workspace_root: String,
    pub session_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<String>,
    pub instance: String,
    #[serde(default)]
    pub kind: String,
}

impl Addr {
    /// Every string cut to 256 chars: an address from a peer is shown to a model and logged.
    pub fn clamped(mut self) -> Self {
        let cut = |s: &mut String| *s = s.chars().take(256).collect();
        cut(&mut self.workspace_key);
        cut(&mut self.workspace_label);
        cut(&mut self.workspace_root);
        cut(&mut self.session_id);
        cut(&mut self.instance);
        cut(&mut self.kind);
        for o in [&mut self.session_name, &mut self.agent_id]
            .into_iter()
            .flatten()
        {
            cut(o);
        }
        self
    }

    /// `ws:<key12>/sess:<session_id>/agent:<instance>`
    pub fn canonical(&self) -> String {
        let k = &self.workspace_key[..self.workspace_key.len().min(12)];
        format!("ws:{k}/sess:{}/agent:{}", self.session_id, self.instance)
    }

    /// `ws:<label>/sess:<name|sid8>/agent:<instance>`
    pub fn display(&self) -> String {
        let s = self
            .session_name
            .clone()
            .unwrap_or_else(|| self.session_id.chars().take(8).collect());
        format!(
            "ws:{}/sess:{s}/agent:{}",
            self.workspace_label, self.instance
        )
    }
}

/// `synthesis.peers` (B8.5). User settings and CLI only; the caller must not pass project values.
#[derive(Debug, Clone, PartialEq)]
pub struct Config {
    pub enabled: bool,
    pub allow: Vec<String>,
    pub deny: Vec<String>,
    pub initiate: bool,
    pub answer: bool,
    pub confirm_replies: bool,
    pub max_hops: usize,
    pub max_cost_usd: f64,
    pub resume_offline: bool,
}

impl Config {
    /// Defaults: off; once on, the allow list is this workspace only (D9).
    pub fn from_settings(peers: &Map<String, Value>, workspace_root: &str) -> Self {
        let flag = |k, d| peers.get(k).and_then(Value::as_bool).unwrap_or(d);
        let list = |k: &str| -> Option<Vec<String>> {
            peers.get(k)?.as_array().map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect()
            })
        };
        Self {
            enabled: flag("enabled", false),
            allow: list("allow").unwrap_or_else(|| vec![format!("workspace:{workspace_root}")]),
            deny: list("deny").unwrap_or_default(),
            initiate: flag("initiate", true),
            answer: flag("answer", true),
            confirm_replies: flag("confirm_replies", false),
            max_hops: peers
                .get("max_hops")
                .and_then(Value::as_u64)
                .map_or(2, |n| n as usize),
            max_cost_usd: peers
                .get("max_cost_usd")
                .and_then(Value::as_f64)
                .unwrap_or(1.0),
            resume_offline: flag("resume_offline", false),
        }
    }

    /// Deny beats allow. The same list governs whom I may ask and who may ask me.
    pub fn permits(&self, p: &Addr) -> bool {
        !self.deny.iter().any(|r| rule_matches(r, p))
            && self.allow.iter().any(|r| rule_matches(r, p))
    }
}

fn rule_matches(rule: &str, p: &Addr) -> bool {
    if let Some(x) = rule.strip_prefix("ws:") {
        x == p.workspace_label || (x.len() >= 6 && p.workspace_key.starts_with(x))
    } else if let Some(x) = rule.strip_prefix("sess:") {
        p.session_name.as_deref() == Some(x) || (x.len() >= 8 && p.session_id.starts_with(x))
    } else if let Some(g) = rule.strip_prefix("workspace:") {
        // shortcut: only a trailing `*` is a wildcard; upgrade to full globs when a rule needs one.
        let (root, g) = (fold(&p.workspace_root), fold(g));
        match g.strip_suffix('*') {
            Some(prefix) => root.starts_with(prefix),
            None => root.trim_end_matches('/') == g.trim_end_matches('/'),
        }
    } else {
        false
    }
}

/// `run/<session_id>.json` (B4).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Entry {
    pub v: u32,
    pub session_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_name: Option<String>,
    pub workspace_key: String,
    pub workspace_label: String,
    pub workspace_root: String,
    pub cwd: String,
    pub pid: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pid_start: Option<u64>,
    pub endpoint: String,
    pub started_at: u64,
}

impl Entry {
    pub fn addr(&self, instance: &str, kind: &str) -> Addr {
        Addr {
            workspace_key: self.workspace_key.clone(),
            workspace_label: self.workspace_label.clone(),
            workspace_root: self.workspace_root.clone(),
            session_id: self.session_id.clone(),
            session_name: self.session_name.clone(),
            agent_id: None,
            instance: instance.into(),
            kind: kind.into(),
        }
    }
}

/// A written registry entry; removed on drop (clean exit).
pub struct Registration(PathBuf);

impl Registration {
    pub fn create(run_dir: &Path, e: &Entry) -> io::Result<Self> {
        if e.session_id.is_empty()
            || !e
                .session_id
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-')
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "bad session id",
            ));
        }
        let path = run_dir.join(format!("{}.json", e.session_id));
        write_private(&path, &serde_json::to_vec(e).map_err(io::Error::other)?)?;
        Ok(Self(path))
    }
}

impl Drop for Registration {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// Every parseable `run/*.json` (v1). Unreadable or foreign files are skipped.
pub fn read_entries(run_dir: &Path) -> Vec<Entry> {
    let Ok(rd) = std::fs::read_dir(run_dir) else {
        return Vec::new();
    };
    let mut v: Vec<Entry> = rd
        .filter_map(Result::ok)
        .filter(|f| f.path().extension().is_some_and(|x| x == "json"))
        .filter_map(|f| serde_json::from_slice::<Entry>(&std::fs::read(f.path()).ok()?).ok())
        .filter(|e| e.v == 1)
        .collect();
    v.sort_by(|a, b| a.session_id.cmp(&b.session_id));
    v
}

/// Delete the entry of a dead session (the caller proved the writer lock is free).
pub fn remove_entry(run_dir: &Path, session_id: &str) {
    if session_id
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-')
    {
        let _ = std::fs::remove_file(run_dir.join(format!("{session_id}.json")));
    }
}

/// A session name is unique among live sessions of one workspace; a taken name is rejected.
pub fn name_taken(entries: &[Entry], workspace_key: &str, name: &str) -> bool {
    entries
        .iter()
        .any(|e| e.workspace_key == workspace_key && e.session_name.as_deref() == Some(name))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn addr(label: &str, root: &str, sess: &str, name: Option<&str>) -> Addr {
        Addr {
            workspace_key: "0123456789abcdef01234567".into(),
            workspace_label: label.into(),
            workspace_root: root.into(),
            session_id: sess.into(),
            session_name: name.map(Into::into),
            instance: "main".into(),
            ..Default::default()
        }
    }

    fn cfg(v: Value) -> Config {
        Config::from_settings(v.as_object().unwrap(), "/work/api")
    }

    #[test]
    fn defaults_are_off_and_same_workspace() {
        let c = cfg(json!({}));
        assert!(!c.enabled && c.initiate && c.answer && !c.confirm_replies && !c.resume_offline);
        assert_eq!((c.max_hops, c.max_cost_usd), (2, 1.0));
        assert!(c.permits(&addr("api", "/work/api", "s1", None)));
        assert!(!c.permits(&addr("web", "/work/web", "s2", None)));
    }

    #[test]
    fn deny_beats_allow_and_rules_match() {
        let c = cfg(json!({"allow":["ws:web","sess:backend","ws:012345"],"deny":["sess:secret"]}));
        assert!(c.permits(&addr("web", "/w", "s", None)));
        assert!(c.permits(&addr("x", "/w", "s", Some("backend"))));
        assert!(c.permits(&addr("x", "/w", "s", None))); // key prefix 012345
        assert!(!c.permits(&addr("web", "/w", "s", Some("secret"))));
        let c = cfg(json!({"allow":["workspace:/work/*"]}));
        assert!(c.permits(&addr("web", "/work/web", "s", None)));
        assert!(!c.permits(&addr("web", "/other/web", "s", None)));
    }

    #[test]
    fn profile_prefix_is_by_component() {
        let p = Path::new;
        assert!(components_start_with(p("/u/bob/.synthesis"), p("/u/bob")));
        assert!(!components_start_with(p("/u/bob2/.synthesis"), p("/u/bob")));
        assert!(!components_start_with(p("/u/bob"), p("")));
    }

    #[test]
    fn addresses() {
        let a = addr("my-app", "/r", "3c9e01aa-1111", Some("backend"));
        assert_eq!(
            a.canonical(),
            "ws:0123456789ab/sess:3c9e01aa-1111/agent:main"
        );
        assert_eq!(a.display(), "ws:my-app/sess:backend/agent:main");
        assert_eq!(
            addr("a", "/r", "3c9e01aa-1", None).display(),
            "ws:a/sess:3c9e01aa/agent:main"
        );
    }

    #[test]
    fn registry_roundtrip_unique_names_and_cleanup() {
        let t = tempfile::tempdir().unwrap();
        let dir = run_dir(t.path());
        net::prepare_run_dir(&dir).unwrap();
        let e = Entry {
            v: 1,
            session_id: "aaaa-bbbb".into(),
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
        let reg = Registration::create(&dir, &e).unwrap();
        let all = read_entries(&dir);
        assert_eq!(all, vec![e.clone()]);
        assert!(name_taken(&all, "k", "backend") && !name_taken(&all, "other", "backend"));
        assert!(
            Registration::create(
                &dir,
                &Entry {
                    session_id: "../x".into(),
                    ..e
                }
            )
            .is_err()
        );
        drop(reg);
        assert!(read_entries(&dir).is_empty());
    }

    #[test]
    fn key_is_created_once_and_hidden() {
        let t = tempfile::tempdir().unwrap();
        let dir = run_dir(t.path());
        let a = load_or_create_key(&dir).unwrap();
        let b = load_or_create_key(&dir).unwrap();
        assert_eq!(a.0, b.0);
        assert_eq!(format!("{a:?}"), "Key(..)");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let m = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
            assert_eq!((m(&dir), m(&dir.join("peer.key"))), (0o700, 0o600));
        }
    }
}
