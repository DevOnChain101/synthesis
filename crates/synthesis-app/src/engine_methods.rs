//! The store-backed and trust methods of `synt engine --stdio` (docs/specs/desktop.md section 8). Every
//! input is validated here (length, charset); the client never names a path, only engine-issued ids and
//! keys that are compared for equality; errors are fixed texts (the detail goes to stderr, never over the
//! wire); every answer is bounded. The store is the one of THIS machine (inside WSL, the distro's).

use std::fmt::Display;
use std::path::{Path, PathBuf};

use serde_json::{Value, json};
use synthesis_config::Env;
use synthesis_harness::sanitize_for_display;
use synthesis_session::{Reader, trust};
use synthesis_view::workspaces::{LoadOpts, WorkspaceList, fold_dir};

use crate::trust::{Grant, parent_refusal, remembered_trust, root_of, trust_policy, trust_refusal};

pub const MAX_WORKSPACES: usize = 200;
pub const MAX_SESSIONS: usize = 200;
/// An answer is one protocol line (8 MiB): the preview stays well under it.
pub const MAX_PREVIEW_BYTES: usize = 4 << 20;
pub const MAX_PREVIEW_EVENTS: usize = 50_000;
/// One event over this is left out of a preview (a huge tool result).
const MAX_PREVIEW_EVENT: usize = 1 << 20;
const MAX_KEY: usize = 4096;
/// Events per page of the preview scan.
const PREVIEW_PAGE: usize = 200;
/// Bytes of events one preview may scan (the newest ones seen are kept).
#[cfg(not(test))]
const MAX_PREVIEW_SCAN: usize = 64 << 20;
#[cfg(test)]
const MAX_PREVIEW_SCAN: usize = 6 << 20;
const MAX_MODEL: usize = 200;
pub const MAX_INSTRUCTIONS: usize = 8192;
const MAX_FILES: usize = 20;
/// Rows of `trust.state` whose configuration is fingerprinted again.
const MAX_FINGERPRINTS: usize = 20;

/// The detail of a failure goes to stderr; the wire gets `what` (no path of this machine in it).
pub fn failed<E: Display>(what: &'static str) -> impl FnOnce(E) -> String {
    move |e| {
        eprintln!(
            "synt engine: {what}: {}",
            sanitize_for_display(&e.to_string())
        );
        what.to_string()
    }
}

/// An engine-issued session id: what the store makes (`[A-Za-z0-9-]`).
pub fn check_id(id: &str) -> Result<(), String> {
    if id.is_empty() || id.len() > 64 || !id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
    {
        return Err("not a session id".into());
    }
    Ok(())
}

/// A workspace key as `workspaces.list` issued it: only ever compared, never opened.
pub fn check_key(k: &str) -> Result<(), String> {
    if k.is_empty() || k.len() > MAX_KEY || k.chars().any(char::is_control) {
        return Err("not a workspace key".into());
    }
    Ok(())
}

pub fn check_model(m: &str) -> Result<(), String> {
    if m.trim().is_empty()
        || m.len() > MAX_MODEL
        || m.chars().any(char::is_control)
        || m.starts_with('-')
    {
        return Err("not a model name".into());
    }
    Ok(())
}

pub fn check_instructions(t: &str) -> Result<(), String> {
    if t.len() > MAX_INSTRUCTIONS {
        return Err("the instructions are too long".into());
    }
    Ok(())
}

/// What the methods need to know about this engine.
pub struct Ctx<'a> {
    pub home: &'a Path,
    pub user_home: Option<&'a Path>,
    pub env: &'a Env,
    /// The sandbox is enforcing: remembered trust is honoured only then (see `launch_common`).
    pub enforcing: bool,
}

impl Ctx<'_> {
    fn reader(&self) -> Result<Reader, String> {
        Reader::open(self.home).map_err(failed("cannot read the session store"))
    }

    fn records(&self) -> Vec<trust::TrustRecord> {
        if self.enforcing {
            trust::list(self.home)
                .map(|l| l.records)
                .unwrap_or_default()
        } else {
            vec![]
        }
    }

    /// The workspaces of the store (and `current`, listed even before it has a session), bounded. The git
    /// scan runs only where a remembered trust covers the folder (never on the client's say).
    pub fn workspaces(&self, current: Option<&Path>) -> Result<(WorkspaceList, bool), String> {
        let r = self.reader()?;
        let home = self.home.to_path_buf();
        let live = move |id: &str| !crate::peers::session_lock_free(&home, id);
        let (records, policy) = (self.records(), trust_policy(self.user_home, self.home));
        let trusted =
            move |root: &str| trust::covers(&records, &root_of(Path::new(root)), &policy).is_some();
        let opts = LoadOpts {
            is_live: Some(&live),
            trusted: Some(&trusted),
            env: Some(self.env),
        };
        let mut list = WorkspaceList::load(&r, current, &opts)
            .map_err(failed("cannot read the session store"))?;
        let cut = list.workspaces.len() > MAX_WORKSPACES
            || list
                .workspaces
                .iter()
                .any(|w| w.sessions.len() > MAX_SESSIONS);
        list.workspaces.truncate(MAX_WORKSPACES);
        for w in &mut list.workspaces {
            w.sessions.truncate(MAX_SESSIONS);
        }
        Ok((list, cut))
    }

    pub fn workspaces_list(&self, current: Option<&Path>) -> Result<Value, String> {
        let (l, truncated) = self.workspaces(current)?;
        Ok(json!({"workspaces": l.workspaces, "truncated": truncated}))
    }

    pub fn sessions_list(&self, key: &str) -> Result<Value, String> {
        check_key(key)?;
        let (l, _) = self.workspaces(None)?;
        let w = l
            .workspaces
            .into_iter()
            .find(|w| w.key == key)
            .ok_or("unknown workspace")?;
        Ok(json!({"sessions": w.sessions}))
    }

    /// The stored conversation of `id`, read-only: its newest events within the bounds.
    pub fn preview(&self, id: &str) -> Result<Value, String> {
        check_id(id)?;
        let r = self.reader()?;
        let rec = r
            .get_session(id)
            .map_err(|_| "unknown session".to_string())?;
        // only the newest events are read, a page at a time, and every limit is in BYTES as well as events: a page
        // is small, an event over the per-event limit is dropped as soon as it is measured, and the scan stops at
        // its own byte budget (the newest events seen so far are kept)
        let mut seq = rec.event_cursor.saturating_sub(MAX_PREVIEW_EVENTS as u64);
        let mut kept: std::collections::VecDeque<(usize, String)> = Default::default();
        let (mut bytes, mut scanned, mut truncated) = (0usize, 0usize, seq > 0);
        'pages: loop {
            let recs = r
                .read_events(id, seq, PREVIEW_PAGE)
                .map_err(failed("cannot read the session"))?;
            let Some(last) = recs.last().map(|r| match r {
                synthesis_events::Record::Known(e) => e.seq,
                synthesis_events::Record::Unknown(e) => e.seq,
            }) else {
                break;
            };
            for rec in recs {
                let synthesis_events::Record::Known(e) = rec else {
                    continue;
                };
                let Ok(text) = serde_json::to_string(&e) else {
                    continue;
                };
                drop(e);
                let n = text.len();
                scanned += n;
                if n > MAX_PREVIEW_EVENT {
                    truncated = true;
                } else {
                    bytes += n;
                    kept.push_back((n, text));
                    while bytes > MAX_PREVIEW_BYTES || kept.len() > MAX_PREVIEW_EVENTS {
                        let (n, _) = kept.pop_front().unwrap_or_default();
                        bytes -= n;
                        truncated = true;
                    }
                }
                if scanned > MAX_PREVIEW_SCAN {
                    truncated = true;
                    break 'pages;
                }
            }
            if last <= seq {
                break;
            }
            seq = last;
        }
        let events: Vec<Value> = kept
            .into_iter()
            .filter_map(|(_, t)| serde_json::from_str(&t).ok())
            .collect();
        Ok(json!({"events": events, "truncated": truncated}))
    }

    pub fn usage(&self, tz_min: i32, now_ms: i64) -> Result<Value, String> {
        if !(-1440..=1440).contains(&tz_min) {
            return Err("not a time zone offset".into());
        }
        let r = self.reader()?;
        Ok(json!({"days": synthesis_view::usage::load_from(&r, now_ms, tz_min)}))
    }

    /// The session `id` belongs to this folder: its stored cwd is `cwd` (compared as real paths). A session of
    /// another folder is never resumed here.
    pub fn check_resumable(&self, id: &str, cwd: &Path) -> Result<(), String> {
        check_id(id)?;
        let rec = self
            .reader()?
            .get_session(id)
            .map_err(|_| "unknown session".to_string())?;
        let real = |p: &Path| synthesis_core::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
        if fold_dir(&real(Path::new(&rec.cwd)).to_string_lossy())
            != fold_dir(&real(cwd).to_string_lossy())
        {
            return Err("that session was started in another folder".into());
        }
        if !Path::new(&rec.cwd).is_dir() {
            return Err("the folder of that session no longer exists".into());
        }
        Ok(())
    }

    /// Hides the session `id` (the caller made sure it is this engine's own, already ended).
    pub fn close_session(&self, id: &str) -> Result<bool, String> {
        check_id(id)?;
        synthesis_session::Store::open(self.home, synthesis_session::StoreOptions::default())
            .and_then(|s| s.close_session(id))
            .map_err(failed("cannot close the session"))
    }

    /// Hides every listed session of the workspace `key` that no other process holds.
    pub fn close_workspace(&self, key: &str) -> Result<Value, String> {
        check_key(key)?;
        let r =
            synthesis_session::Store::open(self.home, synthesis_session::StoreOptions::default())
                .and_then(|s| s.close_workspace(|root| fold_dir(root) == key))
                .map_err(failed("cannot close the workspace"))?;
        Ok(json!({"closed": r.closed, "held": r.held,
            "failed": r.failed.iter().map(|(id, e)| json!([id, sanitize_for_display(e)])).collect::<Vec<_>>()}))
    }

    // ---- trust

    /// What the Trust dialog and `/trust` show, computed here: the client decides nothing from it.
    pub fn trust_state(&self, cwd: &Path, grant: Option<&Grant>, trusted: bool) -> Value {
        let root = root_of(cwd);
        let refusal = trust_refusal(&root, self.user_home, self.home);
        let unfit = trust::fingerprint(&root, cwd).err().map(|e| e.to_string());
        let (remembered, files, since) =
            match remembered_trust(self.home, self.user_home, &root, false) {
                crate::trust::Remembered::No => ("no", vec![], None),
                crate::trust::Remembered::Trusted(_) => ("trusted", vec![], None),
                crate::trust::Remembered::Changed { files, since } => {
                    ("changed", files, Some(since))
                }
            };
        let parent = root
            .parent()
            .filter(|p| p.is_dir() && parent_refusal(p, self.user_home, self.home).is_none())
            .filter(|_| unfit.is_none())
            .map(|p| p.display().to_string());
        let (records, unverified) = match trust::list(self.home) {
            Ok(l) => (l.records, l.unverified),
            Err(_) => (vec![], 0),
        };
        let rows: Vec<Value> = records
            .iter()
            .filter(|r| r.scope != trust::CHILD)
            .take(trust::MAX_ROWS)
            .enumerate()
            .map(|(i, r)| {
                let state = if r.scope == trust::PARENT {
                    "each project is checked at its first start"
                } else if i >= MAX_FINGERPRINTS {
                    // a fingerprint walks the folder: only the first rows are compared
                    "not checked (too many rows)"
                } else if Path::new(&r.root).is_dir() {
                    match trust::fingerprint_root(Path::new(&r.root)) {
                        Ok(f) if f == r.fingerprint => "config unchanged",
                        _ => "CONFIG CHANGED: not honoured until you trust it again",
                    }
                } else {
                    "folder missing"
                };
                json!({"root": r.root, "scope": r.scope, "trusted_at": r.trusted_at.get(..10).unwrap_or(&r.trusted_at), "state": state})
            })
            .collect();
        json!({
            "root": root.display().to_string(), "cwd": cwd.display().to_string(), "trusted": trusted,
            "session_grant": grant.map(|g| g.0.display().to_string()),
            "refusal": refusal,
            "unfit": unfit, "remembered": remembered, "changed_files": files.into_iter().take(MAX_FILES).collect::<Vec<_>>(),
            "since": since, "parent": parent, "enforcing": self.enforcing, "records": rows, "unverified": unverified,
        })
    }

    /// The decision of the person (`mode`): checks the refusal rules, fingerprints with this engine's own
    /// code and, for `remember`/`parent`, writes the MAC-signed rows (the key never leaves this machine).
    /// Returns the grant the restarted session runs under and the rows to put back if that restart fails.
    pub fn grant(&self, cwd: &Path, mode: Mode) -> Result<(Grant, Vec<Undo>), String> {
        let root = root_of(cwd);
        if let Some(why) = trust_refusal(&root, self.user_home, self.home) {
            return Err(why);
        }
        // no grant of any kind without a manifest: without it nothing could detect a change of what was reviewed
        let fp = trust::fingerprint(&root, cwd).map_err(|e| {
            format!(
                "this folder cannot be trusted: change detection is not possible ({})",
                sanitize_for_display(&e.to_string())
            )
        })?;
        if mode == Mode::Session {
            return Ok(((root, Some(fp), true), vec![]));
        }
        let (at, scope) = match mode {
            Mode::Parent => {
                let p = root
                    .parent()
                    .filter(|p| p.is_dir())
                    .ok_or("this folder has no parent that can be trusted")?;
                if let Some(why) = parent_refusal(p, self.user_home, self.home) {
                    return Err(why);
                }
                (p.to_path_buf(), trust::PARENT)
            }
            _ => (root.clone(), trust::FOLDER),
        };
        let policy = trust_policy(self.user_home, self.home);
        let prior = trust::list(self.home)
            .map_err(failed("cannot read the trust store"))?
            .records;
        let was = |p: &Path| prior.iter().find(|r| Path::new(&r.root) == p).cloned();
        let mut undo = vec![(at.clone(), was(&at))];
        let own = if scope == trust::FOLDER {
            fp.as_str()
        } else {
            ""
        };
        let done = trust::add(self.home, &at, scope, own, &policy)
            .map_err(|e| e.to_string())
            .and_then(|()| {
                if scope == trust::PARENT {
                    undo.push((root.clone(), was(&root)));
                    trust::add(self.home, &root, trust::CHILD, &fp, &policy)
                        .map_err(|e| e.to_string())?;
                }
                Ok(())
            });
        if let Err(e) = done {
            let stuck = self.undo(undo);
            let mut msg = failed::<String>("cannot write the trust store")(e);
            if !stuck.is_empty() {
                msg += &format!("; the trust of {} could not be undone", stuck.join(", "));
            }
            return Err(msg);
        }
        Ok(((root, Some(fp), false), undo))
    }

    /// Puts the trust rows back as they were (a trusted restart failed). A row that cannot be restored is revoked
    /// instead (fewer trust, never more); the roots of the rows that could be neither restored nor revoked come back,
    /// for the caller to say.
    pub fn undo(&self, rows: Vec<Undo>) -> Vec<String> {
        let policy = trust_policy(self.user_home, self.home);
        undo_with(
            rows,
            |r| trust::restore(self.home, r, &policy).is_ok(),
            |root| trust::revoke(self.home, root).is_ok(),
        )
    }

    /// Forgets a remembered root. `root` must be one this engine listed (`trust.state`), never any path.
    pub fn revoke(&self, root: &str, grant: Option<&Grant>) -> Result<Value, String> {
        check_key(root)?;
        let listed = trust::list(self.home)
            .map(|l| l.records)
            .unwrap_or_default();
        let held = grant.is_some_and(|g| g.0.to_string_lossy() == root);
        if !held
            && !listed
                .iter()
                .any(|r| r.root == root && r.scope != trust::CHILD)
        {
            return Err("that folder is not remembered".into());
        }
        let was = trust::revoke(self.home, root).map_err(failed("cannot revoke"))?;
        Ok(json!({"revoked": was, "session_grant_ended": held}))
    }
}

/// [`Ctx::undo`] over the two store operations (`true`: it worked).
fn undo_with(
    rows: Vec<Undo>,
    restore: impl Fn(&trust::TrustRecord) -> bool,
    revoke: impl Fn(&str) -> bool,
) -> Vec<String> {
    let mut failed = vec![];
    for (root, prev) in rows {
        let key = root.to_string_lossy();
        let restored = prev.as_ref().is_some_and(&restore);
        if !restored && !revoke(&key) {
            failed.push(sanitize_for_display(&key));
        }
    }
    failed
}

/// A trust row before a grant: `(root, previous row)`.
pub type Undo = (PathBuf, Option<trust::TrustRecord>);

/// The person's choice in the Trust dialog (`deny` and `Not now` change nothing).
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    Session,
    Remember,
    Parent,
    Deny,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup() -> (tempfile::TempDir, PathBuf, PathBuf, PathBuf) {
        let t = tempfile::tempdir().unwrap();
        let base = synthesis_core::canonicalize(t.path()).unwrap();
        let (home, user, proj) = (base.join("synt"), base.join("user"), base.join("user/proj"));
        std::fs::create_dir_all(proj.join(".synt")).unwrap();
        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(proj.join(".synt/settings.json"), "{}").unwrap();
        (t, home, user, proj)
    }

    #[test]
    fn hostile_ids_keys_and_models_are_refused() {
        for bad in ["", "../x", "a/b", "a b", "a\n", &"a".repeat(65)] {
            assert!(check_id(bad).is_err(), "{bad:?}");
        }
        assert!(check_id("0f3c-AB").is_ok());
        for bad in ["", "a\u{0}b", "a\nb", &"k".repeat(MAX_KEY + 1)] {
            assert!(check_key(bad).is_err(), "{bad:?}");
        }
        for bad in [
            "",
            "  ",
            "--sandbox=off",
            "a\nb",
            &"m".repeat(MAX_MODEL + 1),
        ] {
            assert!(check_model(bad).is_err(), "{bad:?}");
        }
        assert!(check_model("openai/gpt-5").is_ok());
        assert!(check_instructions(&"i".repeat(MAX_INSTRUCTIONS + 1)).is_err());
    }

    #[test]
    fn a_remembered_grant_is_signed_listed_changes_with_the_config_and_is_undone() {
        let (_t, home, user, proj) = setup();
        let env = Env::from_pairs::<&str, &str>([]);
        let c = Ctx {
            home: &home,
            user_home: Some(&user),
            env: &env,
            enforcing: true,
        };
        let (grant, undo) = c.grant(&proj, Mode::Remember).unwrap();
        assert_eq!(
            (grant.0.as_path(), grant.2),
            (proj.as_path(), false),
            "remembered: inert"
        );
        let st = c.trust_state(&proj, None, true);
        assert_eq!(st["remembered"], "trusted");
        assert_eq!(st["records"].as_array().unwrap().len(), 1);
        // the trusted configuration moved on: no longer honoured, and the file is named
        std::fs::write(proj.join(".synt/settings.json"), r#"{"hooks":1}"#).unwrap();
        let st = c.trust_state(&proj, None, false);
        assert_eq!(st["remembered"], "changed");
        assert!(
            st["changed_files"].to_string().contains("settings.json"),
            "{st}"
        );
        // a failed restart puts the store back as it was
        c.undo(undo);
        assert_eq!(
            c.trust_state(&proj, None, false)["records"]
                .as_array()
                .unwrap()
                .len(),
            0
        );
    }

    #[test]
    fn the_refusal_rules_apply_and_nothing_session_wide_is_written() {
        let (_t, home, user, proj) = setup();
        let env = Env::from_pairs::<&str, &str>([]);
        let c = Ctx {
            home: &home,
            user_home: Some(&user),
            env: &env,
            enforcing: true,
        };
        // the home folder is refused for every mode
        for m in [Mode::Session, Mode::Remember, Mode::Parent] {
            assert!(c.grant(&user, m).is_err(), "{m:?}");
        }
        let (g, undo) = c.grant(&proj, Mode::Session).unwrap();
        assert!(undo.is_empty() && g.2 && g.1.is_some());
        assert_eq!(
            c.trust_state(&proj, Some(&g), true)["records"]
                .as_array()
                .unwrap()
                .len(),
            0
        );
        // only a root the engine listed (or its own grant) can be revoked; the client names it as the engine
        // reported it (on Windows the canonical form carries a `\\?\` prefix a typed path does not)
        assert!(c.revoke("/etc", None).is_err());
        let own = g.0.to_string_lossy().into_owned();
        assert!(
            c.revoke(&own, None).is_err(),
            "a session grant is not a stored row"
        );
        assert_eq!(
            c.revoke(&own, Some(&g)).unwrap()["session_grant_ended"],
            true
        );
        c.grant(&proj, Mode::Remember).unwrap();
        let st = c.trust_state(&proj, None, false);
        let listed = st["records"][0]["root"].as_str().unwrap().to_string();
        assert_eq!(c.revoke(&listed, None).unwrap()["revoked"], true);
    }

    #[cfg(unix)]
    #[test]
    fn no_grant_of_any_kind_without_a_fingerprint() {
        let (t, home, user, proj) = setup();
        let env = Env::from_pairs::<&str, &str>([]);
        let c = Ctx {
            home: &home,
            user_home: Some(&user),
            env: &env,
            enforcing: true,
        };
        // a configuration file that is a link leaving the project cannot be fingerprinted
        let outside = t.path().join("outside.json");
        std::fs::write(&outside, "{}").unwrap();
        std::fs::remove_file(proj.join(".synt/settings.json")).unwrap();
        std::os::unix::fs::symlink(&outside, proj.join(".synt/settings.json")).unwrap();
        assert!(trust::fingerprint(&proj, &proj).is_err(), "the premise");
        for m in [Mode::Session, Mode::Remember, Mode::Parent] {
            let e = c.grant(&proj, m).unwrap_err();
            assert!(e.contains("change detection"), "{m:?}: {e}");
        }
        assert_eq!(
            c.trust_state(&proj, None, false)["records"]
                .as_array()
                .unwrap()
                .len(),
            0
        );
    }

    #[test]
    fn a_preview_is_bounded_in_bytes_whatever_the_events_weigh() {
        use synthesis_core::{PermissionMode, WorkspaceContext};
        use synthesis_events::{Budget, Event};
        use synthesis_session::{NewSession, Store, StoreOptions};
        let (_t, home, user, proj) = setup();
        let store = Store::open(&home, StoreOptions::default()).unwrap();
        let ws = WorkspaceContext::open("w1", &proj, &proj, &[], proj.join(".art"), "pg").unwrap();
        let mut w = store
            .create_session(NewSession {
                id: Some("s1".into()),
                workspace: &ws,
                model_config_id: "m".into(),
                permission_mode: PermissionMode::AcceptEdits,
                permission_policy_id: "default".into(),
                budget: Budget::default(),
                parent: None,
                title: None,
            })
            .unwrap();
        let mut say = |n: usize| {
            w.append_event(Event::UserMessage {
                text: "x".repeat(n),
                display_text: None,
                images: vec![],
            })
            .unwrap();
        };
        // two events over the per-event limit, then more small ones than the scan budget
        say(MAX_PREVIEW_EVENT + 1000);
        say(MAX_PREVIEW_EVENT + 5000);
        for _ in 0..40 {
            say(300_000);
        }
        let env = Env::from_pairs::<&str, &str>([]);
        let c = Ctx {
            home: &home,
            user_home: Some(&user),
            env: &env,
            enforcing: true,
        };
        let v = c.preview("s1").unwrap();
        let events = v["events"].as_array().unwrap();
        assert_eq!(v["truncated"], true);
        let sizes: Vec<usize> = events.iter().map(|e| e.to_string().len()).collect();
        assert!(
            sizes.iter().sum::<usize>() <= MAX_PREVIEW_BYTES,
            "{}",
            sizes.iter().sum::<usize>()
        );
        assert!(
            sizes.iter().all(|n| *n <= MAX_PREVIEW_EVENT),
            "no event over the per-event limit"
        );
        assert!(!events.is_empty());
    }

    #[test]
    fn a_failed_undo_is_reported_and_a_row_that_cannot_be_restored_is_revoked() {
        let rec = trust::TrustRecord {
            root: "/p".into(),
            scope: trust::FOLDER.into(),
            trusted_at: "2026-01-01T00:00:00Z".into(),
            fingerprint: "f".into(),
        };
        let rows = || {
            vec![
                (PathBuf::from("/p"), Some(rec.clone())),
                (PathBuf::from("/q"), None),
            ]
        };
        // everything works: nothing to say
        assert!(undo_with(rows(), |_| true, |_| true).is_empty());
        // the old row cannot be written back: it is revoked instead (fewer trust), nothing to say
        let revoked = std::cell::RefCell::new(vec![]);
        assert!(
            undo_with(
                rows(),
                |_| false,
                |r| {
                    revoked.borrow_mut().push(r.to_string());
                    true
                }
            )
            .is_empty()
        );
        assert_eq!(*revoked.borrow(), ["/p", "/q"]);
        // neither works: both are named
        assert_eq!(undo_with(rows(), |_| false, |_| false), ["/p", "/q"]);
        // and on a real store the rows really go back
        let (_t, home, user, proj) = setup();
        let env = Env::from_pairs::<&str, &str>([]);
        let c = Ctx {
            home: &home,
            user_home: Some(&user),
            env: &env,
            enforcing: true,
        };
        let (_, undo) = c.grant(&proj, Mode::Remember).unwrap();
        assert!(c.undo(undo).is_empty());
        assert_eq!(
            c.trust_state(&proj, None, false)["records"]
                .as_array()
                .unwrap()
                .len(),
            0
        );
    }

    #[test]
    fn a_session_is_resumable_only_in_its_own_folder() {
        let (_t, home, user, proj) = setup();
        let env = Env::from_pairs::<&str, &str>([]);
        let c = Ctx {
            home: &home,
            user_home: Some(&user),
            env: &env,
            enforcing: true,
        };
        assert!(c.check_resumable("nope", &proj).is_err());
        assert!(c.check_resumable("../x", &proj).is_err());
    }
}
