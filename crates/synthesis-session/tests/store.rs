use std::fs;
use std::path::{Path, PathBuf};

use serde_json::{Value, json};
use synthesis_core::{ParentRef, PermissionMode, SessionStatus, WorkspaceContext};
use synthesis_events::{Budget, Event, Record, TaskStatus};
use synthesis_session::node::{self, items_from_entries, sanitize_path};
use synthesis_session::{
    Error, NewMessage, NewSession, OrphanAction, OrphanState, ProcessProbe, ProcessRecord,
    Retention, SessionWriter, Store, StoreOptions, ToolCallRecord, recover_with,
};

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
}

fn new_session<'a>(id: Option<&str>, ws: &'a WorkspaceContext) -> NewSession<'a> {
    NewSession {
        id: id.map(str::to_string),
        workspace: ws,
        model_config_id: "anthropic/claude".into(),
        permission_mode: PermissionMode::AcceptEdits,
        permission_policy_id: "default".into(),
        budget: Budget::default(),
        parent: None,
        title: None,
    }
}

fn ws(dir: &Path) -> WorkspaceContext {
    WorkspaceContext::open("w1", dir, dir, &[], dir.join(".art"), "pg").unwrap()
}

fn create(e: &Env, id: &str, dir: &Path) -> SessionWriter {
    e.store()
        .create_session(new_session(Some(id), &ws(dir)))
        .unwrap()
}

fn notice(text: &str) -> Event {
    Event::Notice {
        level: "info".into(),
        text: text.into(),
    }
}

#[test]
fn create_persists_explicit_workspace_and_resumes_it() {
    let e = env();
    let (a, b) = (e.dir("proj-a"), e.dir("proj-b"));
    let store = e.store();
    let wa = store
        .create_session(new_session(Some("sa"), &ws(&a)))
        .unwrap();
    let wb = store
        .create_session(new_session(Some("sb"), &ws(&b)))
        .unwrap();
    let r = store.reader().unwrap();
    let ra = r.get_session("sa").unwrap();
    assert_eq!(Path::new(&ra.cwd), a);
    assert_eq!(Path::new(&ra.workspace_root), a);
    assert_eq!(ra.status, SessionStatus::Created);
    assert_eq!(ra.permission_mode, PermissionMode::AcceptEdits);
    assert_eq!(ra.model_config_id, "anthropic/claude");
    assert_eq!(r.get_session("sb").unwrap().cwd, b.to_str().unwrap());
    assert_eq!(r.list_sessions(Some(a.to_str().unwrap())).unwrap().len(), 1);
    assert_eq!(r.list_sessions(None).unwrap().len(), 2);
    // Resume rebuilds the persisted workspace, not the process cwd.
    drop((wa, wb));
    let w = store.resume_session("sa", false).unwrap();
    let rebuilt = w
        .get_session("sa")
        .unwrap()
        .open_workspace(store.home())
        .unwrap();
    assert_eq!(rebuilt.cwd, a);
    assert!(matches!(
        store.create_session(new_session(Some("sa"), &ws(&b))),
        Err(Error::Locked(_))
    ));
    drop(w);
    assert!(matches!(
        store.create_session(new_session(Some("sa"), &ws(&b))),
        Err(Error::AlreadyExists(_))
    ));
}

#[test]
fn sessions_in_different_dirs_do_not_interfere() {
    let e = env();
    let (a, b) = (e.dir("a"), e.dir("b"));
    let store = e.store();
    let mut wa = store
        .create_session(new_session(Some("sa"), &ws(&a)))
        .unwrap();
    let mut wb = store
        .create_session(new_session(Some("sb"), &ws(&b)))
        .unwrap();
    for i in 0..5 {
        assert_eq!(
            wa.append_event(notice(&format!("a{i}"))).unwrap().seq,
            i + 1
        );
        assert_eq!(
            wb.append_event(notice(&format!("b{i}"))).unwrap().seq,
            i + 1
        );
    }
    wa.append_message(NewMessage::user(json!("hi a"))).unwrap();
    assert_eq!(wb.read_messages("sb").unwrap().len(), 0);
    assert_eq!(wa.read_events("sa", 0, 100).unwrap().len(), 5);
    let mirror_a = wa.mirror_path().unwrap().to_path_buf();
    assert!(
        mirror_a.starts_with(
            e.home
                .join("projects")
                .join(sanitize_path(a.to_str().unwrap()))
        )
    );
    assert!(
        wb.mirror_path()
            .unwrap()
            .to_str()
            .unwrap()
            .contains(&sanitize_path(b.to_str().unwrap()))
    );
    assert!(!wb.mirror_path().unwrap().exists());
}

#[test]
fn events_are_gapless_redacted_and_replayable() {
    let e = env();
    let d = e.dir("p");
    let store = Store::open(
        &e.home,
        StoreOptions {
            secrets: vec!["hunter2-secret".into()],
            ..Default::default()
        },
    )
    .unwrap();
    let mut w = store
        .create_session(new_session(Some("s1"), &ws(&d)))
        .unwrap();
    w.task_id = Some("t1".into());
    let env1 = w
        .append_event(notice(
            "key sk-ant-api03-abcdefghijklmnop and hunter2-secret",
        ))
        .unwrap();
    assert_eq!(env1.seq, 1);
    let Event::Notice { text, .. } = &env1.event else {
        panic!()
    };
    assert_eq!(text, "key [REDACTED:sk-ant] and [REDACTED:secret]");
    for i in 0..9 {
        assert_eq!(w.append_event(notice(&format!("n{i}"))).unwrap().seq, i + 2);
    }
    let all = w.read_events("s1", 0, 100).unwrap();
    assert_eq!(all.len(), 10);
    let Record::Known(first) = &all[0] else {
        panic!()
    };
    assert_eq!(first, &env1);
    assert_eq!(first.task_id.as_deref(), Some("t1"));
    // Reconnect from a cursor, with a limit.
    let tail = w.read_events("s1", 7, 100).unwrap();
    assert_eq!(tail.len(), 3);
    assert_eq!(w.read_events("s1", 0, 4).unwrap().len(), 4);
    assert_eq!(w.get_session("s1").unwrap().event_cursor, 10);
    // The raw database never holds the secret.
    let raw = fs::read(e.home.join("synt.db")).unwrap();
    let wal = fs::read(e.home.join("synt.db-wal")).unwrap_or_default();
    for b in [raw, wal] {
        assert!(!String::from_utf8_lossy(&b).contains("hunter2-secret"));
    }
}

#[test]
fn large_payload_becomes_an_artifact_ref() {
    let e = env();
    let d = e.dir("p");
    let mut w = create(&e, "s1", &d);
    let big = "x".repeat(20_000);
    let env1 = w.append_event(notice(&big)).unwrap();
    let small = w.append_event(notice("small")).unwrap();
    let conn = rusqlite::Connection::open(e.home.join("synt.db")).unwrap();
    let row = |seq: i64| -> (Option<String>, Option<String>) {
        conn.query_row(
            "SELECT payload, payload_ref FROM events WHERE seq=?1",
            [seq],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap()
    };
    let (p, r) = row(1);
    assert!(p.is_none() && r.is_some());
    let (p, r) = row(2);
    assert!(p.is_some() && r.is_none());
    // read_events inlines it again.
    let back = w.read_events("s1", 0, 10).unwrap();
    assert_eq!(back[0], Record::Known(env1));
    assert_eq!(back[1], Record::Known(small));
    let arts = w.artifacts("s1").unwrap();
    assert_eq!(arts.len(), 1);
    assert_eq!(arts[0].kind, "event_payload");
    assert_eq!(arts[0].retention_policy, "session");
    assert!(
        e.home
            .join("sessions/s1")
            .join(&arts[0].reference.path)
            .exists()
    );
}

#[test]
fn status_updates_follow_the_state_machine() {
    let e = env();
    let d = e.dir("p");
    let mut w = create(&e, "s1", &d);
    assert!(matches!(
        w.update_status(SessionStatus::Running, None),
        Err(Error::InvalidTransition(_))
    ));
    assert_eq!(w.get_session("s1").unwrap().status, SessionStatus::Created);
    assert_eq!(
        w.get_session("s1").unwrap().event_cursor,
        0,
        "a rejected transition writes no event"
    );
    w.update_status(SessionStatus::Preparing, None).unwrap();
    w.update_status(SessionStatus::Running, Some("task"))
        .unwrap();
    w.update_status(SessionStatus::WaitingApproval, None)
        .unwrap();
    assert!(w.update_status(SessionStatus::Completed, None).is_err());
    let evs = w.read_events("s1", 0, 10).unwrap();
    assert_eq!(evs.len(), 3);
    let Record::Known(env2) = &evs[1] else {
        panic!()
    };
    assert!(
        matches!(&env2.event, Event::SessionStatusChanged { from, to, reason }
        if from == "preparing" && to == "running" && reason.as_deref() == Some("task"))
    );
}

#[test]
fn second_writer_is_refused_readers_are_fine() {
    let e = env();
    let d = e.dir("p");
    let store = e.store();
    let mut w = store
        .create_session(new_session(Some("s1"), &ws(&d)))
        .unwrap();
    w.append_event(notice("a")).unwrap();
    assert!(matches!(
        store.resume_session("s1", true),
        Err(Error::Locked(_))
    ));
    let r = store.reader().unwrap();
    assert_eq!(r.read_events("s1", 0, 10).unwrap().len(), 1);
    w.append_event(notice("b")).unwrap();
    assert_eq!(
        r.read_events("s1", 0, 10).unwrap().len(),
        2,
        "reader sees committed events"
    );
    drop(w);
    store.resume_session("s1", true).unwrap();
}

#[test]
fn schema_is_versioned_and_foreign_keys_are_on() {
    let e = env();
    drop(e.store());
    drop(e.store()); // reopening is a no-op
    let conn = rusqlite::Connection::open(e.home.join("synt.db")).unwrap();
    let v: i64 = conn
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .unwrap();
    assert_eq!(v, synthesis_session::SCHEMA_VERSION_LATEST);
    let mode: String = conn
        .query_row("PRAGMA journal_mode", [], |r| r.get(0))
        .unwrap();
    assert_eq!(mode, "wal");
    let n: i64 = conn
        .query_row("SELECT COUNT(*) FROM schema_migrations", [], |r| r.get(0))
        .unwrap();
    assert_eq!(n, synthesis_session::SCHEMA_VERSION_LATEST);
    for t in [
        "sessions",
        "tasks",
        "events",
        "messages",
        "tool_calls",
        "processes",
        "browser_contexts",
        "artifacts",
        "verification_reports",
        "approvals",
    ] {
        conn.query_row(&format!("SELECT COUNT(*) FROM {t}"), [], |r| {
            r.get::<_, i64>(0)
        })
        .unwrap();
    }
    conn.pragma_update(None, "user_version", 99).unwrap();
    assert!(matches!(
        Store::open(&e.home, StoreOptions::default()),
        Err(Error::SchemaTooNew { found: 99, .. })
    ));
    conn.pragma_update(
        None,
        "user_version",
        synthesis_session::SCHEMA_VERSION_LATEST,
    )
    .unwrap();
    Store::open(&e.home, StoreOptions::default()).unwrap();
}

#[test]
fn caps_and_ids_are_enforced() {
    let e = env();
    let d = e.dir("p");
    let mut w = create(&e, "s1", &d);
    assert!(matches!(
        w.append_event(notice(&"y".repeat(17 * 1024 * 1024))),
        Err(Error::TooLarge { .. })
    ));
    assert!(matches!(
        w.append_message(NewMessage::user(json!("y".repeat(9 * 1024 * 1024)))),
        Err(Error::TooLarge { .. })
    ));
    assert_eq!(w.get_session("s1").unwrap().event_cursor, 0);
    assert!(w.read_messages("s1").unwrap().is_empty());
    for bad in ["../x", "a/b", "", "a b", "x.y"] {
        assert!(
            matches!(
                e.store().create_session(new_session(Some(bad), &ws(&d))),
                Err(Error::InvalidId(_))
            ),
            "{bad}"
        );
    }
}

#[test]
fn mirror_writes_node_shaped_lines_and_reads_back() {
    let e = env();
    let d = e.dir("p");
    let mut w = create(&e, "11111111-2222-4333-8444-555555555555", &d);
    let tr =
        json!([{"type": "tool_result", "tool_use_id": "t1", "content": "x\ny", "is_error": false}]);
    w.append_message(NewMessage::user(json!("fix the bug")))
        .unwrap();
    let mut a = NewMessage::assistant(json!([
        {"type": "thinking", "thinking": "hmm"},
        {"type": "text", "text": "Looking. key sk-ant-api03-abcdefghijklmnop"},
        {"type": "tool_use", "id": "t1", "name": "Read", "input": {"file_path": "a.js"}}
    ]));
    a.msg.insert("id".into(), json!("m1"));
    a.msg.insert("stop_reason".into(), json!("tool_use"));
    w.append_message(a).unwrap();
    w.append_message(NewMessage::user(tr)).unwrap();
    w.append_message(NewMessage::user(json!(
        "<command-name>/compact</command-name><command-args></command-args>"
    )))
    .unwrap();
    w.append_message(NewMessage::compact_boundary("manual", "S"))
        .unwrap();
    w.set_title("my title").unwrap();
    w.set_permission_mode(PermissionMode::Plan).unwrap();
    w.mirror_hook_attachment("hook_success", "PreToolUse:Bash", "PreToolUse", Some(0));
    assert_eq!(w.mirror_failures(), 0);

    let path = w.mirror_path().unwrap().to_path_buf();
    assert_eq!(
        w.get_session(w.id())
            .unwrap()
            .legacy_transcript_path
            .as_deref(),
        path.to_str()
    );
    let text = fs::read_to_string(&path).unwrap();
    assert!(!text.contains("abcdefghijklmnop"), "mirror is redacted too");
    let lines: Vec<Value> = text
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(lines.len(), 8);
    for l in &lines {
        assert!(
            l["timestamp"].is_string()
                && l["sessionId"] == w.id()
                && l["cwd"] == d.to_str().unwrap(),
            "{l}"
        );
    }
    assert_eq!(lines[0]["type"], "user");
    assert_eq!(lines[1]["message"]["id"], "m1");
    assert_eq!(lines[4]["subtype"], "compact_boundary");
    assert_eq!(
        lines[5],
        json!({"type": "summary", "summary": "my title", "timestamp": lines[5]["timestamp"], "sessionId": w.id(), "cwd": d.to_str().unwrap()})
    );
    assert_eq!(lines[6]["permissionMode"], "plan");
    assert_eq!(lines[7]["attachment"]["hookName"], "PreToolUse:Bash");

    // What we wrote, read with the Node-semantics reader, is the conversation.
    let items = node::load_all_items(&path);
    let kinds: Vec<_> = items.iter().map(|i| i["kind"].as_str().unwrap()).collect();
    assert_eq!(
        kinds,
        ["user", "thinking", "text", "tool", "user", "compact"]
    );
    assert_eq!(items[3]["result"], "x\ny");
    assert_eq!(items[3]["status"], "done");
    assert_eq!(items[4]["text"], "/compact");
    assert_eq!(items[5]["trigger"], "restored");
    assert_eq!(node::session_meta(&path).first_prompt, "fix the bug");
    // And the same items come back from an export of the SQLite rows.
    let out = e.base.join("export.jsonl");
    assert_eq!(w.export_node_transcript(&out).unwrap(), 5);
    assert_eq!(
        items_from_entries(&node::read_entries(&out, None).unwrap(), None),
        items
    );
    assert!(matches!(
        w.export_node_transcript(&out),
        Err(Error::AlreadyExists(_))
    ));
    let rows = w.read_messages(w.id()).unwrap();
    assert!(rows[..4].iter().all(|r| r.compacted) && !rows[4].compacted);
    assert_eq!(
        w.get_session(w.id()).unwrap().title.as_deref(),
        Some("my title")
    );
}

#[test]
fn mirror_off_writes_no_node_file_and_external_edits_block_resume() {
    let e = env();
    let d = e.dir("p");
    let off = Store::open(
        &e.home,
        StoreOptions {
            write_node_transcripts: false,
            ..Default::default()
        },
    )
    .unwrap();
    let mut w = off
        .create_session(new_session(Some("s0"), &ws(&d)))
        .unwrap();
    w.append_message(NewMessage::user(json!("hi"))).unwrap();
    assert!(w.mirror_path().is_none());
    assert!(!e.home.join("projects").exists());
    assert_eq!(w.read_messages("s0").unwrap().len(), 1);
    drop(w);

    let store = e.store();
    let mut w = store
        .create_session(new_session(Some("s1"), &ws(&d)))
        .unwrap();
    w.append_message(NewMessage::user(json!("hi"))).unwrap();
    let path = w.mirror_path().unwrap().to_path_buf();
    drop(w);
    store.resume_session("s1", false).unwrap(); // untouched since our last write
    let mut f = fs::OpenOptions::new().append(true).open(&path).unwrap();
    std::io::Write::write_all(
        &mut f,
        b"{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":\"from node\"}}\n",
    )
    .unwrap();
    drop(f);
    assert!(matches!(
        store.resume_session("s1", false),
        Err(Error::TranscriptChanged(_))
    ));
    store.resume_session("s1", true).unwrap();
}

#[test]
fn import_reads_node_jsonl_without_touching_it() {
    let e = env();
    let d = e.dir("p");
    let src = e.base.join("11111111-2222-4333-8444-555555555555.jsonl");
    let text = [
        json!({"type": "user", "timestamp": "2026-10-02T00:00:01.000Z", "cwd": d, "message": {"role": "user", "content": "first"}}).to_string(),
        "{garbage".to_string(),
        json!({"type": "summary", "summary": "Named"}).to_string(),
        json!({"type": "permission-mode", "permissionMode": "plan"}).to_string(),
        json!({"type": "permission-mode", "permissionMode": "bypassPermissions"}).to_string(),
        json!({"type": "assistant", "isSidechain": true, "message": {"role": "assistant", "content": []}}).to_string(),
        json!({"type": "assistant", "timestamp": "2026-10-02T00:00:02.000Z", "message": {"id": "m", "role": "assistant", "content": [{"type": "text", "text": "ok"}], "usage": {"input_tokens": 1}}}).to_string(),
    ]
    .join("\n");
    fs::write(&src, &text).unwrap();
    let store = e.store();
    let (w, stats) = store
        .import_node_transcript(&src, new_session(None, &ws(&d)))
        .unwrap();
    assert_eq!((stats.lines, stats.messages, stats.skipped), (7, 2, 2));
    assert_eq!(fs::read_to_string(&src).unwrap(), text);
    let rec = w.get_session(w.id()).unwrap();
    assert_eq!(rec.id, "11111111-2222-4333-8444-555555555555");
    assert_eq!(rec.title.as_deref(), Some("Named"));
    assert_eq!(
        rec.permission_mode,
        PermissionMode::Plan,
        "bypassPermissions is never restored"
    );
    assert_eq!(rec.status, SessionStatus::Idle);
    assert_eq!(rec.legacy_transcript_path.as_deref(), src.to_str());
    let evs = w.read_events(&rec.id, 0, 10).unwrap();
    let Record::Unknown(last) = evs.last().unwrap() else {
        panic!("legacy.imported is store-owned")
    };
    assert_eq!(last.kind, "legacy.imported");
    assert_eq!(last.payload["lines"], 7);
    // Continuing the session appends to the same Node file, and resume sees it as ours.
    drop(w);
    let mut w = store.resume_session(&rec.id, false).unwrap();
    w.append_message(NewMessage::user(json!("more"))).unwrap();
    assert_eq!(node::load_all_items(&src).len(), 3);
    drop(w);
    store.resume_session(&rec.id, false).unwrap();
}

#[test]
fn records_tool_calls_processes_approvals_reports_artifacts_checkpoints() {
    let e = env();
    let d = e.dir("p");
    fs::write(d.join("a.txt"), "v1 sk-ant-api03-abcdefghijklmnop").unwrap();
    // Byte-exact snapshots are an explicit opt-out (default redacts: see tests/hardening.rs).
    let store = Store::open(
        &e.home,
        StoreOptions {
            redact_snapshots: false,
            ..StoreOptions::default()
        },
    )
    .unwrap();
    let mut w = store
        .create_session(new_session(Some("s1"), &ws(&d)))
        .unwrap();
    let id = w.id().to_string();

    let call = ToolCallRecord {
        id: "toolu_1".into(),
        turn_id: "t".into(),
        tool_name: "Bash".into(),
        input_ref: None,
        result_ref: None,
        status: "running".into(),
        decision: None,
        duration_ms: None,
    };
    w.upsert_tool_call(&call).unwrap();
    w.upsert_tool_call(&ToolCallRecord {
        status: "ok".into(),
        decision: Some("allow".into()),
        duration_ms: Some(12),
        ..call.clone()
    })
    .unwrap();
    let calls = w.tool_calls(&id).unwrap();
    assert_eq!(
        (calls.len(), calls[0].status.as_str(), calls[0].duration_ms),
        (1, "ok", Some(12))
    );

    let p = ProcessRecord {
        id: "p1".into(),
        name: Some("dev".into()),
        pid: Some(4242),
        pid_start: Some("tok".into()),
        cwd: "/x".into(),
        command: "serve --token sk-ant-api03-abcdefghijklmnop".into(),
        started_at: "2026-10-27T00:00:00.000Z".into(),
        ended_at: None,
        status: "running".into(),
        exit_code: None,
        log_artifact_id: None,
        log_cursor: None,
    };
    w.upsert_process(&p).unwrap();
    w.finish_process("p1", "exited", Some(0)).unwrap();
    let ps = w.processes(&id).unwrap();
    assert_eq!(
        (ps[0].status.as_str(), ps[0].exit_code),
        ("exited", Some(0))
    );
    assert!(!ps[0].command.contains("abcdefghij"));

    w.request_approval("ap1", "toolu_1", &json!({"prompt": "run?"}))
        .unwrap();
    assert_eq!(w.approvals(&id).unwrap()[0].decision, "pending");
    w.resolve_approval("ap1", "allow", "user").unwrap();
    let ap = &w.approvals(&id).unwrap()[0];
    assert_eq!(
        (
            ap.decision.as_str(),
            ap.reviewer.as_str(),
            ap.resolved_at.is_some()
        ),
        ("allow", "user", true)
    );
    assert!(matches!(
        w.resolve_approval("nope", "allow", "user"),
        Err(Error::NotFound(_))
    ));

    let t = w.begin_task("do it", &[]).unwrap();
    assert_eq!(w.task_id.as_deref(), Some(t.as_str()));
    w.record_verification_report(&t, 1, "failed", &json!({"checks": []}))
        .unwrap();
    w.record_verification_report(&t, 2, "passed", &json!({"checks": [1]}))
        .unwrap();
    assert_eq!(w.verification_reports(&t).unwrap().len(), 2);
    assert!(
        w.record_verification_report("missing", 1, "x", &json!({}))
            .is_err()
    );
    w.end_task(&t, TaskStatus::Completed, "done").unwrap();
    assert!(w.task_id.is_none());

    let a = w
        .put_artifact(
            "log",
            "text/plain",
            b"out sk-ant-api03-abcdefghijklmnop",
            Retention::Until("2026-12-01T00:00:00.000Z".into()),
        )
        .unwrap();
    let k = w
        .put_artifact("log", "text/plain", b"keep", Retention::Keep)
        .unwrap();
    let stored = w.artifact_bytes(&id, &a.artifact_id).unwrap();
    assert_eq!(String::from_utf8(stored).unwrap(), "out [REDACTED:sk-ant]");
    let rows = w.artifacts(&id).unwrap();
    let row = rows
        .iter()
        .find(|r| r.reference.artifact_id == a.artifact_id)
        .unwrap();
    assert_eq!(
        (row.retention_policy.as_str(), row.expires_at.as_deref()),
        ("ttl", Some("2026-12-01T00:00:00.000Z"))
    );
    assert_eq!(
        rows.iter()
            .find(|r| r.reference.artifact_id == k.artifact_id)
            .unwrap()
            .retention_policy,
        "keep"
    );

    // Checkpoint: event seq + message count + snapshots (byte-exact, inside the workspace only).
    w.append_event(notice("e1")).unwrap();
    w.append_message(NewMessage::user(json!("m0"))).unwrap();
    let cp = w
        .create_checkpoint(
            "before-refactor",
            &[PathBuf::from("a.txt"), d.join("new.txt")],
        )
        .unwrap();
    assert_eq!((cp.event_seq, cp.message_count, cp.files.len()), (1, 1, 2));
    assert!(
        cp.files[1].artifact_id.is_none(),
        "absent file recorded as absent"
    );
    let snap = w
        .artifact_bytes(&id, cp.files[0].artifact_id.as_ref().unwrap())
        .unwrap();
    assert_eq!(snap, fs::read(d.join("a.txt")).unwrap());
    assert_eq!(w.checkpoint(&id, "before-refactor").unwrap(), cp);
    assert_eq!(
        w.get_session(&id).unwrap().checkpoint_id.as_deref(),
        Some(cp.id.as_str())
    );
    assert!(
        w.create_checkpoint("before-refactor", &[]).is_err(),
        "names are unique per session"
    );
    let outside = e.dir("outside");
    fs::write(outside.join("s"), "x").unwrap();
    assert!(matches!(
        w.create_checkpoint("bad", &[outside.join("s")]),
        Err(Error::OutsideWorkspace(_))
    ));
    assert!(matches!(
        w.create_checkpoint("bad2", &[PathBuf::from("../outside/s")]),
        Err(Error::OutsideWorkspace(_))
    ));
}

#[test]
fn sub_agent_sessions_reference_their_parent() {
    let e = env();
    let d = e.dir("p");
    let store = e.store();
    drop(
        store
            .create_session(new_session(Some("parent"), &ws(&d)))
            .unwrap(),
    );
    let wsd = ws(&d);
    let mut n = new_session(Some("child"), &wsd);
    n.parent = Some(ParentRef {
        session_id: "parent".into(),
        tool_call_id: "toolu_9".into(),
    });
    drop(store.create_session(n).unwrap());
    let p = store
        .reader()
        .unwrap()
        .get_session("child")
        .unwrap()
        .parent
        .unwrap();
    assert_eq!(
        (p.session_id.as_str(), p.tool_call_id.as_str()),
        ("parent", "toolu_9")
    );
    let mut bad = new_session(Some("orphan"), &wsd);
    bad.parent = Some(ParentRef {
        session_id: "nope".into(),
        tool_call_id: "x".into(),
    });
    assert!(store.create_session(bad).is_err());
}

#[test]
fn path_sanitisation_odd_and_very_long_cwds() {
    assert_eq!(sanitize_path("/a-b"), "-a-b");
    assert_eq!(sanitize_path("/a/b"), "-a-b", "lossy like Node");
    assert_eq!(sanitize_path("/Users/me/my proj/é"), "-Users-me-my-proj--");
    assert_eq!(
        sanitize_path("😀"),
        "--",
        "one hyphen per UTF-16 unit, as in JS"
    );
    assert_eq!(sanitize_path("C:\\x\\y"), "C--x-y");
    let long = format!("/{}", "d".repeat(5000));
    let s = sanitize_path(&long);
    assert!(s.len() <= 220 && s.starts_with("-ddd"));
    assert_ne!(
        s,
        sanitize_path(&format!("/{}e", "d".repeat(5000))),
        "hash keeps long names distinct"
    );
    assert_eq!(s, sanitize_path(&long), "deterministic");
    let p = node::transcript_path(Path::new("/h"), &long, "id");
    assert!(
        p.file_name().unwrap() == "id.jsonl"
            && p.parent().unwrap().file_name().unwrap().len() < 255
    );
}

#[test]
fn real_directories_with_odd_and_long_names_get_a_mirror_file() {
    let e = env();
    let mut names = vec![
        "with space".to_string(),
        "ünï-çødé".to_string(),
        "emoji-😀".to_string(),
        "dots..and'quote".to_string(),
    ];
    if cfg!(unix) {
        names.push("a:b*c\"d".to_string());
    }
    for (i, n) in names.iter().enumerate() {
        let d = e.dir(n);
        let mut w = create(&e, &format!("odd-{i}"), &d);
        w.append_message(NewMessage::user(json!("hi"))).unwrap();
        assert_eq!(w.mirror_failures(), 0, "{n}");
        assert!(w.mirror_path().unwrap().exists(), "{n}");
        assert_eq!(
            node::session_meta(w.mirror_path().unwrap()).cwd,
            w.get_session(w.id()).unwrap().cwd
        );
    }
    // Deep path whose sanitised name is far over 255 bytes.
    let mut deep = e.base.join("deep");
    for _ in 0..7 {
        deep.push("n".repeat(100));
    }
    fs::create_dir_all(&deep).unwrap();
    let mut w = create(&e, "deep", &deep);
    w.append_message(NewMessage::user(json!("hi"))).unwrap();
    assert_eq!(w.mirror_failures(), 0);
    assert!(w.mirror_path().unwrap().exists());
}

// ---- recovery with a fake OS ----

struct FakeProbe {
    alive: Vec<(u32, &'static str)>,
    killed: std::cell::RefCell<Vec<u32>>,
}
impl ProcessProbe for FakeProbe {
    fn start_time(&self, pid: u32) -> Option<String> {
        self.alive
            .iter()
            .find(|a| a.0 == pid)
            .map(|a| a.1.to_string())
    }
    fn kill_tree(&self, pid: u32) -> std::io::Result<()> {
        self.killed.borrow_mut().push(pid);
        Ok(())
    }
}

fn proc(id: &str, pid: Option<u32>, start: Option<&str>) -> ProcessRecord {
    ProcessRecord {
        id: id.into(),
        name: None,
        pid,
        pid_start: start.map(str::to_string),
        cwd: "/x".into(),
        command: format!("cmd-{id}"),
        started_at: "2026-10-27T00:00:00.000Z".into(),
        ended_at: None,
        status: "running".into(),
        exit_code: None,
        log_artifact_id: None,
        log_cursor: None,
    }
}

#[test]
fn recover_interrupts_sessions_and_handles_orphans_safely() {
    let e = env();
    let d = e.dir("p");
    let mut w = create(&e, "crashed", &d);
    w.update_status(SessionStatus::Preparing, None).unwrap();
    w.update_status(SessionStatus::Running, None).unwrap();
    for p in [
        proc("alive", Some(100), Some("T1")),
        proc("reused", Some(200), Some("OLD")),
        proc("gone", Some(300), Some("T3")),
        proc("nopid", None, None),
        proc("unverified", Some(400), None),
    ] {
        w.upsert_process(&p).unwrap();
    }
    drop(w);
    let mut idle = create(&e, "fine", &d);
    idle.update_status(SessionStatus::Preparing, None).unwrap();
    idle.update_status(SessionStatus::Idle, None).unwrap();
    drop(idle);
    let live = create(&e, "live", &d); // holds its lock: must be left alone
    let probe = FakeProbe {
        alive: vec![(100, "T1"), (200, "NEW"), (400, "T4")],
        killed: Default::default(),
    };

    let rep = recover_with(&e.home, OrphanAction::Report, &probe).unwrap();
    assert_eq!(rep.interrupted, ["crashed"]);
    assert!(
        rep.skipped_live.is_empty(),
        "'live' is created, not running, and has no processes"
    );
    let mut lost = rep.lost.clone();
    lost.sort();
    assert_eq!(lost, ["gone", "nopid", "reused"]);
    let by: Vec<_> = rep
        .orphans
        .iter()
        .map(|o| (o.process_id.as_str(), o.state.clone(), o.killed))
        .collect();
    assert_eq!(
        by,
        [
            ("alive", OrphanState::Alive, false),
            ("unverified", OrphanState::Unverified, false)
        ]
    );
    assert!(probe.killed.borrow().is_empty(), "report mode never kills");
    let r = e.store().reader().unwrap();
    assert_eq!(
        r.get_session("crashed").unwrap().status,
        SessionStatus::Interrupted
    );
    assert_eq!(r.get_session("fine").unwrap().status, SessionStatus::Idle);
    let evs = r.read_events("crashed", 0, 10).unwrap();
    assert!(
        matches!(evs.last().unwrap(), Record::Known(e) if matches!(&e.event, Event::SessionStatusChanged { to, .. } if to == "interrupted"))
    );
    drop(live);

    // Idempotent in report mode, then kill only the verified orphan; the unverified one stays.
    assert_eq!(
        recover_with(&e.home, OrphanAction::Report, &probe)
            .unwrap()
            .orphans,
        rep.orphans
    );
    let rep2 = recover_with(&e.home, OrphanAction::Kill, &probe).unwrap();
    assert_eq!(*probe.killed.borrow(), [100]);
    assert!(rep2.orphans[0].killed && !rep2.orphans[1].killed);
    assert!(rep2.interrupted.is_empty());
    let rep3 = recover_with(&e.home, OrphanAction::Kill, &probe).unwrap();
    assert_eq!(rep3.orphans.len(), 1, "only the unverified one remains");
    assert_eq!(*probe.killed.borrow(), [100]);
    let st: Vec<_> = r
        .processes("crashed")
        .unwrap()
        .into_iter()
        .map(|p| (p.id, p.status))
        .collect();
    assert!(
        st.contains(&("alive".into(), "killed".into()))
            && st.contains(&("reused".into(), "lost".into()))
    );
}

#[cfg(unix)]
#[test]
fn recover_kills_a_real_orphan_only_when_the_start_time_matches() {
    use std::process::Command;
    let e = env();
    let d = e.dir("p");
    let mut child = Command::new("sleep").arg("60").spawn().unwrap();
    let pid = child.id();
    let tok = synthesis_session::process_start_time(pid).expect("start time of a live pid");
    assert_eq!(
        synthesis_session::process_start_time(pid).as_deref(),
        Some(tok.as_str()),
        "stable token"
    );
    let mut w = create(&e, "s", &d);
    w.update_status(SessionStatus::Preparing, None).unwrap();
    w.update_status(SessionStatus::Running, None).unwrap();
    w.upsert_process(&proc("wrong", Some(pid), Some("not-the-start-time")))
        .unwrap();
    drop(w);
    let rep = synthesis_session::recover(&e.home, OrphanAction::Kill).unwrap();
    assert_eq!(rep.lost, ["wrong"]);
    assert!(
        child.try_wait().unwrap().is_none(),
        "a pid with another start time is never killed"
    );
    let mut w = e.store().resume_session("s", true).unwrap();
    w.upsert_process(&proc("right", Some(pid), Some(&tok)))
        .unwrap();
    drop(w);
    let rep = synthesis_session::recover(&e.home, OrphanAction::Kill).unwrap();
    assert!(rep.orphans[0].killed, "{rep:?}");
    assert!(child.wait().is_ok());
    assert!(synthesis_session::process_start_time(pid).is_none());
}

/// A concurrent fork/exec elsewhere in the process briefly inherits an open lock fd, so a lock
/// released by `drop` can still look held for a moment. Reopening right after drop must succeed.
#[test]
fn reopen_after_drop_survives_concurrent_spawns() {
    let e = env();
    let d = e.dir("p");
    let store = e.store();
    drop(
        store
            .create_session(new_session(Some("s1"), &ws(&d)))
            .unwrap(),
    );
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let spawners: Vec<_> = (0..8)
        .map(|_| {
            let stop = stop.clone();
            std::thread::spawn(move || {
                while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                    let _ = std::process::Command::new("true").status();
                }
            })
        })
        .collect();
    for _ in 0..500 {
        drop(store.resume_session("s1", true).unwrap());
    }
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    for s in spawners {
        s.join().unwrap();
    }
}

fn image_message(text: &str) -> Value {
    json!([
        {"type": "text", "text": text},
        {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "SECRETPIXELS"}}
    ])
}

fn contents(home: &Path, sid: &str) -> Vec<(i64, String)> {
    let c = rusqlite::Connection::open(home.join("synt.db")).unwrap();
    let mut q = c
        .prepare("SELECT idx, content FROM messages WHERE session_id = ?1 ORDER BY idx")
        .unwrap();
    q.query_map([sid], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .map(Result::unwrap)
        .collect()
}

fn age(home: &Path, sid: &str, updated_at: &str) {
    rusqlite::Connection::open(home.join("synt.db"))
        .unwrap()
        .execute(
            "UPDATE sessions SET updated_at = ?1 WHERE id = ?2",
            [updated_at, sid],
        )
        .unwrap();
}

#[test]
fn images_of_old_sessions_are_replaced_in_place_and_recent_ones_are_kept() {
    let e = env();
    let store = e.store();
    for id in ["old", "new"] {
        let mut w = create(&e, id, &e.dir(id));
        w.append_message(NewMessage::user(image_message("see")))
            .unwrap();
        w.append_message(NewMessage::assistant(
            json!([{"type": "text", "text": "a cat"}]),
        ))
        .unwrap();
        w.append_message(NewMessage::user(
            json!([{"type": "tool_result", "tool_use_id": "t",
            "content": [{"type": "image", "source": {"data": "NESTEDPIXELS"}}]}]),
        ))
        .unwrap();
        drop(w);
    }
    // "now" is a fixed instant; the old session was last touched 40 days before it
    let now = time::macros::datetime!(2026-11-10 12:00 UTC);
    age(&e.home, "old", "2026-10-01T00:00:00.000Z");
    age(&e.home, "new", "2026-11-09T00:00:00.000Z");
    assert_eq!(
        store.purge_images(0, now).unwrap().images,
        0,
        "0 days keeps everything"
    );
    assert!(contents(&e.home, "old")[0].1.contains("SECRETPIXELS"));
    assert_eq!(
        store.purge_images(30, now).unwrap().images,
        2,
        "the message image and the nested one"
    );
    let old = contents(&e.home, "old");
    assert_eq!(
        old.iter().map(|r| r.0).collect::<Vec<_>>(),
        [0, 1, 2],
        "ids and order kept"
    );
    assert!(old[0].1.contains("[image removed after 30 days]") && old[0].1.contains("see"));
    assert!(!old.iter().any(|r| r.1.contains("PIXELS")));
    assert!(old[1].1.contains("a cat"));
    assert!(
        contents(&e.home, "new")[0].1.contains("SECRETPIXELS"),
        "recent session untouched"
    );
    // idempotent
    assert_eq!(store.purge_images(30, now).unwrap().images, 0);
}

#[test]
fn a_locked_database_is_skipped_not_an_error() {
    let e = env();
    let store = e.store();
    let mut w = create(&e, "s", &e.dir("s"));
    w.append_message(NewMessage::user(image_message("x")))
        .unwrap();
    drop(w);
    age(&e.home, "s", "2020-01-01T00:00:00.000Z");
    let other = rusqlite::Connection::open(e.home.join("synt.db")).unwrap();
    other.execute_batch("BEGIN IMMEDIATE").unwrap();
    let now = time::macros::datetime!(2026-11-10 12:00 UTC);
    assert_eq!(store.purge_images(30, now).unwrap().images, 0);
    other.execute_batch("COMMIT").unwrap();
    assert!(
        contents(&e.home, "s")[0].1.contains("SECRETPIXELS"),
        "nothing changed while locked"
    );
    assert_eq!(
        store.purge_images(30, now).unwrap().images,
        1,
        "next time it runs"
    );
}

fn raw_bytes(home: &Path) -> Vec<u8> {
    let mut all = vec![];
    for f in ["synt.db", "synt.db-wal"] {
        all.extend(fs::read(home.join(f)).unwrap_or_default());
    }
    all
}

fn has(hay: &[u8], needle: &str) -> bool {
    hay.windows(needle.len()).any(|w| w == needle.as_bytes())
}

#[test]
fn only_content_blocks_lose_their_images_and_the_bytes_are_gone_from_the_files() {
    let e = env();
    let store = e.store();
    {
        let mut w = create(&e, "s", &e.dir("s"));
        w.append_message(NewMessage::user(image_message("see")))
            .unwrap();
        // a tool_use whose INPUT merely says type image is not image content
        w.append_message(NewMessage::assistant(json!([
            {"type": "tool_use", "id": "t", "name": "Write",
             "input": {"content": {"type": "image", "note": "KEEPME"}}}
        ])))
        .unwrap();
        w.append_message(NewMessage::user(json!([{"type": "tool_result", "tool_use_id": "t",
            "content": [{"type": "text", "text": "ok"}, {"type": "image", "source": {"data": "TOOLSHOT"}}]}])))
            .unwrap();
    }
    age(&e.home, "s", "2020-01-01T00:00:00.000Z");
    assert!(
        has(&raw_bytes(&e.home), "SECRETPIXELS"),
        "control: the bytes are in the files"
    );
    let now = time::macros::datetime!(2026-11-10 12:00 UTC);
    assert_eq!(store.purge_images(30, now).unwrap().images, 2);
    let rows = contents(&e.home, "s");
    assert!(
        rows[1].1.contains("KEEPME") && rows[1].1.contains("\"image\""),
        "tool_use input untouched: {}",
        rows[1].1
    );
    assert!(rows[2].1.contains("ok") && rows[2].1.contains("[image removed after 30 days]"));
    assert!(
        !rows
            .iter()
            .any(|r| r.1.contains("SECRETPIXELS") || r.1.contains("TOOLSHOT"))
    );
    // freed pages are zeroed and the log is folded in: no copy of the pixels is left in the files
    let raw = raw_bytes(&e.home);
    assert!(
        !has(&raw, "SECRETPIXELS") && !has(&raw, "TOOLSHOT"),
        "image bytes survive in the database files"
    );
}

#[test]
fn meta_images_go_too_and_a_log_held_open_by_a_reader_is_reported_not_hidden() {
    let e = env();
    let store = e.store();
    {
        let mut w = create(&e, "s", &e.dir("s"));
        w.append_message(NewMessage::user(image_message("see")))
            .unwrap();
    }
    let c = rusqlite::Connection::open(e.home.join("synt.db")).unwrap();
    c.execute(
        "UPDATE messages SET meta = ?1",
        [
            json!({"extra": {"attachment": {"type": "image", "source": {"data": "METAPIXELS"}}}})
                .to_string(),
        ],
    )
    .unwrap();
    age(&e.home, "s", "2020-01-01T00:00:00.000Z");
    // a reader with an open snapshot keeps the log from being truncated
    let reader = rusqlite::Connection::open(e.home.join("synt.db")).unwrap();
    reader
        .execute_batch("BEGIN; SELECT count(*) FROM messages;")
        .unwrap();
    let now = time::macros::datetime!(2026-11-10 12:00 UTC);
    let r = store.purge_images(30, now).unwrap();
    assert_eq!(r.images, 2, "the content image and the one in meta");
    assert!(!r.wal_truncated, "a busy checkpoint is said, not hidden");
    let (content, meta): (String, String) = c
        .query_row("SELECT content, meta FROM messages", [], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })
        .unwrap();
    assert!(
        !content.contains("SECRETPIXELS") && !meta.contains("METAPIXELS"),
        "{content} {meta}"
    );
    reader.execute_batch("COMMIT").unwrap();
    // nothing left to do: nothing reported
    let again = store.purge_images(30, now).unwrap();
    assert_eq!((again.images, again.wal_truncated), (0, true));
}

#[test]
fn usage_by_day_sums_across_sessions_and_cuts_days_at_the_offset() {
    use synthesis_events::Usage;
    let e = env();
    let (da, db) = (e.dir("a"), e.dir("b"));
    let turn = |i: u64, o: u64| Event::TurnCompleted {
        stop_reason: "end_turn".into(),
        usage: Some(Usage {
            input_tokens: i,
            output_tokens: o,
            cache_read_input_tokens: None,
            cache_creation_input_tokens: None,
            cost_usd: None,
        }),
        duration_ms: 1,
    };
    let mut a = create(&e, "sa", &da);
    let mut b = create(&e, "sb", &db);
    let s1 = a.append_event(turn(100, 50)).unwrap().seq;
    let s2 = a.append_event(turn(10, 5)).unwrap().seq;
    let s3 = a
        .append_event(Event::UserMessage {
            text: "hi".into(),
            display_text: None,
            images: vec![],
        })
        .unwrap()
        .seq;
    let s4 = b.append_event(turn(7, 3)).unwrap().seq;
    drop((a, b));
    let conn = rusqlite::Connection::open(e.home.join("synt.db")).unwrap();
    let set = |sid: &str, seq: u64, ts: &str| {
        conn.execute(
            "UPDATE events SET ts=?1 WHERE session_id=?2 AND seq=?3",
            rusqlite::params![ts, sid, seq as i64],
        )
        .unwrap();
    };
    // day 20513 is 2026-03-01; 23:30Z is already 03-02 at +120 min; 03-02 10:00Z is 03-02 in both
    set("sa", s1, "2026-03-01T23:30:00.000Z");
    set("sa", s2, "2026-03-02T10:00:00.000Z");
    set("sa", s3, "2026-03-02T10:00:01.000Z");
    set("sb", s4, "2026-03-02T11:00:00.000Z");
    let r = synthesis_session::Reader::open(&e.home).unwrap();
    let since = 0;
    let utc = r.usage_by_day(since, 0).unwrap();
    assert_eq!(utc.len(), 2);
    assert_eq!(
        (utc[0].day, utc[0].tokens, utc[0].sessions),
        (20513, 150, 1)
    );
    assert_eq!(utc[1].day, 20514);
    assert_eq!((utc[1].tokens, utc[1].output, utc[1].messages), (25, 8, 1));
    assert_eq!(utc[1].sessions, 2);
    let east = r.usage_by_day(since, 120).unwrap();
    assert_eq!(east.len(), 1);
    assert_eq!(
        (east[0].day, east[0].tokens, east[0].sessions),
        (20514, 175, 2)
    );
    // `since` cuts older events
    let late = r.usage_by_day(1_772_445_600_000, 0).unwrap(); // 2026-03-02T10:00Z
    assert_eq!(late.len(), 1);
}

#[test]
fn usage_by_day_classifies_tool_starts() {
    let e = env();
    let d = e.dir("a");
    let mut a = create(&e, "sa", &d);
    for (i, name) in ["Bash", "Bash", "Edit", "Write", "browser_navigate", "mcp__gh__issue", "Read"]
        .iter()
        .enumerate()
    {
        a.append_event(Event::ToolStarted {
            tool_call_id: format!("t{i}"),
            name: (*name).into(),
            side_effect: "read".into(),
            cwd: ".".into(),
        })
        .unwrap();
    }
    drop(a);
    let r = synthesis_session::Reader::open(&e.home).unwrap();
    let days = r.usage_by_day(0, 0).unwrap();
    let t = &days[0];
    assert_eq!(
        (t.tools, t.commands, t.files, t.browser, t.mcp),
        (7, 2, 2, 1, 1)
    );
}

#[test]
fn prune_removes_only_old_unlocked_empty_sessions_and_listing_hides_empty_ones() {
    let e = env();
    let d = e.dir("w");
    let store = e.store();
    let hour = std::time::Duration::from_secs(3600);
    let now = time::OffsetDateTime::now_utc();
    let later = now + time::Duration::hours(2);
    let mut with_msg = create(&e, "with-msg", &d);
    with_msg
        .append_message(NewMessage::user(json!("hi")))
        .unwrap();
    let mut with_task = create(&e, "with-task", &d);
    with_task.begin_task("do it", &[]).unwrap();
    with_task.append_event(notice("n")).unwrap();
    let _locked = create(&e, "locked", &d);
    let _current = create(&e, "current", &d);
    drop(create(&e, "empty", &d));
    let empty_dir = e.home.join("sessions").join("empty");
    assert!(empty_dir.exists());

    let r = store.reader().unwrap();
    let used: Vec<_> = r
        .list_used_sessions(None)
        .unwrap()
        .into_iter()
        .map(|s| s.id)
        .collect();
    assert_eq!(used.len(), 2, "{used:?}");
    assert!(used.contains(&"with-msg".into()) && used.contains(&"with-task".into()));

    // recent: nothing goes
    assert_eq!(store.prune_empty_sessions("current", hour, now).unwrap(), 0);
    // old: only the unlocked, non-current, empty one
    assert_eq!(
        store.prune_empty_sessions("current", hour, later).unwrap(),
        1
    );
    assert!(!empty_dir.exists());
    let mut left: Vec<_> = r
        .list_sessions(None)
        .unwrap()
        .into_iter()
        .map(|s| s.id)
        .collect();
    left.sort();
    assert_eq!(left, ["current", "locked", "with-msg", "with-task"]);
    assert_eq!(
        store.prune_empty_sessions("current", hour, later).unwrap(),
        0
    );
}

#[test]
fn close_hides_a_session_from_the_lists_but_keeps_it_and_its_messages() {
    let e = env();
    let d = e.dir("w");
    let store = e.store();
    let mut s = create(&e, "to-close", &d);
    s.append_message(NewMessage::user(json!("hi"))).unwrap();
    drop(s);
    let r = store.reader().unwrap();
    assert_eq!(r.list_used_sessions(None).unwrap().len(), 1);
    assert!(store.close_session("to-close").unwrap());
    assert!(!store.close_session("to-close").unwrap(), "already closed");
    assert!(r.list_used_sessions(None).unwrap().is_empty());
    assert_eq!(r.list_sessions(None).unwrap().len(), 1);
    assert_eq!(r.read_messages("to-close").unwrap().len(), 1);
}
