//! The session and workspace that were open when the app closed (`<synt home>/desktop-state.json`), so the
//! next launch starts there and not in `$HOME`. One entry per scope ("local"; a WSL distro would be
//! `wsl:<distro>`). Written atomically after every session start; a damaged file is ignored, never fatal.

use std::path::{Path, PathBuf};

const FILE: &str = "desktop-state.json";
pub const LOCAL: &str = "local";

/// The scope of an engine: local, or `wsl:<distro>` (the distro name is only a JSON key, never a path);
/// `None`: a remote engine that is not a WSL distro.
pub fn scope(remote: bool, distro: Option<&str>) -> Option<String> {
    match (remote, distro) {
        (false, _) => Some(LOCAL.into()),
        (true, Some(d)) if !d.is_empty() => Some(format!("wsl:{d}")),
        _ => None,
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Last {
    pub sid: String,
    pub root: String,
}

pub fn load(home: &Path, scope: &str) -> Option<Last> {
    let doc: serde_json::Value =
        serde_json::from_slice(&std::fs::read(home.join(FILE)).ok()?).ok()?;
    let e = doc.get("scopes")?.get(scope)?;
    let (sid, root) = (e.get("sid")?.as_str()?, e.get("root")?.as_str()?);
    (!sid.is_empty() && !root.is_empty()).then(|| Last {
        sid: sid.into(),
        root: root.into(),
    })
}

/// Records `last` for `scope`, keeping the other scopes' entries.
pub fn save(home: &Path, scope: &str, last: &Last) -> std::io::Result<()> {
    let mut doc: serde_json::Value = std::fs::read(home.join(FILE))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .filter(serde_json::Value::is_object)
        .unwrap_or_else(|| serde_json::json!({}));
    doc["version"] = 1.into();
    let mut e = entry(&doc, scope);
    e["sid"] = last.sid.clone().into();
    e["root"] = last.root.clone().into();
    doc["scopes"][scope] = e;
    write(home, &doc)
}

/// The scope's entry as an object (what a damaged file holds there is dropped).
fn entry(doc: &serde_json::Value, scope: &str) -> serde_json::Value {
    doc.get("scopes")
        .and_then(|s| s.get(scope))
        .filter(|e| e.is_object())
        .cloned()
        .unwrap_or_else(|| serde_json::json!({}))
}

fn write(home: &Path, doc: &serde_json::Value) -> std::io::Result<()> {
    std::fs::create_dir_all(home)?;
    synthesis_config::write_private_atomic(
        &home.join(FILE),
        (serde_json::to_string_pretty(doc)? + "\n").as_bytes(),
    )
}

/// The last active session of each workspace (workspace key -> session id) of `scope`; damaged: empty.
pub fn load_active(home: &Path, scope: &str) -> std::collections::HashMap<String, String> {
    let doc: Option<serde_json::Value> = std::fs::read(home.join(FILE))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok());
    doc.as_ref()
        .and_then(|d| d.get("scopes")?.get(scope)?.get("active")?.as_object())
        .map(|m| {
            m.iter()
                .filter_map(|(k, v)| Some((k.clone(), v.as_str()?.to_string())))
                .collect()
        })
        .unwrap_or_default()
}

/// Records the last active session of each workspace for `scope`, keeping everything else.
pub fn save_active(
    home: &Path,
    scope: &str,
    active: &std::collections::HashMap<String, String>,
) -> std::io::Result<()> {
    let mut doc: serde_json::Value = std::fs::read(home.join(FILE))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .filter(serde_json::Value::is_object)
        .unwrap_or_else(|| serde_json::json!({}));
    doc["version"] = 1.into();
    let mut e = entry(&doc, scope);
    e["active"] = serde_json::to_value(active)?;
    doc["scopes"][scope] = e;
    write(home, &doc)
}

/// The remembered display order of one scope: workspace keys, and per workspace key its session ids.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Order {
    pub workspaces: Vec<String>,
    pub sessions: std::collections::BTreeMap<String, Vec<String>>,
}

/// `scopes.<scope>.order` (`{"workspaces": [key], "sessions": {key: [id]}}`); missing or damaged: empty.
pub fn load_order(home: &Path, scope: &str) -> Order {
    let doc: Option<serde_json::Value> = std::fs::read(home.join(FILE))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok());
    let strs = |v: Option<&serde_json::Value>| -> Vec<String> {
        v.and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|x| x.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default()
    };
    let Some(o) = doc
        .as_ref()
        .and_then(|d| d.get("scopes")?.get(scope)?.get("order"))
    else {
        return Order::default();
    };
    Order {
        workspaces: strs(o.get("workspaces")),
        sessions: o
            .get("sessions")
            .and_then(|m| m.as_object())
            .map(|m| m.iter().map(|(k, v)| (k.clone(), strs(Some(v)))).collect())
            .unwrap_or_default(),
    }
}

fn save_order(home: &Path, scope: &str, order: &Order) -> std::io::Result<()> {
    let mut doc: serde_json::Value = std::fs::read(home.join(FILE))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .filter(serde_json::Value::is_object)
        .unwrap_or_else(|| serde_json::json!({}));
    doc["version"] = 1.into();
    let mut e = entry(&doc, scope);
    e["order"] = serde_json::json!({"workspaces": order.workspaces, "sessions": order.sessions});
    doc["scopes"][scope] = e;
    write(home, &doc)
}

/// Sorts `items` by their place in `old`; an item not there yet keeps the computed place before the known ones
/// (`last`: after them), so a new folder or session joins where the load rule puts it and nothing known moves.
fn place<T>(items: &mut [T], old: &[String], key: impl Fn(&T) -> &str, last: impl Fn(&T) -> bool) {
    items.sort_by_key(|t| match old.iter().position(|k| k == key(t)) {
        Some(i) => i + 1,
        None if last(t) => usize::MAX,
        None => 0,
    });
}

/// Puts the loaded list into the remembered order, then remembers the result (entries of deleted folders and
/// sessions drop out). Best effort: an unreadable or unwritable file leaves the computed order.
pub fn settle(home: &Path, scope: &str, ws: &mut synthesis_view::workspaces::WorkspaceList) {
    let old = load_order(home, scope);
    place(
        &mut ws.workspaces,
        &old.workspaces,
        |w| &w.key,
        |w| w.sessions.is_empty(),
    );
    for w in &mut ws.workspaces {
        let ids = old.sessions.get(&w.key).map_or(&[][..], Vec::as_slice);
        place(&mut w.sessions, ids, |s| &s.id, |_| false);
    }
    let now = Order {
        workspaces: ws.workspaces.iter().map(|w| w.key.clone()).collect(),
        sessions: ws
            .workspaces
            .iter()
            .filter(|w| !w.sessions.is_empty())
            .map(|w| {
                (
                    w.key.clone(),
                    w.sessions.iter().map(|s| s.id.clone()).collect(),
                )
            })
            .collect(),
    };
    if now != old {
        let _ = save_order(home, scope, &now);
    }
}

/// Where a launch without flags starts.
#[derive(Debug, PartialEq, Eq)]
pub enum Start {
    /// Continue this stored session (its folder is the workspace).
    Resume(String),
    /// A fresh session in this folder (the session is gone, the workspace is not).
    Folder(PathBuf),
}

/// `saved`: stored sessions, newest first, as (id, folder). `resumable(id, folder)`: the session can be
/// continued (its folder exists and it loads); `is_dir`: the folder exists. The last session if it still can be
/// continued, else a fresh one in its workspace, else the newest session that can be; `None` means nothing
/// is left to restore (the caller says so; it does not pick `$HOME` silently).
pub fn resolve(
    last: Option<&Last>,
    saved: &[(String, String)],
    resumable: impl Fn(&str, &str) -> bool,
    is_dir: impl Fn(&str) -> bool,
) -> Option<Start> {
    if let Some(l) = last {
        if saved
            .iter()
            .any(|(id, cwd)| *id == l.sid && resumable(id, cwd))
        {
            return Some(Start::Resume(l.sid.clone()));
        }
        if is_dir(&l.root) {
            return Some(Start::Folder(PathBuf::from(&l.root)));
        }
    }
    saved
        .iter()
        .find(|(id, cwd)| resumable(id, cwd))
        .map(|(id, _)| Start::Resume(id.clone()))
}

/// The launch target from the store: (folder, session to continue). `None`: no flags apply and nothing is
/// left to restore, or the store cannot be read.
pub fn from_store(home: &Path) -> Option<(PathBuf, Option<String>)> {
    let last = load(home, LOCAL);
    let saved: Vec<(String, String)> = synthesis_view::sessions::list_saved(home, None)
        .ok()?
        .into_iter()
        .map(|s| (s.id, s.cwd))
        .collect();
    let start = resolve(
        last.as_ref(),
        &saved,
        |id, cwd| Path::new(cwd).is_dir() && crate::engine::load_resume_in_place(home, id).is_ok(),
        |d| Path::new(d).is_dir(),
    )?;
    match start {
        Start::Folder(p) => Some((p, None)),
        Start::Resume(id) => {
            let cwd = saved.iter().find(|(i, _)| *i == id)?.1.clone();
            Some((PathBuf::from(cwd), Some(id)))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(id: &str, cwd: &str) -> (String, String) {
        (id.into(), cwd.into())
    }

    fn last() -> Last {
        Last {
            sid: "b".into(),
            root: "/w/b".into(),
        }
    }

    #[test]
    fn the_last_session_comes_back_even_when_it_is_not_the_newest() {
        let saved = [s("a", "/w/a"), s("b", "/w/b")];
        let r = resolve(Some(&last()), &saved, |_, _| true, |_| true);
        assert_eq!(r, Some(Start::Resume("b".into())));
    }

    #[test]
    fn a_deleted_session_opens_a_fresh_one_in_its_workspace() {
        let saved = [s("a", "/w/a")];
        let r = resolve(Some(&last()), &saved, |_, _| true, |d| d == "/w/b");
        assert_eq!(r, Some(Start::Folder("/w/b".into())));
    }

    #[test]
    fn a_missing_workspace_falls_back_to_the_newest_one_that_exists_never_home() {
        let saved = [s("a", "/w/gone"), s("c", "/w/c"), s("b", "/w/b")];
        let r = resolve(Some(&last()), &saved, |_, cwd| cwd == "/w/c", |_| false);
        assert_eq!(r, Some(Start::Resume("c".into())));
        assert_eq!(
            resolve(Some(&last()), &saved, |_, _| false, |_| false),
            None
        );
        assert_eq!(resolve(None, &[], |_, _| true, |_| true), None);
    }

    #[test]
    fn the_active_sessions_survive_a_session_save_and_back() {
        let d = tempfile::tempdir().unwrap();
        let m: std::collections::HashMap<_, _> = [("w".to_string(), "s1".to_string())].into();
        save_active(d.path(), LOCAL, &m).unwrap();
        save(d.path(), LOCAL, &last()).unwrap();
        assert_eq!(load_active(d.path(), LOCAL), m);
        assert_eq!(load(d.path(), LOCAL), Some(last()));
        assert!(load_active(d.path(), "wsl:x").is_empty());
        // a WSL scope persists like the local one, and the two never mix
        let (w, sc) = (
            std::collections::HashMap::from([("k".to_string(), "s9".to_string())]),
            scope(true, Some("Ubuntu")).unwrap(),
        );
        assert_eq!(sc, "wsl:Ubuntu");
        save_active(d.path(), &sc, &w).unwrap();
        assert_eq!(load_active(d.path(), &sc), w);
        assert_eq!(load_active(d.path(), LOCAL), m);
        assert_eq!(scope(false, None).as_deref(), Some(LOCAL));
        assert_eq!(scope(true, None), None);
    }

    #[test]
    fn the_display_order_survives_a_reload_and_a_new_item_joins_without_moving_the_rest() {
        use synthesis_core::SessionStatus::Idle;
        use synthesis_view::workspaces::{SessionRow, WorkspaceList, WorkspaceRow};
        let sess = |id: &str| SessionRow {
            id: id.into(),
            title: id.into(),
            status: Idle,
            updated_at: String::new(),
            mtime: 0,
            is_live: false,
        };
        let w = |k: &str, ids: &[&str]| WorkspaceRow {
            root: format!("/{k}"),
            key: k.into(),
            label: k.into(),
            sessions: ids.iter().map(|i| sess(i)).collect(),
            available: true,
            changes: None,
        };
        let list = |v: Vec<WorkspaceRow>| WorkspaceList { workspaces: v };
        let shape = |l: &WorkspaceList| -> Vec<(String, Vec<String>)> {
            l.workspaces
                .iter()
                .map(|w| {
                    (
                        w.key.clone(),
                        w.sessions.iter().map(|s| s.id.clone()).collect(),
                    )
                })
                .collect()
        };
        let d = tempfile::tempdir().unwrap();
        let mut first = list(vec![w("a", &["a2", "a1"]), w("b", &["b1"])]);
        settle(d.path(), LOCAL, &mut first);
        // a later load computes another order (activity moved things); the remembered one wins
        let mut again = list(vec![w("b", &["b1"]), w("a", &["a1", "a2"])]);
        settle(d.path(), LOCAL, &mut again);
        assert_eq!(shape(&again), shape(&first));
        // new folder and session join before the known ones; a deleted folder drops out of the file
        let mut more = list(vec![
            w("a", &["a1", "a3", "a2"]),
            w("c", &["c1"]),
            w("b", &["b1"]),
        ]);
        settle(d.path(), LOCAL, &mut more);
        let keys: Vec<_> = more.workspaces.iter().map(|w| w.key.as_str()).collect();
        assert_eq!(keys, ["c", "a", "b"]);
        assert_eq!(more.workspaces[1].sessions[0].id, "a3");
        let mut gone = list(vec![w("c", &["c1"])]);
        settle(d.path(), LOCAL, &mut gone);
        assert_eq!(load_order(d.path(), LOCAL).workspaces, ["c"]);
        // a damaged file is ignored; other scope keys survive
        save_active(
            d.path(),
            LOCAL,
            &[("k".to_string(), "s".to_string())].into(),
        )
        .unwrap();
        assert_eq!(load_active(d.path(), LOCAL).len(), 1);
        std::fs::write(d.path().join(FILE), b"{oops").unwrap();
        let mut x = list(vec![w("b", &[]), w("a", &[])]);
        settle(d.path(), LOCAL, &mut x);
        assert_eq!(x.workspaces[0].key, "b");
    }

    #[test]
    fn the_state_file_round_trips_per_scope_and_a_damaged_one_is_ignored() {
        let d = tempfile::tempdir().unwrap();
        assert_eq!(load(d.path(), LOCAL), None);
        save(d.path(), LOCAL, &last()).unwrap();
        let other = Last {
            sid: "x".into(),
            root: "/home/u".into(),
        };
        save(d.path(), "wsl:Ubuntu", &other).unwrap();
        assert_eq!(load(d.path(), LOCAL), Some(last()));
        assert_eq!(load(d.path(), "wsl:Ubuntu"), Some(other));
        assert_eq!(load(d.path(), "wsl:Debian"), None);
        std::fs::write(d.path().join(FILE), "{ not json").unwrap();
        assert_eq!(load(d.path(), LOCAL), None);
        save(d.path(), LOCAL, &last()).unwrap();
        assert_eq!(load(d.path(), LOCAL), Some(last()));
    }
}
