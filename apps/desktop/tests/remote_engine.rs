//! The remote engine client (`Engine::start_remote`) against a fake engine process: this same test
//! binary re-run with `--fake-engine MODE` (so it needs no WSL, no `synt` build and runs on every OS).
//! `harness = false`: the fake engine's stdout must carry protocol lines only.

use std::io::{BufRead, Write};
use std::time::{Duration, Instant};

use futures::channel::mpsc::UnboundedReceiver;
use serde_json::{Value, json};
use synthesis_app::proto::{PROTOCOL, VERSION};
use synthesis_desktop::engine::{Choice, Engine, NOT_REMOTE, UiMsg};
use synthesis_desktop::remote::Launcher;

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
            "start" => say(
                json!({"msg":"result","id":id,"ok":{"sid":"s1","label":"fake/model","models":["fake/model"],"cwd":req["params"]["cwd"],"root":req["params"]["cwd"],"notices":["n\u{1b}[31m1"]}}),
            ),
            "send" => {
                say(json!({"msg":"result","id":id,"ok":{}}));
                println!("{EVENT}");
                if mode == "die-after-send" {
                    std::process::exit(3);
                }
                say(
                    json!({"msg":"ask.approval","id":"a1","agent_id":null,"tool":"Bash","input":{},"dialog":"permission","prompt":"p","reason":"r","origin":null,"suggestions":[]}),
                );
            }
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
    Launcher {
        program: std::env::current_exe().unwrap().into(),
        args: vec!["--fake-engine".into(), mode.into()],
    }
}

fn start(mode: &str) -> Result<(Engine, UnboundedReceiver<UiMsg>), String> {
    let dir = std::env::temp_dir();
    Engine::start_remote(&launcher(mode), &dir)
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
    assert_eq!(e.compact(None), Err(NOT_REMOTE));
    assert!(e.rename("x").is_err());
    assert!(e.close().is_none());
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

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if let Some(i) = args.iter().position(|a| a == "--fake-engine") {
        return fake_engine(&args[i + 1]);
    }
    let tests: [(&str, fn()); 5] = [
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
    ];
    for (name, t) in tests {
        t();
        println!("test {name} ... ok");
    }
}
