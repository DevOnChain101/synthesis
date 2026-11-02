//! Security hardening (review M13, M14, L1, L5): file modes, import trust, kill guards, caps.

use std::fs;
use std::path::{Path, PathBuf};

use serde_json::json;
use synthesis_core::{PermissionMode, SessionStatus, WorkspaceContext};
use synthesis_events::{Budget, Event};
use synthesis_session::{NewSession, Retention, SessionWriter, Store, StoreOptions};

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

fn ws(dir: &Path) -> WorkspaceContext {
    WorkspaceContext::open("w1", dir, dir, &[], dir.join(".art"), "pg").unwrap()
}

fn new_session<'a>(id: Option<&str>, ws: &'a WorkspaceContext) -> NewSession<'a> {
    NewSession {
        id: id.map(str::to_string),
        workspace: ws,
        model_config_id: "m".into(),
        permission_mode: PermissionMode::Default,
        permission_policy_id: "default".into(),
        budget: Budget::default(),
        parent: None,
        title: None,
    }
}

fn session(e: &Env, opts: StoreOptions, id: &str) -> (Store, SessionWriter) {
    let d = e.base.join("p");
    fs::create_dir_all(&d).unwrap();
    let store = Store::open(&e.home, opts).unwrap();
    let w = store
        .create_session(new_session(Some(id), &ws(&d)))
        .unwrap();
    (store, w)
}

fn notice(text: &str) -> Event {
    Event::Notice {
        level: "info".into(),
        text: text.into(),
    }
}

#[cfg(unix)]
fn mode(p: &Path) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    fs::metadata(p).unwrap().permissions().mode() & 0o777
}

#[cfg(unix)]
#[test]
fn home_db_artifacts_and_mirror_are_private() {
    use synthesis_session::NewMessage;
    let e = env();
    let (_s, mut w) = session(&e, StoreOptions::default(), "s1");
    w.append_message(NewMessage::user(json!("hi"))).unwrap();
    w.append_event(notice(&"x".repeat(20_000))).unwrap(); // payload artifact
    let a = w
        .put_artifact("log", "text/plain", b"out", Retention::Session)
        .unwrap();
    assert_eq!(mode(&e.home), 0o700);
    assert_eq!(mode(&e.home.join("sessions")), 0o700);
    assert_eq!(mode(&e.home.join("sessions/s1")), 0o700);
    assert_eq!(mode(&e.home.join("sessions/s1/artifacts")), 0o700);
    assert_eq!(mode(&e.home.join("sessions/s1/lock")), 0o600);
    for f in ["synthesis.db", "synthesis.db-wal", "synthesis.db-shm"] {
        assert_eq!(mode(&e.home.join(f)), 0o600, "{f}");
    }
    assert_eq!(mode(&e.home.join("sessions/s1").join(&a.path)), 0o600);
    for r in w.artifacts("s1").unwrap() {
        assert_eq!(
            mode(&e.home.join("sessions/s1").join(&r.reference.path)),
            0o600
        );
    }
    assert_eq!(mode(w.mirror_path().unwrap()), 0o600);
}

#[cfg(unix)]
#[test]
fn existing_loose_home_and_db_are_tightened() {
    use std::os::unix::fs::PermissionsExt;
    let e = env();
    fs::create_dir_all(&e.home).unwrap();
    fs::set_permissions(&e.home, fs::Permissions::from_mode(0o755)).unwrap();
    fs::write(e.home.join("synthesis.db"), b"").unwrap();
    fs::set_permissions(
        e.home.join("synthesis.db"),
        fs::Permissions::from_mode(0o644),
    )
    .unwrap();
    Store::open(&e.home, StoreOptions::default()).unwrap();
    assert_eq!(mode(&e.home), 0o700);
    assert_eq!(mode(&e.home.join("synthesis.db")), 0o600);
}

#[cfg(unix)]
#[test]
fn symlinked_db_is_refused() {
    let e = env();
    fs::create_dir_all(&e.home).unwrap();
    let target = e.base.join("elsewhere.db");
    std::os::unix::fs::symlink(&target, e.home.join("synthesis.db")).unwrap();
    assert!(Store::open(&e.home, StoreOptions::default()).is_err());
    assert!(!target.exists(), "must not create through the symlink");
}

#[cfg(unix)]
#[test]
fn artifact_creation_never_follows_a_planted_symlink() {
    let e = env();
    let (_s, mut w) = session(&e, StoreOptions::default(), "s1");
    w.put_artifact("log", "text/plain", b"a", Retention::Session)
        .unwrap();
    // Replace the artifacts dir with a symlink: creating files must fail, not write through.
    let dir = e.home.join("sessions/s1/artifacts");
    let outside = e.base.join("outside");
    fs::create_dir_all(&outside).unwrap();
    fs::remove_dir_all(&dir).unwrap();
    std::os::unix::fs::symlink(&outside, &dir).unwrap();
    assert!(
        w.put_artifact("log", "text/plain", b"b", Retention::Session)
            .is_err()
    );
    assert_eq!(fs::read_dir(&outside).unwrap().count(), 0);
}

#[test]
fn imported_permission_mode_is_capped_unless_trusted() {
    let e = env();
    let d = e.base.join("p");
    fs::create_dir_all(&d).unwrap();
    for (raw, untrusted, trusted) in [
        (
            "acceptEdits",
            PermissionMode::Default,
            PermissionMode::AcceptEdits,
        ),
        ("auto", PermissionMode::Default, PermissionMode::Auto),
        ("plan", PermissionMode::Plan, PermissionMode::Plan),
        (
            "bypassPermissions",
            PermissionMode::AcceptEdits,
            PermissionMode::AcceptEdits,
        ), // ignored: the caller's own mode stands
    ] {
        for (trust, want) in [(false, untrusted), (true, trusted)] {
            let src = e.base.join(format!("{raw}-{trust}.jsonl"));
            fs::write(
                &src,
                format!(
                    "{}\n{}\n",
                    json!({"type":"user","message":{"role":"user","content":"hi"}}),
                    json!({"type":"permission-mode","permissionMode":raw})
                ),
            )
            .unwrap();
            let store = Store::open(&e.home, StoreOptions::default()).unwrap();
            let wsx = ws(&d);
            let mut new = new_session(None, &wsx);
            new.permission_mode = PermissionMode::AcceptEdits;
            let (w, _) = if trust {
                store.import_node_transcript_trusted(&src, new, true)
            } else {
                store.import_node_transcript(&src, new)
            }
            .unwrap();
            let got = w.get_session(w.id()).unwrap().permission_mode;
            assert_eq!(got, want, "{raw} trust={trust}");
        }
    }
    let _ = SessionStatus::Idle;
}

// ---- snapshots ----

#[test]
fn snapshots_withhold_secret_files_and_redact_text_by_default() {
    let e = env();
    let (_s, mut w) = session(&e, StoreOptions::default(), "s1");
    let d = e.base.join("p");
    let key = "sk-ant-abcdefghijklmnopqrstuvwxyz";
    fs::write(d.join(".env"), format!("TOKEN={key}\n")).unwrap();
    fs::write(d.join("id_rsa"), "PRIVATE").unwrap();
    fs::write(d.join("notes.txt"), format!("key {key} end")).unwrap();
    fs::write(d.join("plain.txt"), "nothing here").unwrap();
    let files: Vec<PathBuf> = [".env", "id_rsa", "notes.txt", "plain.txt"]
        .iter()
        .map(|n| d.join(n))
        .collect();
    let cp = w.create_checkpoint("c", &files).unwrap();
    let bytes = |i: usize| {
        String::from_utf8(
            w.artifact_bytes("s1", cp.files[i].artifact_id.as_ref().unwrap())
                .unwrap(),
        )
        .unwrap()
    };
    for i in [0, 1] {
        assert!(
            bytes(i).starts_with("[synt: secret file withheld"),
            "{}",
            bytes(i)
        );
        assert!(!bytes(i).contains("PRIVATE") && !bytes(i).contains(key));
        assert!(
            cp.files[i]
                .lossy_sha256
                .as_ref()
                .is_some_and(|h| h.len() == 64)
        );
    }
    assert!(bytes(2).contains("[REDACTED:") && !bytes(2).contains(key));
    assert!(cp.files[2].lossy_sha256.is_some());
    assert_eq!(bytes(3), "nothing here");
    assert!(cp.files[3].lossy_sha256.is_none());
    let back = w.checkpoint("s1", "c").unwrap();
    assert_eq!(back.files, cp.files, "lossy marker survives storage");
}

#[test]
fn snapshots_can_be_made_byte_exact() {
    let e = env();
    let opts = StoreOptions {
        redact_snapshots: false,
        ..StoreOptions::default()
    };
    let (_s, mut w) = session(&e, opts, "s1");
    let f = e.base.join("p/.env");
    fs::write(&f, "A=1\n").unwrap();
    let cp = w.create_checkpoint("c", &[f]).unwrap();
    let id = cp.files[0].artifact_id.as_ref().unwrap();
    assert_eq!(w.artifact_bytes("s1", id).unwrap(), b"A=1\n");
    assert!(cp.files[0].lossy_sha256.is_none());
}

// ---- retention / quotas ----

#[test]
fn event_row_cap_is_hard_and_seq_stays_gapless() {
    let e = env();
    let opts = StoreOptions {
        max_events_per_session: Some(5),
        ..StoreOptions::default()
    };
    let (_s, mut w) = session(&e, opts, "s1");
    let mut n = 0;
    let err = loop {
        match w.append_event(notice("x")) {
            Ok(_) => n += 1,
            Err(err) => break err,
        }
    };
    assert_eq!(n, 5);
    assert!(matches!(
        err,
        synthesis_session::Error::QuotaExceeded { what: "events", .. }
    ));
    let seqs: Vec<u64> = w
        .read_events("s1", 0, 100)
        .unwrap()
        .iter()
        .map(|r| match r {
            synthesis_events::Record::Known(e) => e.seq,
            synthesis_events::Record::Unknown(u) => u.seq,
        })
        .collect();
    assert_eq!(seqs, [1, 2, 3, 4, 5]);
}

fn delta(block: i64, text: &str) -> Event {
    Event::ModelDelta {
        block_index: block,
        kind: "text".into(),
        text: text.into(),
    }
}

fn texts(w: &SessionWriter) -> Vec<(u64, String)> {
    w.read_events("s1", 0, 100_000)
        .unwrap()
        .into_iter()
        .filter_map(|r| match r {
            synthesis_events::Record::Known(e) => match e.event {
                Event::ModelDelta { text, .. } => Some((e.seq, text)),
                _ => None,
            },
            _ => None,
        })
        .collect()
}

#[test]
fn old_deltas_of_completed_messages_collapse_keeping_seq_and_text() {
    let e = env();
    let opts = StoreOptions {
        keep_recent_deltas: 2,
        ..StoreOptions::default()
    };
    let (_s, mut w) = session(&e, opts, "s1");
    for t in ["a", "b", "c", "d"] {
        w.append_event(delta(0, t)).unwrap();
    }
    // Not complete yet: nothing may be touched.
    assert_eq!(w.compact_deltas().unwrap(), 0);
    w.append_event(Event::AssistantMessage {
        message_id: Some("m".into()),
        content: vec![json!({"type": "text", "text": "abcd"})],
        stop_reason: "end_turn".into(),
        usage: None,
    })
    .unwrap();
    for t in ["e", "f"] {
        w.append_event(delta(0, t)).unwrap();
    }
    let before: String = texts(&w).iter().map(|t| t.1.clone()).collect();
    assert!(w.compact_deltas().unwrap() > 0);
    let after = texts(&w);
    // Seq intact (1..4 and 6,7), joined text identical, old burst now one batch + empties.
    assert_eq!(
        after.iter().map(|t| t.0).collect::<Vec<_>>(),
        [1, 2, 3, 4, 6, 7]
    );
    assert_eq!(
        after.iter().map(|t| t.1.clone()).collect::<String>(),
        before
    );
    assert_eq!(after[0].1, "ab"); // keep=2: the 4 old deltas minus the newest 2 of those rows
    assert_eq!(w.compact_deltas().unwrap(), 0, "idempotent");
    assert_eq!(w.read_events("s1", 0, 100).unwrap().len(), 7);
}

#[test]
fn byte_cap_triggers_delta_collapse_then_refuses() {
    let e = env();
    let opts = StoreOptions {
        max_event_bytes_per_session: Some(40_000),
        keep_recent_deltas: 0,
        ..StoreOptions::default()
    };
    let (_s, mut w) = session(&e, opts, "s1");
    let big = "x".repeat(200);
    // 127 events of ~230 bytes; the 128th append runs the byte check.
    for _ in 0..100 {
        w.append_event(delta(0, &big)).unwrap();
    }
    w.append_event(Event::AssistantMessage {
        message_id: None,
        content: vec![json!({"type": "text", "text": "done"})],
        stop_reason: "end_turn".into(),
        usage: None,
    })
    .unwrap();
    for _ in 0..40 {
        w.append_event(delta(0, &big)).unwrap(); // crosses seq 128 with ~32 KB: under the cap
    }
    let mut hit = None;
    for i in 0..2000 {
        if let Err(err) = w.append_event(notice(&big)) {
            hit = Some((i, err));
            break;
        }
    }
    let (_, err) = hit.expect("cap must eventually refuse");
    assert!(matches!(
        err,
        synthesis_session::Error::QuotaExceeded {
            what: "event bytes",
            ..
        }
    ));
    // Collapse ran: the first burst is now empty rows after its batch heads.
    assert!(texts(&w).iter().any(|t| t.1.is_empty()));
    let n = w.read_events("s1", 0, 100_000).unwrap().len() as u64;
    assert_eq!(w.get_session("s1").unwrap().event_cursor, n, "gapless");
}

#[test]
fn artifact_quota_is_enforced_per_session() {
    let e = env();
    let opts = StoreOptions {
        max_artifact_bytes_per_session: Some(10),
        ..StoreOptions::default()
    };
    let (_s, mut w) = session(&e, opts, "s1");
    w.put_artifact("log", "text/plain", b"123456", Retention::Session)
        .unwrap();
    let err = w
        .put_artifact("log", "text/plain", b"123456", Retention::Session)
        .unwrap_err();
    assert!(matches!(
        err,
        synthesis_session::Error::QuotaExceeded { .. }
    ));
    w.put_artifact("log", "text/plain", b"1234", Retention::Session)
        .unwrap();
    let files = fs::read_dir(e.home.join("sessions/s1/artifacts"))
        .unwrap()
        .count();
    assert_eq!(files, 2, "the refused artifact left no file");
}

// ---- orphan artifact files ----

#[test]
fn rolled_back_transaction_leaves_no_artifact_file() {
    let e = env();
    let (store, mut w) = session(&e, StoreOptions::default(), "s1");
    {
        let c = rusqlite::Connection::open(e.home.join("synthesis.db")).unwrap();
        c.execute_batch(
            "CREATE TRIGGER boom BEFORE INSERT ON events WHEN NEW.type='notice'
             BEGIN SELECT RAISE(ABORT,'boom'); END;",
        )
        .unwrap();
    }
    assert!(w.append_event(notice(&"y".repeat(20_000))).is_err());
    let dir = e.home.join("sessions/s1/artifacts");
    assert_eq!(fs::read_dir(&dir).map(|d| d.count()).unwrap_or(0), 0);
    // A crash leftover is swept on the next writer open; referenced files are kept.
    let kept = w
        .put_artifact("log", "text/plain", b"keep", Retention::Session)
        .unwrap();
    fs::write(dir.join("stray.bin"), b"orphan").unwrap();
    fs::write(dir.join(".dead.tmp"), b"orphan").unwrap();
    drop(w);
    let w = store.resume_session("s1", true).unwrap();
    let mut names: Vec<_> = fs::read_dir(&dir)
        .unwrap()
        .map(|d| d.unwrap().file_name().into_string().unwrap())
        .collect();
    names.sort();
    assert_eq!(names, [kept.path.trim_start_matches("artifacts/")]);
    assert_eq!(w.artifact_bytes("s1", &kept.artifact_id).unwrap(), b"keep");
}

// ---- cancelling recovery ----

#[test]
fn crash_during_cancelling_is_repaired_to_interrupted() {
    let e = env();
    let (store, mut w) = session(&e, StoreOptions::default(), "s1");
    w.update_status(SessionStatus::Preparing, None).unwrap();
    w.update_status(SessionStatus::Running, None).unwrap();
    w.update_status(SessionStatus::Cancelling, None).unwrap();
    drop(w);
    let rep = synthesis_session::recover_with(
        &e.home,
        synthesis_session::OrphanAction::Report,
        &Fake::default(),
    )
    .unwrap();
    assert_eq!(rep.interrupted, ["s1"]);
    let r = store.reader().unwrap();
    assert_eq!(
        r.get_session("s1").unwrap().status,
        SessionStatus::Interrupted
    );
    let last = r.read_events("s1", 0, 100).unwrap().pop().unwrap();
    assert!(matches!(last, synthesis_events::Record::Known(e)
        if matches!(&e.event, Event::SessionStatusChanged { from, to, .. } if from == "cancelling" && to == "interrupted")));
    // resume_session repairs the same way, and the edge is not opened for other states.
    let (_s2, mut w) = session(&e, StoreOptions::default(), "s2");
    w.update_status(SessionStatus::Preparing, None).unwrap();
    w.update_status(SessionStatus::Running, None).unwrap();
    w.update_status(SessionStatus::Cancelling, None).unwrap();
    drop(w);
    let mut w = store.resume_session("s2", true).unwrap();
    assert_eq!(
        w.get_session("s2").unwrap().status,
        SessionStatus::Interrupted
    );
    assert!(w.update_status(SessionStatus::Cancelling, None).is_err());
}

// ---- kill guards ----

use std::cell::{Cell, RefCell};
use synthesis_session::{OrphanAction, ProcessProbe, ProcessRecord, recover_with};

#[derive(Default)]
struct Fake {
    /// start_time answers per call, last one repeats
    tokens: RefCell<Vec<&'static str>>,
    killed: RefCell<Vec<u32>>,
    pgid: Cell<Option<u32>>,
}
impl ProcessProbe for Fake {
    fn start_time(&self, _pid: u32) -> Option<String> {
        let mut t = self.tokens.borrow_mut();
        let v = if t.len() > 1 {
            t.remove(0)
        } else {
            *t.first()?
        };
        Some(v.to_string())
    }
    fn kill_tree(&self, pid: u32) -> std::io::Result<()> {
        self.killed.borrow_mut().push(pid);
        Ok(())
    }
    fn own_pgid(&self) -> Option<u32> {
        self.pgid.get()
    }
}

fn running_with(e: &Env, pid: u32, start: &str) {
    let (_s, mut w) = session(e, StoreOptions::default(), "s1");
    w.update_status(SessionStatus::Preparing, None).unwrap();
    w.update_status(SessionStatus::Running, None).unwrap();
    w.upsert_process(&ProcessRecord {
        id: "p".into(),
        name: None,
        pid: Some(pid),
        pid_start: Some(start.into()),
        cwd: "/x".into(),
        command: "c".into(),
        started_at: "2026-10-27T00:00:00.000Z".into(),
        ended_at: None,
        status: "running".into(),
        exit_code: None,
        log_artifact_id: None,
        log_cursor: None,
    })
    .unwrap();
}

#[test]
fn kill_refuses_reserved_self_and_own_group_pids() {
    let own = std::process::id();
    for (pid, pgid) in [(1, None), (0, None), (own, None), (4242, Some(4242))] {
        let e = env();
        running_with(&e, pid, "T");
        let f = Fake::default();
        f.tokens.borrow_mut().push("T");
        f.pgid.set(pgid);
        let rep = recover_with(&e.home, OrphanAction::Kill, &f).unwrap();
        assert!(f.killed.borrow().is_empty(), "pid {pid}");
        let o = &rep.orphans[0];
        assert!(
            !o.killed && o.kill_error.as_deref().unwrap().contains("refusing"),
            "{o:?}"
        );
    }
}

#[test]
fn kill_reverifies_the_start_time_right_before_killing() {
    let e = env();
    running_with(&e, 4242, "T");
    // First probe matches, the pid is reused before the kill.
    let f = Fake::default();
    f.tokens.borrow_mut().extend(["T", "OTHER"]);
    let rep = recover_with(&e.home, OrphanAction::Kill, &f).unwrap();
    assert!(f.killed.borrow().is_empty());
    assert!(!rep.orphans[0].killed && rep.orphans[0].kill_error.is_some());
    // Stable identity still kills.
    let f = Fake::default();
    f.tokens.borrow_mut().extend(["T", "T"]);
    let rep = recover_with(&e.home, OrphanAction::Kill, &f).unwrap();
    assert_eq!(*f.killed.borrow(), [4242]);
    assert!(rep.orphans[0].killed);
}
