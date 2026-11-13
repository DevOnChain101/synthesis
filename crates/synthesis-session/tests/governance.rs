//! Hash-chained events, the stop request and the stored worktree pins (docs/UNATTENDED.md).

use std::path::PathBuf;

use rusqlite::Connection;
use serde_json::json;
use synthesis_core::{PermissionMode, WorkspaceContext};
use synthesis_events::{Budget, Event};
use synthesis_session::{NewSession, SessionWriter, Store, StoreOptions};

fn open(tag: &str, opts: StoreOptions) -> (tempfile::TempDir, PathBuf, Store, SessionWriter) {
    let tmp = tempfile::tempdir().unwrap();
    let base = synthesis_core::canonicalize(tmp.path()).unwrap();
    let (home, dir) = (base.join("home"), base.join("proj"));
    std::fs::create_dir_all(&dir).unwrap();
    let ws = WorkspaceContext::open("w1", &dir, &dir, &[], dir.join(".art"), tag).unwrap();
    let store = Store::open(&home, opts).unwrap();
    let w = store
        .create_session(NewSession {
            id: Some(tag.into()),
            workspace: &ws,
            model_config_id: "a/m".into(),
            permission_mode: PermissionMode::AcceptEdits,
            permission_policy_id: "default".into(),
            budget: Budget::default(),
            parent: None,
            title: None,
        })
        .unwrap();
    (tmp, home, store, w)
}

fn notice(t: &str) -> Event {
    Event::Notice {
        level: "info".into(),
        text: t.into(),
    }
}

fn db(home: &std::path::Path) -> Connection {
    Connection::open(home.join("synt.db")).unwrap()
}

fn five(w: &mut SessionWriter) {
    for i in 0..5 {
        w.append_event(notice(&format!("n{i}"))).unwrap();
    }
}

#[test]
fn an_untouched_chain_verifies() {
    let (_t, _h, _s, mut w) = open("s1", StoreOptions::default());
    five(&mut w);
    let r = w.verify_chain("s1").unwrap();
    assert_eq!((r.chained, r.unchained, r.broken), (5, 0, None));
}

#[test]
fn a_rewritten_event_is_found_at_the_next_link() {
    let (_t, h, _s, mut w) = open("s1", StoreOptions::default());
    five(&mut w);
    db(&h)
        .execute(
            "UPDATE events SET payload=?1 WHERE session_id='s1' AND seq=2",
            [json!({"level": "info", "text": "forged"}).to_string()],
        )
        .unwrap();
    let b = w.verify_chain("s1").unwrap().broken.unwrap();
    assert_eq!(b.seq, 3, "{b:?}");
}

#[test]
fn a_rewritten_last_event_or_a_cut_tail_is_found_by_the_head() {
    let (_t, h, _s, mut w) = open("s1", StoreOptions::default());
    five(&mut w);
    db(&h)
        .execute(
            "UPDATE events SET payload=?1 WHERE session_id='s1' AND seq=5",
            [json!({"level": "info", "text": "forged"}).to_string()],
        )
        .unwrap();
    assert!(w.verify_chain("s1").unwrap().broken.is_some());

    let (_t, h, _s, mut w) = open("s2", StoreOptions::default());
    five(&mut w);
    db(&h)
        .execute("DELETE FROM events WHERE session_id='s2' AND seq=5", [])
        .unwrap();
    assert!(w.verify_chain("s2").unwrap().broken.is_some());
}

#[test]
fn a_removed_middle_event_or_link_is_found() {
    let (_t, h, _s, mut w) = open("s1", StoreOptions::default());
    five(&mut w);
    db(&h)
        .execute("DELETE FROM events WHERE session_id='s1' AND seq=3", [])
        .unwrap();
    let b = w.verify_chain("s1").unwrap().broken.unwrap();
    assert_eq!(b.seq, 4, "{b:?}");

    let (_t, h, _s, mut w) = open("s2", StoreOptions::default());
    five(&mut w);
    db(&h)
        .execute(
            "UPDATE events SET prev=NULL WHERE session_id='s2' AND seq=4",
            [],
        )
        .unwrap();
    assert!(w.verify_chain("s2").unwrap().broken.is_some());
}

#[test]
fn collapsing_old_deltas_keeps_the_chain_valid() {
    let (_t, _h, _s, mut w) = open(
        "s1",
        StoreOptions {
            keep_recent_deltas: 1,
            ..StoreOptions::default()
        },
    );
    for t in ["a", "b", "c"] {
        w.append_event(Event::ModelDelta {
            block_index: 0,
            kind: "text".into(),
            text: t.into(),
        })
        .unwrap();
    }
    w.append_event(Event::AssistantMessage {
        message_id: Some("m".into()),
        content: vec![json!({"type": "text", "text": "abc"})],
        stop_reason: "end_turn".into(),
        usage: None,
    })
    .unwrap();
    assert!(w.compact_deltas().unwrap() > 0);
    assert_eq!(w.verify_chain("s1").unwrap().broken, None);
}

#[test]
fn events_from_before_the_chain_are_reported_not_failed() {
    let (_t, h, _s, mut w) = open("s1", StoreOptions::default());
    for i in 0..2 {
        w.append_event(notice(&format!("old{i}"))).unwrap();
    }
    // what a session written by an older build looks like: no links, no head
    db(&h)
        .execute("UPDATE events SET prev=NULL WHERE session_id='s1'", [])
        .unwrap();
    db(&h)
        .execute(
            "UPDATE sessions SET chain_head=NULL, chained=0 WHERE id='s1'",
            [],
        )
        .unwrap();
    for i in 0..3 {
        w.append_event(notice(&format!("new{i}"))).unwrap();
    }
    let r = w.verify_chain("s1").unwrap();
    assert_eq!(
        (r.unchained, r.chained, r.broken, r.legacy),
        (2, 3, None, true)
    );
    // ... but a link removed from the middle of a chain is not "old"
    db(&h)
        .execute(
            "UPDATE events SET prev=NULL WHERE session_id='s1' AND seq=4",
            [],
        )
        .unwrap();
    assert!(w.verify_chain("s1").unwrap().broken.is_some());
}

#[test]
fn a_chained_session_cannot_be_downgraded_by_nulling_links_and_head() {
    let (_t, h, _s, mut w) = open("s1", StoreOptions::default());
    five(&mut w);
    // the attacker removes every link and the head, hoping for "an old session"
    db(&h)
        .execute("UPDATE events SET prev=NULL WHERE session_id='s1'", [])
        .unwrap();
    db(&h)
        .execute("UPDATE sessions SET chain_head=NULL WHERE id='s1'", [])
        .unwrap();
    let r = w.verify_chain("s1").unwrap();
    assert!(r.broken.is_some() && !r.legacy, "{r:?}");
}

#[test]
fn rewriting_the_whole_chain_without_the_key_is_found() {
    let (_t, h, _s, mut w) = open("s1", StoreOptions::default());
    five(&mut w);
    // a DB-only attacker rewrites event 5 and recomputes sha256 links; the keyed MAC differs
    db(&h)
        .execute(
            "UPDATE events SET payload=?1 WHERE session_id='s1' AND seq=5",
            [json!({"level": "info", "text": "forged"}).to_string()],
        )
        .unwrap();
    db(&h)
        .execute("UPDATE sessions SET chain_head='00' WHERE id='s1'", [])
        .unwrap();
    assert!(w.verify_chain("s1").unwrap().broken.is_some());
    // and without the key file nothing can be verified at all
    std::fs::remove_file(h.join("chain.key")).unwrap();
    assert!(w.verify_chain("s1").unwrap().no_key);
}

#[test]
fn stop_request_counts_only_when_newer_than_the_process_start_and_needs_a_holder() {
    let (_t, _h, s, w) = open("s1", StoreOptions::default());
    let r = s.reader().unwrap();
    let before = synthesis_session::timestamp();
    assert!(!r.stop_requested_since("s1", &before).unwrap());
    // a live writer holds the session lock
    assert!(s.session_held("s1").unwrap());
    std::thread::sleep(std::time::Duration::from_millis(5));
    assert!(s.request_stop("s1").unwrap());
    assert!(r.stop_requested_since("s1", &before).unwrap());
    // a process that starts later (a resume) ignores the old request, and a resume does not erase it
    std::thread::sleep(std::time::Duration::from_millis(5));
    let after = synthesis_session::timestamp();
    assert!(!r.stop_requested_since("s1", &after).unwrap());
    assert!(!s.request_stop("nope").unwrap());
    drop(w);
    assert!(!s.session_held("s1").unwrap(), "nobody holds it any more");
    s.resume_session("s1", true).unwrap();
    assert!(
        r.stop_requested_since("s1", &before).unwrap(),
        "the mark survives a resume"
    );
}

#[test]
fn git_info_round_trips() {
    let (_t, _h, s, mut w) = open("s1", StoreOptions::default());
    assert_eq!(w.get_session("s1").unwrap().git_info, None);
    w.set_git_info(r#"{"x":1}"#).unwrap();
    assert_eq!(
        s.reader()
            .unwrap()
            .get_session("s1")
            .unwrap()
            .git_info
            .as_deref(),
        Some(r#"{"x":1}"#)
    );
}
