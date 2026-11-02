//! A writer process is killed mid-run: the DB stays intact and seq stays gapless.

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

use synthesis_core::SessionStatus;
use synthesis_events::{Event, Record};
use synthesis_session::{Error, OrphanAction, Store, StoreOptions, recover};

fn helper() -> PathBuf {
    // target/<profile>/deps/<test> -> target/<profile>/examples/crash_writer
    let mut p = std::env::current_exe().unwrap();
    p.pop();
    p.pop();
    p.push("examples");
    p.push(format!("crash_writer{}", std::env::consts::EXE_SUFFIX));
    assert!(p.exists(), "build the example first: {}", p.display());
    p
}

fn spawn(
    home: &Path,
    ws: &Path,
    id: &str,
    mode: &str,
) -> (Child, BufReader<std::process::ChildStdout>) {
    let mut c = Command::new(helper())
        .args([home.to_str().unwrap(), ws.to_str().unwrap(), id, mode])
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let r = BufReader::new(c.stdout.take().unwrap());
    (c, r)
}

fn integrity(home: &Path) {
    let conn = rusqlite::Connection::open(home.join("synt.db")).unwrap();
    let r: String = conn
        .query_row("PRAGMA integrity_check", [], |r| r.get(0))
        .unwrap();
    assert_eq!(r, "ok");
    let bad: i64 = conn
        .query_row("SELECT COUNT(*) FROM pragma_foreign_key_check", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(bad, 0);
}

fn assert_gapless(store: &Store, id: &str) -> u64 {
    let rec = store.reader().unwrap().get_session(id).unwrap();
    let evs = store
        .reader()
        .unwrap()
        .read_events(id, 0, usize::MAX >> 1)
        .unwrap();
    let seqs: Vec<u64> = evs
        .iter()
        .map(|r| match r {
            Record::Known(e) => e.seq,
            Record::Unknown(e) => e.seq,
        })
        .collect();
    assert_eq!(
        seqs,
        (1..=seqs.len() as u64).collect::<Vec<_>>(),
        "gapless from 1"
    );
    assert_eq!(
        rec.event_cursor,
        seqs.len() as u64,
        "event_cursor == last durable seq"
    );
    rec.event_cursor
}

#[test]
fn kill_writer_between_commits() {
    let dir = tempfile::tempdir().unwrap();
    let (home, ws) = (dir.path().join("home"), dir.path().join("ws"));
    std::fs::create_dir_all(&ws).unwrap();
    let (mut child, mut out) = spawn(&home, &ws, "crash-loop", "loop");
    let mut last = 0u64;
    let mut line = String::new();
    while last < 40 {
        line.clear();
        assert!(out.read_line(&mut line).unwrap() > 0, "helper exited early");
        last = line.trim().strip_prefix("seq ").unwrap().parse().unwrap();
    }
    // While the helper is alive: its writer lock is respected, recovery leaves it alone.
    let store = Store::open(&home, StoreOptions::default()).unwrap();
    assert!(matches!(
        store.resume_session("crash-loop", true),
        Err(Error::Locked(_))
    ));
    let rep = recover(&home, OrphanAction::Report).unwrap();
    assert_eq!(rep.skipped_live, ["crash-loop"]);
    assert!(rep.interrupted.is_empty());
    child.kill().unwrap();
    child.wait().unwrap();

    integrity(&home);
    let cursor = assert_gapless(&store, "crash-loop");
    assert!(
        cursor >= last,
        "every printed (committed) seq is durable: {cursor} >= {last}"
    );

    let rep = recover(&home, OrphanAction::Report).unwrap();
    assert_eq!(rep.interrupted, ["crash-loop"]);
    assert_eq!(
        recover(&home, OrphanAction::Report).unwrap(),
        Default::default(),
        "idempotent"
    );
    let mut w = store.resume_session("crash-loop", true).unwrap();
    assert_eq!(
        store
            .reader()
            .unwrap()
            .get_session("crash-loop")
            .unwrap()
            .status,
        SessionStatus::Interrupted
    );
    let env = w
        .append_event(Event::Notice {
            level: "info".into(),
            text: "after".into(),
        })
        .unwrap();
    assert_eq!(
        env.seq,
        cursor + 2,
        "continues right after the last durable event"
    );
}

#[test]
fn kill_writer_mid_transaction() {
    let dir = tempfile::tempdir().unwrap();
    let (home, ws) = (dir.path().join("home"), dir.path().join("ws"));
    std::fs::create_dir_all(&ws).unwrap();
    let (mut child, mut out) = spawn(&home, &ws, "crash-txn", "txn");
    let mut line = String::new();
    out.read_line(&mut line).unwrap();
    assert_eq!(line.trim(), "READY");
    // Readers are not blocked by the open write transaction and do not see its row.
    let store = Store::open(&home, StoreOptions::default()).unwrap();
    let before = store
        .reader()
        .unwrap()
        .get_session("crash-txn")
        .unwrap()
        .event_cursor;
    child.kill().unwrap();
    child.wait().unwrap();

    integrity(&home);
    let cursor = assert_gapless(&store, "crash-txn");
    assert_eq!(cursor, before, "the uncommitted event rolled back");
    let mut w = store.resume_session("crash-txn", true).unwrap();
    let env = w
        .append_event(Event::Notice {
            level: "info".into(),
            text: "after".into(),
        })
        .unwrap();
    assert_eq!(
        env.seq,
        cursor + 2,
        "status_changed(interrupted) took cursor+1 on resume"
    );
}
