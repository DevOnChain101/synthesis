//! Workspaces known to the session store and their sessions, as the Workspace and Running panels
//! list them: empty sessions and closed ones are hidden (`Reader::list_used_sessions`).

use std::path::Path;

use serde::Serialize;
use synthesis_config::Env;
use synthesis_core::SessionStatus;
use synthesis_session::{Reader, SessionRecord};

use crate::model::parse_ts;
use crate::sessions::first_prompt;
use crate::text::basename;

/// Only the newest sessions get a title read from their first prompt (a read per session).
const TITLED: usize = 100;

/// Canonical, case-folded where the file system folds case (macOS, Windows): the key of a folder.
pub fn fold_dir(p: &str) -> String {
    let c = std::fs::canonicalize(p)
        .map_or_else(|_| p.to_string(), |p| p.to_string_lossy().into_owned());
    if cfg!(any(windows, target_os = "macos")) {
        c.to_lowercase()
    } else {
        c
    }
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct SessionRow {
    pub id: String,
    /// The stored title, else the first prompt, else `(untitled)`.
    pub title: String,
    pub status: SessionStatus,
    pub updated_at: String,
    /// Epoch ms of `updated_at` (0 when unknown).
    pub mtime: i64,
    /// A process holds the session now (decided by the host, see [`LoadOpts::is_live`]).
    pub is_live: bool,
}

/// Uncommitted changes of a trusted folder: `git diff --numstat` summed.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub struct Changes {
    pub added: u64,
    pub deleted: u64,
    pub files: usize,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct WorkspaceRow {
    /// The folder as stored (what `/new` opens).
    pub root: String,
    /// What makes two paths the same workspace.
    pub key: String,
    pub label: String,
    /// Sessions the panel lists (not empty, not closed), newest first.
    pub sessions: Vec<SessionRow>,
    /// The folder exists on this machine.
    pub available: bool,
    /// `None` unless the folder is trusted, available and a git repository with a readable diff.
    pub changes: Option<Changes>,
}

impl WorkspaceRow {
    pub fn session_count(&self) -> usize {
        self.sessions.len()
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct WorkspaceList {
    /// Newest session first; a workspace without sessions (the current one) comes last.
    pub workspaces: Vec<WorkspaceRow>,
}

/// What the host decides; the defaults are the safe answers (nothing live, nothing trusted).
#[derive(Clone, Copy, Default)]
pub struct LoadOpts<'a> {
    pub is_live: Option<&'a dyn Fn(&str) -> bool>,
    /// The trust gate of the git scan: `--trust-project` for this folder. A repository's config
    /// can name programs, so an untrusted folder is never scanned.
    pub trusted: Option<&'a dyn Fn(&str) -> bool>,
    /// For the scan only (the child's PATH); without it nothing is scanned.
    pub env: Option<&'a Env>,
}

impl LoadOpts<'_> {
    fn live(&self, id: &str) -> bool {
        self.is_live.is_some_and(|f| f(id))
    }
}

fn row(rec: &SessionRecord, title: String, live: bool) -> SessionRow {
    SessionRow {
        id: rec.id.clone(),
        title: match title.trim() {
            "" => "(untitled)".into(),
            t => t.to_string(),
        },
        status: rec.status,
        updated_at: rec.updated_at.clone(),
        mtime: parse_ts(&rec.updated_at).unwrap_or(0),
        is_live: live,
    }
}

fn changes(root: &str, opts: &LoadOpts) -> Option<Changes> {
    let (trusted, env) = (opts.trusted?, opts.env?);
    if !trusted(root) || !Path::new(root).is_dir() {
        return None;
    }
    let p = crate::project::scan(Path::new(root), env);
    if p.note.is_some() {
        return None;
    }
    let n = |s: &str| s.parse::<u64>().unwrap_or(0); // `-` for a binary file
    Some(Changes {
        added: p.stats.iter().map(|s| n(&s.0)).sum(),
        deleted: p.stats.iter().map(|s| n(&s.1)).sum(),
        files: p.stats.len(),
    })
}

impl WorkspaceList {
    /// A renamed session shows its new title without reading the store again.
    pub fn set_title(&mut self, id: &str, title: &str) {
        for s in self.workspaces.iter_mut().flat_map(|w| &mut w.sessions) {
            if s.id == id {
                s.title = title.into();
            }
        }
    }

    /// `current`: the folder of the running session, listed even before it has a session.
    pub fn load(r: &Reader, current: Option<&Path>, opts: &LoadOpts) -> Result<Self, String> {
        let recs = r.list_used_sessions(None).map_err(|e| e.to_string())?;
        let mut ws: Vec<WorkspaceRow> = vec![];
        for (i, rec) in recs.iter().enumerate() {
            let title = match &rec.title {
                Some(t) if !t.trim().is_empty() => t.clone(),
                _ if i < TITLED => first_prompt(r, &rec.id).unwrap_or_default(),
                _ => String::new(),
            };
            let s = row(rec, title, opts.live(&rec.id));
            let key = fold_dir(&rec.workspace_root);
            match ws.iter_mut().find(|w| w.key == key) {
                Some(w) => w.sessions.push(s),
                None => ws.push(blank(&rec.workspace_root, key, vec![s])),
            }
        }
        if let Some(c) = current {
            let raw = c.to_string_lossy();
            let key = fold_dir(&raw);
            if !ws.iter().any(|w| w.key == key) {
                ws.push(blank(&raw, key, vec![]));
            }
        }
        // A session keeps its place: creation time, never `updated_at` (selecting or running one must not move rows).
        let created = |id: &str| {
            recs.iter()
                .find(|r| r.id == id)
                .map(|r| parse_ts(&r.created_at))
        };
        for w in &mut ws {
            w.sessions
                .sort_by_cached_key(|s| (std::cmp::Reverse(created(&s.id)), s.id.clone()));
            w.changes = changes(&w.root, opts);
        }
        Ok(Self { workspaces: ws })
    }

    pub fn find(&self, root: &str) -> Option<&WorkspaceRow> {
        let key = fold_dir(root);
        self.workspaces.iter().find(|w| w.key == key)
    }
}

fn blank(root: &str, key: String, sessions: Vec<SessionRow>) -> WorkspaceRow {
    WorkspaceRow {
        root: root.to_string(),
        key,
        label: basename(root),
        sessions,
        available: Path::new(root).is_dir(),
        changes: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use synthesis_core::{PermissionMode, WorkspaceContext};
    use synthesis_events::{Budget, Event};
    use synthesis_session::{NewMessage, NewSession, Store, StoreOptions};

    fn session(store: &Store, id: &str, dir: &Path, say: Option<&str>) {
        let ws = WorkspaceContext::open("w1", dir, dir, &[], dir.join(".art"), "pg").unwrap();
        let mut w = store
            .create_session(NewSession {
                id: Some(id.into()),
                workspace: &ws,
                model_config_id: "m".into(),
                permission_mode: PermissionMode::AcceptEdits,
                permission_policy_id: "default".into(),
                budget: Budget::default(),
                parent: None,
                title: None,
            })
            .unwrap();
        if let Some(t) = say {
            w.append_message(NewMessage::user(json!(t))).unwrap();
            w.append_event(Event::UserMessage {
                text: t.into(),
                display_text: None,
                images: vec![],
            })
            .unwrap();
        }
    }

    /// `a` has a used session and an empty one; `b` only a closed one; `c` is the current folder.
    #[test]
    fn hides_empty_and_closed_and_groups_by_folder() {
        let t = tempfile::tempdir().unwrap();
        let base = synthesis_core::canonicalize(t.path()).unwrap();
        let dir = |n: &str| {
            let d = base.join(n);
            std::fs::create_dir_all(&d).unwrap();
            d
        };
        let (a, b, c) = (dir("a"), dir("b"), dir("c"));
        let home = base.join("home");
        let store = Store::open(&home, StoreOptions::default()).unwrap();
        session(&store, "s-used", &a, Some("fix the build"));
        session(&store, "s-empty", &a, None);
        session(&store, "s-closed", &b, Some("old"));
        assert!(store.close_session("s-closed").unwrap());

        let live = |id: &str| id == "s-used";
        let opts = LoadOpts {
            is_live: Some(&live),
            ..LoadOpts::default()
        };
        let list = WorkspaceList::load(&store.reader().unwrap(), Some(&c), &opts).unwrap();

        let roots: Vec<_> = list.workspaces.iter().map(|w| w.label.as_str()).collect();
        assert_eq!(
            roots,
            ["a", "c"],
            "b has only a closed session; c is current"
        );
        let wa = list.find(a.to_str().unwrap()).unwrap();
        assert_eq!(wa.session_count(), 1);
        assert_eq!(
            (wa.sessions[0].id.as_str(), wa.sessions[0].is_live),
            ("s-used", true)
        );
        assert_eq!(
            wa.sessions[0].title, "fix the build",
            "first prompt names it"
        );
        assert!(wa.available && wa.changes.is_none(), "no trust, no scan");
        assert_eq!(list.find(c.to_str().unwrap()).unwrap().session_count(), 0);
    }

    #[test]
    fn sessions_keep_their_creation_order_when_one_is_touched() {
        let t = tempfile::tempdir().unwrap();
        let a = synthesis_core::canonicalize(t.path()).unwrap();
        let store = Store::open(&a.join("home"), StoreOptions::default()).unwrap();
        let ids = |store: &Store| -> Vec<String> {
            let l = WorkspaceList::load(&store.reader().unwrap(), None, &LoadOpts::default());
            l.unwrap().workspaces[0]
                .sessions
                .iter()
                .map(|s| s.id.clone())
                .collect()
        };
        let pause = || std::thread::sleep(std::time::Duration::from_millis(5));
        for id in ["s1", "s2", "s3"] {
            session(&store, id, &a, Some("hi"));
            pause();
        }
        assert_eq!(ids(&store), ["s3", "s2", "s1"], "newest created first");
        pause();
        // activity on the oldest session moves its updated_at, not its place
        let mut w = store.resume_session("s1", true).unwrap();
        w.set_title("renamed").unwrap();
        assert_eq!(ids(&store), ["s3", "s2", "s1"], "activity does not reorder");
        session(&store, "s4", &a, Some("new"));
        assert_eq!(
            ids(&store),
            ["s4", "s3", "s2", "s1"],
            "a new one joins at the top"
        );
    }

    #[test]
    fn fold_dir_is_stable_for_a_missing_path() {
        assert_eq!(fold_dir("/no/such/dir/x"), fold_dir("/no/such/dir/x"));
    }
}
