//! Several threads open a fresh home and create sessions while a reader loops: none may fail
//! with "database is locked", and the usage reader never writes.

use std::fs;
use std::sync::atomic::{AtomicBool, Ordering};

use synthesis_core::{PermissionMode, WorkspaceContext};
use synthesis_events::Budget;
use synthesis_session::{NewSession, Reader, Store, StoreOptions};

#[test]
fn concurrent_opens_and_creates_with_a_reading_loop() {
    let tmp = tempfile::tempdir().unwrap();
    let base = synthesis_core::canonicalize(tmp.path()).unwrap();
    let home = base.join("home");
    let stop = AtomicBool::new(false);
    std::thread::scope(|s| {
        let reader = s.spawn(|| {
            while !stop.load(Ordering::Relaxed) {
                // may fail before the database exists or while it is busy; must never panic
                if let Ok(r) = Reader::open(&home) {
                    let _ = r.usage_by_day(0, 0);
                }
            }
        });
        let workers: Vec<_> = (0..8)
            .map(|i| {
                let (home, base) = (home.clone(), base.clone());
                s.spawn(move || {
                    let ws =
                        WorkspaceContext::open("w1", &base, &base, &[], base.join(".art"), "pg")
                            .unwrap();
                    let store = Store::open(&home, StoreOptions::default()).unwrap();
                    store
                        .create_session(NewSession {
                            id: Some(format!("s{i}")),
                            workspace: &ws,
                            model_config_id: "m".into(),
                            permission_mode: PermissionMode::Default,
                            permission_policy_id: "default".into(),
                            budget: Budget::default(),
                            parent: None,
                            title: None,
                        })
                        .unwrap();
                })
            })
            .collect();
        for w in workers {
            w.join().unwrap();
        }
        stop.store(true, Ordering::Relaxed);
        reader.join().unwrap();
    });
}

#[test]
fn usage_reader_never_writes() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    drop(Store::open(&home, StoreOptions::default()).unwrap());
    let db = home.join("synt.db");
    let before = fs::read(&db).unwrap();
    let r = Reader::open(&home).unwrap();
    r.usage_by_day(0, 0).unwrap();
    drop(r);
    assert_eq!(before, fs::read(&db).unwrap());
    // a read-only connection refuses a write
    let c = rusqlite::Connection::open_with_flags(&db, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
        .unwrap();
    assert!(c.execute("DELETE FROM sessions", []).is_err());
}

/// Another process holds the write lock past the 5 s busy timeout: the append waits and goes
/// through instead of breaking the run ("event sink failed").
#[test]
fn append_outlasts_a_write_lock_held_longer_than_the_busy_timeout() {
    let tmp = tempfile::tempdir().unwrap();
    let base = synthesis_core::canonicalize(tmp.path()).unwrap();
    let home = base.join("home");
    let ws = WorkspaceContext::open("w1", &base, &base, &[], base.join(".art"), "pg").unwrap();
    let store = Store::open(&home, StoreOptions::default()).unwrap();
    let mut w = store
        .create_session(NewSession {
            id: Some("s1".into()),
            workspace: &ws,
            model_config_id: "m".into(),
            permission_mode: PermissionMode::Default,
            permission_policy_id: "default".into(),
            budget: Budget::default(),
            parent: None,
            title: None,
        })
        .unwrap();
    let c = rusqlite::Connection::open(home.join("synt.db")).unwrap();
    c.execute_batch("BEGIN IMMEDIATE").unwrap();
    let holder = std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_secs(6));
        c.execute_batch("COMMIT").unwrap();
    });
    w.append_event(synthesis_events::Event::Notice {
        level: "info".into(),
        text: "x".into(),
    })
    .unwrap();
    holder.join().unwrap();
}
