//! The remote engine client (`Engine::start_remote`) against a fake engine process: this same test
//! binary re-run with `--fake-engine MODE` (so it needs no WSL, no `synt` build and runs on every OS).
//! `harness = false`: the fake engine's stdout must carry protocol lines only.

use std::io::{BufRead, Write};
use std::path::Path;
use std::time::{Duration, Instant};

use futures::channel::mpsc::UnboundedReceiver;
use serde_json::{Value, json};
use synthesis_app::proto::{PROTOCOL, VERSION};
use synthesis_config::Env;
use synthesis_desktop::engine::{Choice, Engine, NOT_REMOTE, UiMsg};
use synthesis_desktop::remote::Launcher;
use synthesis_desktop::wsl;
use synthesis_permissions::Mode;

const EVENT: &str = r#"{"v":1,"seq":1,"ts":"2026-01-01T00:00:00Z","session_id":"s1","task_id":null,"turn_id":null,"agent_id":null,"type":"notice","payload":{"level":"info","text":"hi"}}"#;

fn say(v: Value) {
    let mut o = std::io::stdout().lock();
    writeln!(o, "{v}").unwrap();
    o.flush().unwrap();
}

fn fake_engine(mode: &str) {
    match mode {
        "mismatch" => {
            return say(json!({"msg":"hello","version":"0.0.0-other","protocol":PROTOCOL}));
        }
        "eof" => return,
        _ => say(json!({"msg":"hello","version":VERSION,"protocol":PROTOCOL})),
    }
    if mode == "chatty" {
        // a lot of long stderr: drained and capped, never a stall
        for _ in 0..3000 {
            eprintln!("\u{1b}[31m{}", "e".repeat(3000));
        }
    }
    if mode == "flood" {
        // a hostile engine: endless unreadable lines, then it hangs
        for _ in 0..1000 {
            println!("garbage");
        }
        std::thread::sleep(Duration::from_secs(600));
    }
    for line in std::io::stdin().lock().lines() {
        let req: Value = serde_json::from_str(&line.unwrap()).unwrap();
        let id = req["id"].clone();
        match req["method"].as_str().unwrap() {
            "start" => {
                // as a fake wsl.exe: `cwd` is what `--cd` set, `root` what `start` asked for
                let cd = std::env::args().skip_while(|a| a != "--cd").nth(1);
                let cwd = cd.as_deref().unwrap_or("/engine/side");
                let root = req["params"]["cwd"].as_str().filter(|_| cd.is_some());
                say(
                    json!({"msg":"result","id":id,"ok":{"sid":"s1","label":"fake/model","models":["fake/model"],"mode":"acceptEdits","trusted":true,"cwd":cwd,"root":root.unwrap_or("/engine/side"),"notices":["n\u{1b}[31m1"]}}),
                )
            }
            "send" => {
                say(json!({"msg":"result","id":id,"ok":{}}));
                if let Some(i) = req["params"].get("images") {
                    say(json!({"msg":"notice","text":format!("images:{i}")}));
                }
                println!("{EVENT}");
                match mode {
                    "die-after-send" => std::process::exit(3),
                    "hang" => std::thread::sleep(Duration::from_secs(600)),
                    "event-flood" => (0..20_000).for_each(|_| println!("{EVENT}")),
                    "forged-restart" => say(json!({"msg":"restarted"})),
                    "hello-flood" => (0..150).for_each(|_| {
                        say(json!({"msg":"hello","version":VERSION,"protocol":PROTOCOL}))
                    }),
                    "ask-flood" => (0..100).for_each(|i| {
                        say(json!({"msg":"ask.approval","id":format!("f{i}"),"agent_id":null,"tool":"Bash","input":{},"dialog":"permission","prompt":"p","reason":"r","origin":null,"suggestions":[]}))
                    }),
                    _ => {}
                }
                say(
                    json!({"msg":"ask.approval","id":"a1","agent_id":null,"tool":"Bash","input":{},"dialog":"permission","prompt":"p","reason":"r","origin":null,"suggestions":[]}),
                );
            }
            "workspaces.list" => {
                let many: Vec<Value> = (0..300)
                    .map(|i| json!({"root":format!("/r{i}"),"key":format!("/r{i}"),"label":"l","sessions":[],"available":true,"changes":null}))
                    .collect();
                let mut ws = vec![
                    json!({"root":"/home/me/repo","key":"/home/me/repo","label":"re\u{1b}[31mpo","sessions":[{"id":"s1","title":"t\u{1b}[2Jitle","status":"idle","updated_at":"","mtime":0,"is_live":false}],"available":true,"changes":null}),
                ];
                ws.extend(many);
                say(json!({"msg":"result","id":id,"ok":{"workspaces":ws,"truncated":false}}))
            }
            "session.preview" => say(
                json!({"msg":"result","id":id,"ok":{"events":[serde_json::from_str::<Value>(EVENT).unwrap(), {"junk":true}],"truncated":false}}),
            ),
            "fs.list" => {
                // a hostile tree: escapes, a name with a slash, an empty name, a huge depth, a bad status, and
                // pages without end
                let off = req["params"]["offset"].as_u64().unwrap_or(0);
                let mut rows = vec![];
                if off == 0 {
                    rows.push(json!([0, "a\u{1b}[31m.rs", false, "M"]));
                    rows.push(json!([0, "x/y", false, null]));
                    rows.push(json!([0, "", false, null]));
                    rows.push(json!([99999, "deep", true, "Z"]));
                }
                rows.extend((0..196).map(|i| json!([0, format!("f{off}-{i}"), false, null])));
                say(
                    json!({"msg":"result","id":id,"ok":{"rows":rows,"total":99999,"next":off+200,"truncated":false,"note":"n\u{1b}[2J","trusted":false}}),
                )
            }
            "fs.read" => {
                let lines: Vec<String> = (0..5000).map(|i| format!("l{i}\u{1b}[31m")).collect();
                say(json!({"msg":"result","id":id,"ok":{"lines":lines,"size":123,"cut":true}}))
            }
            "image.put" => {
                let p = &req["params"];
                let data = p["data"].as_str().unwrap_or_default();
                let bad = data.len() > 400_000
                    || !data.len().is_multiple_of(4) && p["last"] != true
                    || (p["seq"] == 0 && (p["mime"] != "image/png" || p["size"] != 600_000));
                if bad {
                    say(json!({"msg":"result","id":id,"error":"bad chunk"}))
                } else if p["last"] == true {
                    say(json!({"msg":"result","id":id,"ok":{"id":"img-1","bytes":600_000}}))
                } else {
                    say(json!({"msg":"result","id":id,"ok":{"upload":"u1"}}))
                }
            }
            "rewind.list" => say(
                json!({"msg":"result","id":id,"ok":{"checkpoints":[{"name":"c\u{1b}1","when":"2026-11-09 14:02 UTC","after_message":3,"files":["a.txt"]}]}}),
            ),
            "rewind.dry_run" => say(
                json!({"msg":"result","id":id,"ok":{"token":"rw-1","entries":[{"shown":"/w/a\u{1b}.txt","kind":"restore","size":5,"reason":null},{"shown":"/w/b","kind":"skip","size":0,"reason":"why\u{1b}"}]}}),
            ),
            "rewind.apply" if req["params"]["token"] == "rw-1" => say(
                json!({"msg":"result","id":id,"ok":{"restored":1,"removed":0,"skipped":1,"failed":["f\u{1b}"],"changed":[]}}),
            ),
            "rewind.apply" => say(json!({"msg":"result","id":id,"error":"no longer valid"})),
            "clean.plan" => {
                let plans: Vec<Value> = ["ClosedEmpty", "Inactive", "All"]
                    .iter()
                    .map(|s| json!({"scope":s,"token":format!("cl-{s}"),"counts":{"sessions":2,"workspaces":1,"messages":4,"events":9,"disk_bytes":2048,"screenshot_bytes":0,"snapshot_bytes":0,"log_bytes":0,"skipped_running":0,"skipped_other":0}}))
                    .collect();
                say(json!({"msg":"result","id":id,"ok":{"plans":plans}}))
            }
            "clean.apply" => say(
                json!({"msg":"result","id":id,"ok":{"deleted":2,"events":9,"skipped_running":0,"skipped_other":0,"failed":[],"freed_bytes":2048,"backup":null}}),
            ),
            "memory.list" => say(
                json!({"msg":"result","id":id,"ok":{"records":[{"id":"m-1","kind":"fact","scope":"user","trust":"owner","title":"t\u{1b}[31mitle"}],"note":null}}),
            ),
            "mcp.list" => say(
                json!({"msg":"result","id":id,"ok":{"servers":[{"name":"s\u{1b}","state":"connected","transport":"stdio","tool_count":1,"error":null,"tools":[["t","d\u{1b}"]]}]}}),
            ),
            "doctor.summary" => say(
                json!({"msg":"result","id":id,"ok":{"rows":[{"level":"warn","text":"w\u{1b}[31m"},{"level":"mystery","text":"x"}]}}),
            ),
            "usage.summary" => say(json!({"msg":"result","id":id,"ok":{"days":[]}})),
            "trust.state" => say(
                json!({"msg":"result","id":id,"ok":{"root":"/home/me/re\u{1b}po","cwd":"/home/me/repo","trusted":false,"remembered":"changed","changed_files":["a"],"since":"2026-01-01","records":[{"root":"/x","scope":"folder","trusted_at":"2026-01-01","state":"config unchanged"}]}}),
            ),
            "trust.set" | "model.set" | "session.new" if mode == "busy" => say(
                json!({"msg":"result","id":id,"error":"refused: busy: finish or cancel the task first"}),
            ),
            "trust.set" | "model.set" | "session.new" if mode == "fatal" => {
                say(json!({"msg":"result","id":id,"error":"fatal: the session could not continue"}))
            }
            // an error the engine did not mark as a refusal: nothing says the session is still there
            "trust.set" | "model.set" | "session.new" if mode == "unmarked" => {
                say(json!({"msg":"result","id":id,"error":"something went wrong"}))
            }
            "session.rename" | "clear" | "compact" if mode == "slow" => {
                std::thread::sleep(Duration::from_millis(1500));
                say(json!({"msg":"result","id":id,"ok":{"title":"t","started":true}}))
            }
            "model.set" if mode == "wsl" => say(
                json!({"msg":"result","id":id,"ok":{"sid":"s2","label":"fake/model","models":[],"mode":"default","trusted":false,"cwd":"/elsewhere","root":"/elsewhere","notices":[]}}),
            ),
            "trust.set" | "model.set" | "session.new" => {
                // the old session's trailing line, the marker, then the new session
                println!("{EVENT}");
                if mode == "stale" {
                    // the ending session's last words: all dropped by the client
                    say(json!({"msg":"task_done","error":null}));
                    say(json!({"msg":"notice","text":"old"}));
                    say(json!({"msg":"compacted","error":null}));
                }
                say(json!({"msg":"restarted"}));
                println!(
                    "{}",
                    EVENT
                        .replace(r#""seq":1"#, r#""seq":2"#)
                        .replace("\"s1\"", "\"s2\"")
                );
                let cd = std::env::args().skip_while(|a| a != "--cd").nth(1);
                let cwd = cd.as_deref().unwrap_or("/engine/side");
                say(
                    json!({"msg":"result","id":id,"ok":{"sid":"s2","label":"fake/model","models":[],"mode":"default","trusted":req["method"]=="trust.set","cwd":cwd,"root":cwd,"notices":[]}}),
                )
            }
            "set_mode" => say(json!({"msg":"result","id":id,"ok":{"mode":req["params"]["mode"]}})),
            "answer" => {
                say(
                    json!({"msg":"result","id":id,"ok":{"accepted":req["params"]["choice"]=="once"}}),
                );
                say(json!({"msg":"task_done","error":null}));
            }
            "close" => {
                say(json!({"msg":"result","id":id,"ok":{"worktree":null}}));
                return;
            }
            _ => say(json!({"msg":"result","id":id,"ok":{}})),
        }
    }
}

fn launcher(mode: &str) -> Launcher {
    Launcher::new(
        std::env::current_exe().unwrap(),
        vec!["--fake-engine".into(), mode.into()],
        &Env::from_process(),
    )
}

fn start_with(l: &Launcher) -> Result<(Engine, UnboundedReceiver<UiMsg>), String> {
    Engine::start_remote(l, &std::env::temp_dir(), &Default::default())
}

fn start(mode: &str) -> Result<(Engine, UnboundedReceiver<UiMsg>), String> {
    start_with(&launcher(mode))
}

/// Runs a send against an engine that misbehaves and waits for the task to end with an error.
fn killed_after_send(mode: &str, idle: Option<Duration>) {
    let mut l = launcher(mode);
    l.idle_timeout = idle.unwrap_or(l.idle_timeout);
    let (e, mut rx) = start_with(&l).expect("starts");
    e.send("go".into()).unwrap();
    let t0 = Instant::now();
    loop {
        assert!(
            t0.elapsed() < Duration::from_secs(60),
            "{mode}: no TaskDone"
        );
        if matches!(next(&mut rx, mode), UiMsg::TaskDone(Err(_))) {
            break;
        }
    }
    assert!(!e.running());
    assert!(e.send("more".into()).is_err());
}

fn next(rx: &mut UnboundedReceiver<UiMsg>, what: &str) -> UiMsg {
    let t0 = Instant::now();
    loop {
        match rx.try_recv() {
            Ok(m) => return m,
            Err(_) => {
                assert!(t0.elapsed() < Duration::from_secs(30), "timed out: {what}");
                std::thread::sleep(Duration::from_millis(10));
            }
        }
    }
}

fn mismatch_is_refused() {
    let e = start("mismatch").err().expect("refused");
    assert!(e.contains("mismatch"), "{e}");
}

fn exit_before_hello_is_an_error() {
    assert!(start("eof").is_err());
}

fn a_flooding_engine_is_killed_not_waited_for() {
    let t0 = Instant::now();
    assert!(start("flood").is_err());
    assert!(t0.elapsed() < Duration::from_secs(60), "the start hung");
}

fn a_session_over_the_pipe() {
    let (e, mut rx) = start("ok").expect("starts");
    assert_eq!((e.sid.as_str(), e.label.as_str()), ("s1", "fake/model"));
    assert!(
        !e.notices[0].contains('\u{1b}'),
        "text from the engine is made printable"
    );
    // the engine's own folder strings are display text; the host opens only the folder it chose
    assert_eq!(e.cwd, std::env::temp_dir().to_string_lossy());
    assert_eq!(e.remote_cwd.as_deref(), Some("/engine/side"));
    // trust is the engine's decision (the person's choice went through `trust.set`); shown as it reports it
    assert!(e.trusted);
    // the mode is the engine's, not an assumed default; a change shows once the engine accepted it
    assert!(e.is_remote() && e.mode() == Mode::AcceptEdits);
    e.set_mode(Mode::Plan);
    let t0 = Instant::now();
    while e.mode() != Mode::Plan {
        assert!(t0.elapsed() < Duration::from_secs(30), "mode change");
        std::thread::sleep(Duration::from_millis(10));
    }
    e.set_mode(Mode::BypassPermissions);
    assert!(matches!(next(&mut rx, "refusal"), UiMsg::BadEvent(m) if m.contains("bypass")));
    assert_eq!(e.mode(), Mode::Plan);
    assert!(!e.running());
    e.send("go".into()).unwrap();
    assert!(e.running());
    assert_eq!(e.send("again".into()), Err("a task is already running"));
    assert!(matches!(next(&mut rx, "event"), UiMsg::Event(ev) if ev.seq == 1));
    let UiMsg::Approval(a) = next(&mut rx, "ask") else {
        panic!("an approval ask")
    };
    assert_eq!((a.id.as_str(), a.tool.as_str()), ("a1", "Bash"));
    assert!(
        !e.answer("nope", Choice::Once),
        "an ask the engine never made"
    );
    assert!(e.answer("a1", Choice::Once));
    assert!(!e.answer("a1", Choice::Once), "answered once");
    assert!(matches!(next(&mut rx, "done"), UiMsg::TaskDone(Ok(()))));
    assert!(!e.running());
    // what the local engine has and the remote one does not yet says so
    assert_eq!(e.mcp_report(), vec![NOT_REMOTE.to_string()]);
    // remote compact/rename are background calls: they return at once and answer as messages
    let e = std::sync::Arc::new(e);
    assert_eq!(e.compact(None), Ok(false));
    // no host store, no note file: a `#` note has no place to go in remote mode
    assert!(e.notes.save(true, "x").is_err());
    assert!(e.synthesis_home.as_os_str().is_empty());
    let e = std::sync::Arc::try_unwrap(e).ok().expect("sole owner");
    assert!(e.close().is_none());
}

fn the_store_methods_are_parsed_bounded_and_made_printable() {
    let (e, _rx) = start("ok").expect("starts");
    let r = e.remote().expect("remote");
    let ws = r.workspaces().unwrap();
    assert_eq!(
        ws.workspaces.len(),
        200,
        "bounded whatever the engine sends"
    );
    let w = &ws.workspaces[0];
    assert!(!w.label.contains('\u{1b}') && !w.sessions[0].title.contains('\u{1b}'));
    // events: the unreadable one is skipped
    assert_eq!(r.preview("s1").unwrap().len(), 1);
    assert!(r.usage(0).unwrap().is_empty());
    let t = r.trust_state().unwrap();
    assert!(!t.root.contains('\u{1b}'));
    assert_eq!(
        (
            t.remembered.as_str(),
            t.changed_files.len(),
            t.records.len()
        ),
        ("changed", 1, 1)
    );
    assert!(e.close().is_none());
}

fn files_images_rewind_clean_and_settings_are_parsed_bounded_and_made_printable() {
    let (e, mut rx) = start("ok").expect("starts");
    let e = std::sync::Arc::new(e);
    let r = e.remote().expect("remote");
    // the tree: bounded, printable, no row that could corrupt a path, depth and status clamped
    let t = r.fs_list().unwrap();
    assert!(
        t.rows.len() <= 1000 && t.truncated || t.rows.len() == 1000,
        "{}",
        t.rows.len()
    );
    assert!(
        t.rows
            .iter()
            .all(|x| !x.name.is_empty() && !x.name.contains('/') && !x.name.contains('\u{1b}'))
    );
    assert!(
        t.rows
            .iter()
            .all(|x| x.depth <= 64 && x.status != Some('Z'))
    );
    assert_eq!(t.rows[0].status, Some('M'));
    assert!(!t.note.as_deref().unwrap().contains('\u{1b}'));
    let (lines, size, cut) = r.fs_read("a.txt").unwrap();
    assert!(
        lines.len() <= 2000 && lines.iter().all(|l| !l.contains('\u{1b}')) && size == 123 && cut
    );
    // an image goes in bounded chunks and is named in the send by the id the engine issued
    let img = synthesis_harness::ImageAttachment {
        media_type: "image/png".into(),
        data_base64: "QUJD".repeat(200_000),
    };
    e.send_images("look".into(), vec![img]).unwrap();
    assert!(matches!(next(&mut rx, "images"), UiMsg::BadEvent(m) if m == r#"images:["img-1"]"#));
    // a hostile size is refused by the client before it matters (the fake refuses a wrong first chunk too)
    let bad = r.image_put("image/png", &"QUJD".repeat(10));
    assert!(bad.is_err());
    // rewind: the dry run carries the engine's token, rows are printable; apply needs exactly that token
    let cps = r.rewind_list().unwrap();
    assert!(!cps[0].name.contains('\u{1b}') && cps[0].files == ["a.txt"]);
    let p = r.rewind_dry_run(None).unwrap();
    assert_eq!(p.token.as_deref(), Some("rw-1"));
    assert!(p.entries.iter().all(|x| !x.shown.contains('\u{1b}')));
    let o = r.rewind_apply("rw-1").unwrap();
    assert_eq!(
        (o.restored, o.removed, o.skipped, o.failed.len()),
        (1, 0, 1, 1)
    );
    assert!(!o.failed[0].contains('\u{1b}'));
    assert!(r.rewind_apply("rw-2").is_err());
    // clean: the plan's only "id" is the token
    let plans = r.clean_plan().unwrap();
    assert_eq!(plans.len(), 3);
    assert_eq!(plans[2].1.ids, ["cl-All"]);
    assert_eq!(
        (plans[0].1.counts.sessions, plans[0].1.counts.events),
        (2, 9)
    );
    assert_eq!(r.clean_apply("cl-All", false).unwrap().deleted, 2);
    // memory, mcp, doctor
    let m = r.memory_rows(None).unwrap();
    assert!(m[0].starts_with("m-1") && !m[0].contains('\u{1b}') && m[1].contains("1 record"));
    assert!(
        r.mcp_report()
            .unwrap()
            .iter()
            .all(|l| !l.contains('\u{1b}'))
    );
    let d = r.doctor_rows().unwrap();
    assert!(d.len() == 2 && d.iter().all(|x| !x.text.contains('\u{1b}')));
    assert!(
        std::sync::Arc::try_unwrap(e)
            .ok()
            .expect("sole owner")
            .close()
            .is_none()
    );
}

fn a_restart_moves_the_window_to_the_new_session_and_drops_the_old_lines() {
    let (e, mut old_rx) = start("ok").expect("starts");
    assert!(e.is_remote());
    let (e, mut rx, note, v) = e
        .restart_remote(synthesis_app::proto::Call::TrustSet {
            mode: synthesis_app::engine_methods::Mode::Session,
        })
        .expect("restarts");
    assert!(note.is_none());
    assert_eq!((e.sid.as_str(), v["sid"].as_str()), ("s2", Some("s2")));
    assert!(e.trusted);
    assert_eq!(e.mode(), Mode::Default);
    // the new window gets the new session's line and not the old session's trailing one
    assert!(matches!(next(&mut rx, "new event"), UiMsg::Event(ev) if ev.seq == 2));
    assert!(rx.try_recv().is_err());
    assert!(
        old_rx.try_recv().is_err(),
        "the old window hears nothing more"
    );
    assert!(e.close().is_none());
}

fn the_old_sessions_last_lines_are_dropped_not_shown_in_the_new_window() {
    let (e, _old) = start("stale").expect("starts");
    let (e, mut rx, _, _) = e
        .restart_remote(synthesis_app::proto::Call::SessionNew)
        .expect("restarts");
    // the first thing the new window hears is the new session's event: no old task end, notice or compaction
    assert!(matches!(next(&mut rx, "new event"), UiMsg::Event(ev) if ev.seq == 2));
    assert!(rx.try_recv().is_err());
    assert!(e.close().is_none());
}

fn only_a_marked_refusal_keeps_the_engine_everything_else_ends_it() {
    for mode in ["fatal", "unmarked"] {
        let (e, _old) = start(mode).expect("starts");
        let r = e.restart_remote(synthesis_app::proto::Call::SessionNew);
        assert!(r.is_err(), "{mode}: the engine is dropped, not kept");
    }
}

fn a_slow_engine_never_blocks_the_callers_of_rename_clear_and_compact() {
    let (e, mut rx) = start("slow").expect("starts");
    let e = std::sync::Arc::new(e);
    let t0 = Instant::now();
    e.rename_remote("x");
    e.clear_history();
    assert_eq!(e.compact(None), Ok(false));
    assert!(
        t0.elapsed() < Duration::from_millis(500),
        "returned at once"
    );
    // a call in flight keeps the engine from being restarted or closed under it
    assert!(synthesis_desktop::engine::in_use(&e));
    // the answers arrive later, as messages
    let mut renamed = false;
    let t1 = Instant::now();
    while !renamed {
        assert!(t1.elapsed() < Duration::from_secs(30), "no answer");
        if let UiMsg::Renamed(sid, Ok(t)) = next(&mut rx, "answer") {
            assert_eq!(sid, "s1", "the answer names the session it was asked for");
            renamed = t == "t";
        }
    }
}

fn a_busy_engine_is_kept_until_its_calls_ended() {
    let (e, mut rx) = start("slow").expect("starts");
    let e = std::sync::Arc::new(e);
    assert!(!synthesis_desktop::engine::in_use(&e));
    e.rename_remote("x");
    assert!(
        synthesis_desktop::engine::in_use(&e),
        "the call holds the engine"
    );
    // once it answered, the engine is free again
    while !matches!(next(&mut rx, "answer"), UiMsg::Renamed(..)) {}
    let t0 = Instant::now();
    while synthesis_desktop::engine::in_use(&e) {
        assert!(t0.elapsed() < Duration::from_secs(10), "still held");
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn a_refused_restart_leaves_the_session_running() {
    let (e, _old) = start("busy").expect("starts");
    let (e, mut rx, note, _) = e
        .restart_remote(synthesis_app::proto::Call::SessionNew)
        .expect("not fatal");
    assert!(note.unwrap().contains("busy"));
    assert_eq!(e.sid, "s1");
    // still the same engine, and its events reach the new window
    e.send("go".into()).unwrap();
    assert!(matches!(next(&mut rx, "event"), UiMsg::Event(_)));
    drop(e);
}

fn a_restart_the_client_did_not_ask_for_kills_the_engine() {
    killed_after_send("forged-restart", None);
}

fn a_dead_engine_ends_the_task_instead_of_hanging() {
    let (e, mut rx) = start("die-after-send").expect("starts");
    e.send("go".into()).unwrap();
    let mut done = None;
    let t0 = Instant::now();
    while done.is_none() {
        assert!(t0.elapsed() < Duration::from_secs(30), "no TaskDone");
        if let UiMsg::TaskDone(r) = next(&mut rx, "done") {
            done = Some(r);
        }
    }
    assert!(done.unwrap().is_err());
    assert!(!e.running());
    assert!(e.send("more".into()).is_err());
    drop(e);
}

fn a_misbehaving_engine_is_cut_off() {
    killed_after_send("event-flood", None);
    killed_after_send("ask-flood", None);
    killed_after_send("hello-flood", None);
    // silent while a task runs: the watchdog ends it
    killed_after_send("hang", Some(Duration::from_secs(1)));
}

fn a_chatty_stderr_does_not_stall_the_engine() {
    let (e, _rx) = start("chatty").expect("starts");
    assert!(e.close().is_none());
}

/// The whole WSL path with a fake `wsl.exe` (this binary, re-run with wsl.exe's arguments) that checks
/// them, then acts as the engine, and a table standing in for what `wsl.exe` prints.
fn a_session_through_a_fake_wsl() {
    let utf16 = |s: &str| -> Vec<u8> {
        let mut v = vec![0xFF, 0xFE];
        v.extend(s.encode_utf16().flat_map(u16::to_le_bytes));
        v
    };
    let seen = std::sync::Mutex::new(vec![]);
    let run = |a: &[&str], _: Duration| {
        seen.lock().unwrap().push(a.join(" "));
        let out = |b: Vec<u8>| {
            Ok(wsl::Out {
                ok: true,
                stdout: b,
            })
        };
        match a {
            ["--status"] => out(vec![]),
            ["-l", "-v"] => out(utf16("  NAME STATE VERSION\r\n* Ubuntu Stopped 2\r\n")),
            ["-d", "Ubuntu", "--exec", "/bin/sh", ..] => out(b"/home/me/.synt/bin/synt\n".to_vec()),
            [
                "-d",
                "Ubuntu",
                "--exec",
                "/home/me/.synt/bin/synt",
                "--version",
            ] => out(format!("synt {VERSION}\n").into_bytes()),
            ["-d", "Ubuntu", "--exec", "/usr/bin/wslpath", "-u", p] => out(format!(
                "/mnt/c{}\n",
                p.trim_start_matches("C:").replace('\\', "/")
            )
            .into_bytes()),
            _ => Err(format!("unexpected wsl.exe call {a:?}")),
        }
    };
    let exe = std::env::current_exe().unwrap();
    let env = Env::from_pairs([("SYNT_API_KEY", "must-not-travel")]);
    for (folder, linux) in [
        (r"C:\work\repo", "/mnt/c/work/repo"),
        (r"\\wsl.localhost\ubuntu\home\me\repo", "/home/me/repo"),
    ] {
        let (e, mut rx) = wsl::start_via(
            &run,
            &exe,
            "",
            VERSION,
            &env,
            Path::new(folder),
            &Default::default(),
            wsl::CostCap::Usd(10.0),
        )
        .unwrap_or_else(|e| panic!("{folder}: {e}"));
        // the engine got the Linux folder twice (--cd and start); the host kept the Windows one
        assert_eq!(e.remote_cwd.as_deref(), Some(linux), "--cd");
        assert_eq!(e.remote_root.as_deref(), Some(linux), "start");
        assert_eq!(e.cwd, folder);
        e.send("go".into()).unwrap();
        assert!(matches!(next(&mut rx, "event"), UiMsg::Event(_)));
        assert!(e.close().is_none());
        // a restart that comes back in another folder is refused: the engine is dropped, never used
        let (idle, _rx) = wsl::start_via(
            &run,
            &exe,
            "",
            VERSION,
            &env,
            Path::new(folder),
            &Default::default(),
            wsl::CostCap::Usd(10.0),
        )
        .unwrap();
        let drift = wsl::restart(
            idle,
            synthesis_app::proto::Call::ModelSet {
                model: "a/b".into(),
            },
        );
        assert!(drift.err().unwrap().contains("reports its folder"));
    }
    assert!(
        seen.lock()
            .unwrap()
            .iter()
            .any(|c| c.starts_with("-d Ubuntu --exec /usr/bin/wslpath -u C:"))
    );
    // another distro's folder is refused before any engine starts
    let e = wsl::start_via(
        &run,
        &exe,
        "",
        VERSION,
        &env,
        Path::new(r"\\wsl$\Debian\x"),
        &Default::default(),
        wsl::CostCap::Usd(10.0),
    );
    assert!(e.err().unwrap().contains("Debian"));
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    // a fake wsl.exe: the shape of the real command line is checked, then it is the engine
    if args.get(1).map(String::as_str) == Some("-d") {
        let ok = args.len() == 13
            && (args[3].as_str(), args[5].as_str()) == ("--cd", "--exec")
            && args[6..]
                == [
                    "/home/me/.synt/bin/synt",
                    "engine",
                    "--stdio",
                    "--host",
                    "desktop",
                    "--max-cost-usd",
                    "10",
                ];
        if !ok || std::env::var_os("SYNT_API_KEY").is_some() || std::env::var_os("WSLENV").is_some()
        {
            eprintln!("fake wsl.exe: bad command line or environment: {args:?}");
            std::process::exit(2);
        }
        return fake_engine("wsl");
    }
    if let Some(i) = args.iter().position(|a| a == "--fake-engine") {
        return fake_engine(&args[i + 1]);
    }
    let tests: [(&str, fn()); 17] = [
        (
            "files_images_rewind_clean_and_settings_are_parsed_bounded_and_made_printable",
            files_images_rewind_clean_and_settings_are_parsed_bounded_and_made_printable,
        ),
        (
            "the_store_methods_are_parsed_bounded_and_made_printable",
            the_store_methods_are_parsed_bounded_and_made_printable,
        ),
        (
            "a_restart_moves_the_window_to_the_new_session_and_drops_the_old_lines",
            a_restart_moves_the_window_to_the_new_session_and_drops_the_old_lines,
        ),
        (
            "the_old_sessions_last_lines_are_dropped_not_shown_in_the_new_window",
            the_old_sessions_last_lines_are_dropped_not_shown_in_the_new_window,
        ),
        (
            "only_a_marked_refusal_keeps_the_engine_everything_else_ends_it",
            only_a_marked_refusal_keeps_the_engine_everything_else_ends_it,
        ),
        (
            "a_slow_engine_never_blocks_the_callers_of_rename_clear_and_compact",
            a_slow_engine_never_blocks_the_callers_of_rename_clear_and_compact,
        ),
        (
            "a_busy_engine_is_kept_until_its_calls_ended",
            a_busy_engine_is_kept_until_its_calls_ended,
        ),
        (
            "a_refused_restart_leaves_the_session_running",
            a_refused_restart_leaves_the_session_running,
        ),
        (
            "a_restart_the_client_did_not_ask_for_kills_the_engine",
            a_restart_the_client_did_not_ask_for_kills_the_engine,
        ),
        ("mismatch_is_refused", mismatch_is_refused),
        (
            "exit_before_hello_is_an_error",
            exit_before_hello_is_an_error,
        ),
        (
            "a_flooding_engine_is_killed_not_waited_for",
            a_flooding_engine_is_killed_not_waited_for,
        ),
        ("a_session_over_the_pipe", a_session_over_the_pipe),
        (
            "a_dead_engine_ends_the_task_instead_of_hanging",
            a_dead_engine_ends_the_task_instead_of_hanging,
        ),
        (
            "a_misbehaving_engine_is_cut_off",
            a_misbehaving_engine_is_cut_off,
        ),
        (
            "a_chatty_stderr_does_not_stall_the_engine",
            a_chatty_stderr_does_not_stall_the_engine,
        ),
        ("a_session_through_a_fake_wsl", a_session_through_a_fake_wsl),
    ];
    for (name, t) in tests {
        t();
        println!("test {name} ... ok");
    }
}
