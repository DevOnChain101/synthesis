//! `/clean`: `Store::clean_plan`, `Store::clean`, `Store::backup_db` (temp home only).

use std::fs;
#[cfg(unix)]
use std::path::Path;
use std::path::PathBuf;

use serde_json::json;
use synthesis_core::{ParentRef, PermissionMode, WorkspaceContext};
use synthesis_events::{Budget, Event};
use synthesis_session::{
    CleanScope, NewMessage, NewSession, Retention, SessionWriter, Store, StoreOptions,
};
use time::OffsetDateTime;

struct Env {
    _tmp: tempfile::TempDir,
    home: PathBuf,
    base: PathBuf,
}

fn env() -> Env {
    let tmp = tempfile::tempdir().unwrap();
    let base = synthesis_core::canonicalize(tmp.path()).unwrap();
    Env {
        home: base.join("home"),
        base,
        _tmp: tmp,
    }
}

impl Env {
    fn store(&self) -> Store {
        Store::open(&self.home, StoreOptions::default()).unwrap()
    }
    fn dir(&self, name: &str) -> PathBuf {
        let d = self.base.join(name);
        fs::create_dir_all(&d).unwrap();
        d
    }
    fn sdir(&self, id: &str) -> PathBuf {
        self.home.join("sessions").join(id)
    }
    /// A session with a message (`used`) or none, in workspace folder `ws`.
    fn make(&self, id: &str, ws: &str, used: bool, parent: Option<&str>) -> SessionWriter {
        let d = self.dir(ws);
        let w = WorkspaceContext::open("w1", &d, &d, &[], d.join(".art"), "pg").unwrap();
        let mut w = self
            .store()
            .create_session(NewSession {
                id: Some(id.into()),
                workspace: &w,
                model_config_id: "m".into(),
                permission_mode: PermissionMode::AcceptEdits,
                permission_policy_id: "default".into(),
                budget: Budget::default(),
                parent: parent.map(|p| ParentRef {
                    session_id: p.into(),
                    tool_call_id: "t".into(),
                }),
                title: None,
            })
            .unwrap();
        if used {
            w.append_message(NewMessage::user(json!("hi"))).unwrap();
            w.append_event(Event::Notice {
                level: "info".into(),
                text: "n".into(),
            })
            .unwrap();
            w.put_artifact("snapshot", "text/plain", b"12345", Retention::Session)
                .unwrap();
        }
        w
    }
    fn ids(&self) -> Vec<String> {
        let mut v: Vec<String> = self
            .store()
            .reader()
            .unwrap()
            .list_sessions(None)
            .unwrap()
            .into_iter()
            .map(|s| s.id)
            .collect();
        v.sort();
        v
    }
}

fn all(ids: &[&str]) -> Vec<String> {
    ids.iter().map(|s| s.to_string()).collect()
}

fn run(
    e: &Env,
    scope: CleanScope,
    keep: &[&str],
    now: OffsetDateTime,
    backup: bool,
) -> synthesis_session::CleanResult {
    let s = e.store();
    let keep = all(keep);
    let plan = s.clean_plan(scope, &keep, now).unwrap();
    s.clean(scope, &keep, now, &plan.ids, backup).unwrap()
}

/// old-used (closed), recent-used, empty (old enough), and one running session.
fn fixture(e: &Env) -> SessionWriter {
    drop(e.make("closed-used", "ws-a", true, None));
    assert!(e.store().close_session("closed-used").unwrap());
    drop(e.make("used", "ws-a", true, None));
    drop(e.make("empty", "ws-b", false, None));
    e.make("running", "ws-c", true, None)
}

#[test]
fn each_scope_deletes_exactly_its_set_and_the_counts_match() {
    let e = env();
    let _running = fixture(&e);
    let now = OffsetDateTime::now_utc();
    let later = now + time::Duration::hours(2);
    let s = e.store();

    // an empty session younger than an hour may still be starting: not in "closed and empty"
    let p = s.clean_plan(CleanScope::ClosedEmpty, &[], now).unwrap();
    assert_eq!(p.ids, ["closed-used"]);
    let p = s.clean_plan(CleanScope::ClosedEmpty, &[], later).unwrap();
    assert_eq!(p.ids, ["closed-used", "empty"]);
    assert_eq!(p.counts.sessions, 2);
    assert_eq!(
        p.counts.workspaces, 1,
        "ws-a keeps `used`; only ws-b disappears"
    );
    assert_eq!(p.counts.skipped_running, 0);

    // inactive: nothing is 30 days old now; everything is in 31 days, but not the running one
    let p = s.clean_plan(CleanScope::Inactive, &[], now).unwrap();
    assert!(p.ids.is_empty());
    let far = now + time::Duration::days(31);
    let p = s.clean_plan(CleanScope::Inactive, &[], far).unwrap();
    assert_eq!(p.ids, ["closed-used", "empty", "used"]);
    assert_eq!(p.counts.skipped_running, 1);

    // all: the running one stays, the current one named in `keep` stays
    let keep = all(&["used"]);
    let p = s.clean_plan(CleanScope::All, &keep, now).unwrap();
    assert_eq!(p.ids, ["closed-used", "empty"]);
    assert_eq!(p.counts.skipped_running, 2);

    // deleting: exactly the planned set goes, with directories
    let r = run(&e, CleanScope::ClosedEmpty, &[], later, false);
    assert_eq!((r.deleted, r.failed.len()), (2, 0));
    assert!(r.freed_bytes >= 5 && r.events >= 1);
    assert_eq!(e.ids(), ["running", "used"]);
    assert!(!e.sdir("closed-used").exists() && !e.sdir("empty").exists());
    assert!(e.sdir("used").exists() && e.sdir("running").exists());
    // idempotent
    let r = run(&e, CleanScope::ClosedEmpty, &[], later, false);
    assert_eq!((r.deleted, r.skipped_running), (0, 0));
    // all, with `used` as the current session: only it and the running one survive
    let r = run(&e, CleanScope::All, &["used"], later, false);
    assert_eq!((r.deleted, r.skipped_running), (0, 2));
    assert_eq!(e.ids(), ["running", "used"]);
}

#[test]
fn all_spares_current_running_and_their_trees_and_deletes_the_rest() {
    let e = env();
    let _running = fixture(&e);
    let _child = e.make("running-child", "ws-c", true, Some("running"));
    drop(e.make("parent", "ws-d", true, None));
    drop(e.make("kid", "ws-d", true, Some("parent")));
    drop(e.make("current", "ws-e", true, None));
    let now = OffsetDateTime::now_utc();
    let r = run(&e, CleanScope::All, &["current"], now, false);
    assert_eq!(r.failed, Vec::<String>::new());
    // children go before their parents
    assert_eq!(r.deleted, 5);
    assert_eq!(e.ids(), ["current", "running", "running-child"]);
    for id in ["closed-used", "used", "empty", "parent", "kid"] {
        assert!(!e.sdir(id).exists(), "{id}");
    }
}

#[test]
fn a_session_with_a_kept_child_is_kept() {
    let e = env();
    drop(e.make("p", "w", true, None));
    drop(e.make("c", "w", true, Some("p")));
    e.store().close_session("p").unwrap(); // only the parent is in scope
    let r = run(
        &e,
        CleanScope::ClosedEmpty,
        &[],
        OffsetDateTime::now_utc(),
        false,
    );
    assert_eq!((r.deleted, r.skipped_other), (0, 1));
    assert_eq!(e.ids(), ["c", "p"]);
}

#[test]
fn only_what_was_shown_is_deleted() {
    let e = env();
    drop(e.make("a", "w", true, None));
    drop(e.make("b", "w", true, None));
    let s = e.store();
    let now = OffsetDateTime::now_utc();
    let r = s
        .clean(CleanScope::All, &[], now, &all(&["a"]), false)
        .unwrap();
    assert_eq!(r.deleted, 1);
    assert_eq!(e.ids(), ["b"]);
}

#[test]
fn links_are_never_followed_and_nothing_outside_the_sessions_dir_goes() {
    let e = env();
    drop(e.make("inner", "w", true, None));
    drop(e.make("linked", "w", true, None));
    let outside = e.dir("outside");
    fs::write(outside.join("precious.txt"), "keep").unwrap();
    #[cfg(unix)]
    {
        // a link inside the session directory
        std::os::unix::fs::symlink(&outside, e.sdir("inner").join("artifacts/dirlink")).unwrap();
        std::os::unix::fs::symlink(
            outside.join("precious.txt"),
            e.sdir("inner").join("filelink"),
        )
        .unwrap();
        // the session directory itself replaced by a link
        fs::rename(e.sdir("linked"), e.base.join("moved")).unwrap();
        std::os::unix::fs::symlink(&outside, e.sdir("linked")).unwrap();
    }
    let r = run(&e, CleanScope::All, &[], OffsetDateTime::now_utc(), false);
    assert!(outside.join("precious.txt").exists());
    assert_eq!(
        fs::read_to_string(outside.join("precious.txt")).unwrap(),
        "keep"
    );
    #[cfg(unix)]
    {
        assert_eq!(r.deleted, 1, "{r:?}");
        assert!(!e.sdir("inner").exists());
        // the session whose directory is a link is not touched at all
        assert_eq!(e.ids(), ["linked"]);
        assert_eq!(r.skipped_other, 1);
        assert!(fs::symlink_metadata(e.sdir("linked")).is_ok());
        assert!(e.base.join("moved").exists());
    }
    #[cfg(not(unix))]
    let _ = r;
}

#[test]
fn the_rest_of_the_home_is_untouched() {
    let e = env();
    drop(e.make("s", "w", true, None));
    let files = [
        "settings.json",
        "auth.json",
        "trust.key",
        "memory/user.md",
        "models-cache.json",
        "logs/x.log",
        "projects/-w/s.jsonl",
    ];
    for f in files {
        let p = e.home.join(f);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(&p, f).unwrap();
    }
    let r = run(&e, CleanScope::All, &[], OffsetDateTime::now_utc(), true);
    assert_eq!(r.deleted, 1);
    for f in files {
        assert_eq!(fs::read_to_string(e.home.join(f)).unwrap(), f);
    }
    assert!(e.home.join("synt.db").exists());
}

#[test]
fn the_backup_is_a_valid_private_database_with_the_rows_and_no_artifacts() {
    let e = env();
    drop(e.make("s1", "w", true, None));
    drop(e.make("s2", "w", true, None));
    let r = run(
        &e,
        CleanScope::All,
        &["s2"],
        OffsetDateTime::now_utc(),
        true,
    );
    assert_eq!(r.deleted, 1);
    let b = r.backup.unwrap();
    assert!(b.starts_with(e.home.join("backups")) && b.ends_with("synt.db"));
    let c = rusqlite::Connection::open_with_flags(&b, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
        .unwrap();
    let n: i64 = c
        .query_row(
            "SELECT count(*) FROM sessions WHERE id IN ('s1','s2')",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(n, 2, "the backup was taken before the delete");
    let m: i64 = c
        .query_row(
            "SELECT count(*) FROM messages WHERE session_id='s1'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(m, 1);
    assert_eq!(
        fs::read_dir(b.parent().unwrap()).unwrap().count(),
        1,
        "only synt.db"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = |p: &Path| fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&b), 0o600);
        assert_eq!(mode(b.parent().unwrap()), 0o700);
    }
}

#[test]
fn nothing_to_delete_makes_no_backup() {
    let e = env();
    drop(e.make("s", "w", true, None));
    let r = run(
        &e,
        CleanScope::ClosedEmpty,
        &[],
        OffsetDateTime::now_utc(),
        true,
    );
    assert_eq!((r.deleted, r.backup), (0, None));
    assert!(!e.home.join("backups").exists());
}

#[test]
fn a_failed_backup_deletes_nothing() {
    let e = env();
    drop(e.make("s", "w", true, None));
    // `backups` is a file: the backup cannot be made
    fs::write(e.home.join("backups"), "x").unwrap();
    let s = e.store();
    let now = OffsetDateTime::now_utc();
    assert!(
        s.clean(CleanScope::All, &[], now, &all(&["s"]), true)
            .is_err()
    );
    assert_eq!(e.ids(), ["s"]);
    assert!(e.sdir("s").exists());
}

#[test]
fn workspaces_count_only_folders_that_lose_their_last_session() {
    let e = env();
    drop(e.make("a1", "ws-a", true, None));
    drop(e.make("a2", "ws-a", true, None));
    drop(e.make("b1", "ws-b", true, None));
    e.store().close_session("a1").unwrap();
    e.store().close_session("b1").unwrap();
    let p = e
        .store()
        .clean_plan(CleanScope::ClosedEmpty, &[], OffsetDateTime::now_utc())
        .unwrap();
    assert_eq!((p.counts.sessions, p.counts.workspaces), (2, 1));
    assert_eq!(p.counts.snapshot_bytes, 10);
    assert!(p.counts.events >= 2 && p.counts.disk_bytes >= 10);
}

#[test]
fn a_running_parent_that_is_not_in_scope_still_protects_its_closed_child() {
    let e = env();
    let _parent = e.make("parent", "w", true, None);
    drop(e.make("child", "w", true, Some("parent")));
    e.store().close_session("child").unwrap();
    let r = run(
        &e,
        CleanScope::ClosedEmpty,
        &[],
        OffsetDateTime::now_utc(),
        false,
    );
    assert_eq!((r.deleted, r.skipped_running), (0, 1));
    assert_eq!(e.ids(), ["child", "parent"]);
}

#[cfg(unix)]
#[test]
fn an_existing_backups_dir_is_tightened_to_0700() {
    use std::os::unix::fs::PermissionsExt;
    let e = env();
    drop(e.make("s", "w", true, None));
    fs::create_dir_all(e.home.join("backups")).unwrap();
    fs::set_permissions(e.home.join("backups"), fs::Permissions::from_mode(0o755)).unwrap();
    let r = run(&e, CleanScope::All, &[], OffsetDateTime::now_utc(), true);
    assert!(r.backup.is_some());
    let mode = fs::metadata(e.home.join("backups"))
        .unwrap()
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(mode, 0o700);
}

#[test]
fn the_scope_counts_messages_and_a_closed_session_with_history_is_deleted() {
    let e = env();
    drop(e.make("c", "w", true, None));
    e.store().close_session("c").unwrap();
    let p = e
        .store()
        .clean_plan(CleanScope::ClosedEmpty, &[], OffsetDateTime::now_utc())
        .unwrap();
    assert_eq!((p.counts.sessions, p.counts.messages), (1, 1));
}

#[test]
fn a_parent_whose_lock_cannot_be_probed_keeps_its_in_scope_child() {
    let e = env();
    drop(e.make("parent", "w", true, None));
    drop(e.make("child", "w", true, Some("parent")));
    e.store().close_session("child").unwrap();
    // the lock path is a directory: not probeable
    fs::remove_file(e.sdir("parent").join("lock")).unwrap();
    fs::create_dir(e.sdir("parent").join("lock")).unwrap();
    let r = run(
        &e,
        CleanScope::ClosedEmpty,
        &[],
        OffsetDateTime::now_utc(),
        false,
    );
    assert_eq!(r.deleted, 0, "{r:?}");
    assert_eq!(e.ids(), ["child", "parent"]);
}
