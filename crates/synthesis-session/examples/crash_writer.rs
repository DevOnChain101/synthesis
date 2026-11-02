//! Test helper for tests/crash.rs: a writer process the test kills.
//! usage: crash_writer <synthesis_home> <workspace_dir> <session_id> loop|txn
//! `loop`: appends events forever, printing `seq N` after each commit.
//! `txn`: leaves an uncommitted write transaction open, prints `READY`, then sleeps.

use std::io::Write;
use std::path::Path;

use synthesis_core::{PermissionMode, SessionStatus, WorkspaceContext};
use synthesis_events::{Budget, Event};
use synthesis_session::{NewSession, Store, StoreOptions};

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let (home, ws_dir, id, mode) = (
        Path::new(&a[1]),
        Path::new(&a[2]),
        a[3].clone(),
        a[4].as_str(),
    );
    let store = Store::open(home, StoreOptions::default()).unwrap();
    let ws = WorkspaceContext::open(
        "w",
        ws_dir,
        ws_dir,
        &[],
        home.join("sessions").join(&id),
        "pg",
    )
    .unwrap();
    let mut w = store
        .create_session(NewSession {
            id: Some(id),
            workspace: &ws,
            model_config_id: "p/m".into(),
            permission_mode: PermissionMode::Default,
            permission_policy_id: "pol".into(),
            budget: Budget::default(),
            parent: None,
            title: None,
        })
        .unwrap();
    w.update_status(SessionStatus::Preparing, None).unwrap();
    w.update_status(SessionStatus::Running, None).unwrap();
    let mut out = std::io::stdout();
    if mode == "txn" {
        let conn = rusqlite::Connection::open(home.join("synt.db")).unwrap();
        conn.busy_timeout(std::time::Duration::from_secs(5))
            .unwrap();
        conn.execute_batch(&format!(
            "BEGIN IMMEDIATE;
             INSERT INTO events(session_id,seq,v,ts,type,payload) SELECT id, event_cursor+1, 1, 'x', 'notice', '{{}}' FROM sessions WHERE id='{}';
             UPDATE sessions SET event_cursor = event_cursor + 1 WHERE id='{}';",
            w.id(),
            w.id()
        ))
        .unwrap();
        writeln!(out, "READY").unwrap();
        out.flush().unwrap();
        std::thread::sleep(std::time::Duration::from_secs(120));
        return;
    }
    loop {
        let env = w
            .append_event(Event::Notice {
                level: "info".into(),
                text: "x".repeat(2000),
            })
            .unwrap();
        writeln!(out, "seq {}", env.seq).unwrap();
        out.flush().unwrap();
    }
}
