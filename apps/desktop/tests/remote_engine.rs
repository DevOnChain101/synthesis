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
                println!("{EVENT}");
                match mode {
                    "die-after-send" => std::process::exit(3),
                    "hang" => std::thread::sleep(Duration::from_secs(600)),
                    "event-flood" => (0..20_000).for_each(|_| println!("{EVENT}")),
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
    Engine::start_remote(l, &std::env::temp_dir())
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
    assert!(!e.trusted, "an engine cannot make a folder trusted");
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
        let (e, mut rx) = wsl::start_via(&run, &exe, "", VERSION, &env, Path::new(folder))
            .unwrap_or_else(|e| panic!("{folder}: {e}"));
        // the engine got the Linux folder twice (--cd and start); the host kept the Windows one
        assert_eq!(e.remote_cwd.as_deref(), Some(linux), "--cd");
        assert_eq!(e.remote_root.as_deref(), Some(linux), "start");
        assert_eq!(e.cwd, folder);
        e.send("go".into()).unwrap();
        assert!(matches!(next(&mut rx, "event"), UiMsg::Event(_)));
        assert!(e.close().is_none());
    }
    assert!(
        seen.lock()
            .unwrap()
            .iter()
            .any(|c| c.starts_with("-d Ubuntu --exec /usr/bin/wslpath -u C:"))
    );
    // another distro's folder is refused before any engine starts
    let e = wsl::start_via(&run, &exe, "", VERSION, &env, Path::new(r"\\wsl$\Debian\x"));
    assert!(e.err().unwrap().contains("Debian"));
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    // a fake wsl.exe: the shape of the real command line is checked, then it is the engine
    if args.get(1).map(String::as_str) == Some("-d") {
        let ok = args.len() == 10
            && (args[3].as_str(), args[5].as_str()) == ("--cd", "--exec")
            && args[6..]
                == [
                    "/home/me/.synt/bin/synt",
                    "engine",
                    "--stdio",
                    "--no-cost-cap",
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
    let tests: [(&str, fn()); 8] = [
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
