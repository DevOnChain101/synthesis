//! `synt engine --stdio` end to end through the real binary with a scripted model (feature
//! `test-provider`): hello, start, a send that streams events, an approval ask and its answer,
//! cancel, bad input, and shutdown on stdin EOF. No WSL, no terminal, nothing from the host's home.
#![cfg(feature = "test-provider")]

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{Receiver, RecvTimeoutError, channel};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

const WAIT: Duration = Duration::from_secs(60);

struct Engine {
    child: Child,
    stdin: Option<ChildStdin>,
    lines: Receiver<String>,
    ws: PathBuf,
    home: PathBuf,
    next: u64,
    _dir: tempfile::TempDir,
}

impl Drop for Engine {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Starts the engine (not the session) on a script of model turns (given the workspace folder).
fn engine(script: impl FnOnce(&Path) -> Value) -> Engine {
    engine_with(&["--no-cost-cap"], None, false, script)
}

/// [`engine`] with its own flags, optional user settings, and optionally a git repository as the
/// workspace (one commit holding `a.txt`).
fn engine_with(
    flags: &[&str],
    settings: Option<Value>,
    git: bool,
    script: impl FnOnce(&Path) -> Value,
) -> Engine {
    let dir = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(dir.path()).unwrap();
    let (ws, home) = (root.join("ws"), root.join("home"));
    std::fs::create_dir_all(&ws).unwrap();
    std::fs::create_dir_all(&home).unwrap();
    if let Some(v) = settings {
        std::fs::write(home.join("settings.json"), v.to_string()).unwrap();
    }
    if git {
        std::fs::write(ws.join("a.txt"), "hello\n").unwrap();
        for args in [
            &["init", "-q"][..],
            &["add", "."],
            &[
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "commit",
                "-q",
                "-m",
                "init",
            ],
        ] {
            assert!(
                Command::new("git")
                    .args(args)
                    .current_dir(&ws)
                    .status()
                    .unwrap()
                    .success()
            );
        }
    }
    let script_path = root.join("script.json");
    std::fs::write(&script_path, script(&ws).to_string()).unwrap();
    let mut c = Command::new(env!("CARGO_BIN_EXE_synt"));
    c.args(["engine", "--stdio"])
        .args(flags)
        .args(["--sandbox", "off", "--provider"])
        .arg(format!("fake:{}", script_path.display()))
        .env_clear();
    for k in [
        "PATH",
        "SystemRoot",
        "PATHEXT",
        "ComSpec",
        "USERPROFILE",
        "TEMP",
        "TMP",
    ] {
        if let Some(v) = std::env::var_os(k) {
            c.env(k, v);
        }
    }
    let mut child = c
        .env("HOME", &root)
        .env("SYNT_HOME", &home)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let stdin = child.stdin.take();
    let out = child.stdout.take().unwrap();
    let (tx, lines) = channel();
    std::thread::spawn(move || {
        for l in BufReader::new(out).lines() {
            let Ok(l) = l else { break };
            if tx.send(l).is_err() {
                break;
            }
        }
    });
    Engine {
        child,
        stdin,
        lines,
        ws,
        home,
        next: 1,
        _dir: dir,
    }
}

impl Engine {
    /// The next line of stdout; it must be one JSON object (stdout carries protocol lines only).
    fn line(&self) -> Option<Value> {
        match self.lines.recv_timeout(WAIT) {
            Ok(l) => {
                Some(serde_json::from_str(&l).unwrap_or_else(|e| panic!("not JSON ({e}): {l}")))
            }
            Err(RecvTimeoutError::Disconnected) => None,
            Err(RecvTimeoutError::Timeout) => panic!("timed out waiting for the engine"),
        }
    }

    fn raw(&mut self, text: &str) {
        let w = self.stdin.as_mut().unwrap();
        writeln!(w, "{text}").unwrap();
        w.flush().unwrap();
    }

    fn call(&mut self, method: &str, params: Value) -> u64 {
        let id = self.next;
        self.next += 1;
        let mut req = json!({"id": id, "method": method});
        if !params.is_null() {
            req["params"] = params;
        }
        self.raw(&req.to_string());
        id
    }

    /// Reads lines until one satisfies `stop`; returns everything read, the match last.
    fn until(&self, what: &str, stop: impl Fn(&Value) -> bool) -> Vec<Value> {
        let t0 = Instant::now();
        let mut seen = vec![];
        while t0.elapsed() < WAIT {
            let v = self
                .line()
                .unwrap_or_else(|| panic!("engine ended before: {what}"));
            let done = stop(&v);
            seen.push(v);
            if done {
                return seen;
            }
        }
        panic!("timed out: {what}");
    }

    fn result(&self, id: u64) -> (Vec<Value>, Value) {
        let mut seen = self.until("result", |v| v["msg"] == "result" && v["id"] == id);
        let r = seen.pop().unwrap();
        (seen, r)
    }

    fn hello(&self) {
        let h = self.line().unwrap();
        assert_eq!(h["msg"], "hello");
        assert_eq!(h["version"], env!("CARGO_PKG_VERSION"));
        assert_eq!(h["protocol"], 1);
    }

    fn start(&mut self) -> Value {
        self.hello();
        let ws = self.ws.to_str().unwrap().to_string();
        let id = self.call("start", json!({"cwd": ws}));
        let (_, r) = self.result(id);
        assert!(r["error"].is_null(), "{r}");
        r["ok"].clone()
    }
}

fn is_task_done(v: &Value) -> bool {
    v["msg"] == "task_done"
}

/// A model that writes `hello.txt` in the workspace, then finishes.
fn write_script(ws: &Path) -> Value {
    let target = ws.join("hello.txt");
    json!([
        {"text": "Writing the file.", "tools": [
            {"id": "w1", "name": "Write", "input": {"file_path": target, "content": "hi\n"}}
        ]},
        {"text": "Finished."}
    ])
}

#[test]
fn hello_start_and_a_send_that_streams_events_then_a_result() {
    let mut e = engine(|_| json!([{"text": "Hello from the model."}]));
    let info = e.start();
    assert!(!info["sid"].as_str().unwrap().is_empty());
    assert!(info["cwd"].as_str().is_some());
    let id = e.call("send", json!({"text": "say hi"}));
    let seen = e.until("task_done", is_task_done);
    assert!(
        seen.iter()
            .any(|v| v["msg"] == "result" && v["id"] == id && v["ok"].is_object())
    );
    // events are the ordinary envelopes: a `type` and no `msg`
    let events: Vec<&Value> = seen.iter().filter(|v| v["msg"].is_null()).collect();
    assert!(
        events
            .iter()
            .all(|v| v["type"].is_string() && v["session_id"] == info["sid"])
    );
    assert!(
        events.iter().any(|v| v["type"] == "task.started"),
        "{events:?}"
    );
    assert!(seen.last().unwrap()["error"].is_null());
}

#[test]
fn an_approval_ask_waits_for_the_answer_by_id() {
    let mut e = engine(write_script);
    let target = e.ws.join("hello.txt");
    e.start();
    e.call("send", json!({"text": "write it"}));
    let ask = e
        .until("ask", |v| v["msg"] == "ask.approval")
        .pop()
        .unwrap();
    assert_eq!(ask["tool"], "Write");
    assert!(!target.exists(), "nothing is written before the answer");
    // an unknown id is not accepted and answers nothing
    let id = e.call("answer", json!({"id": "nope", "choice": "once"}));
    assert_eq!(e.result(id).1["ok"]["accepted"], false);
    assert!(!target.exists());
    let id = e.call("answer", json!({"id": ask["id"], "choice": "once"}));
    assert_eq!(e.result(id).1["ok"]["accepted"], true);
    let seen = e.until("task_done", is_task_done);
    assert!(seen.last().unwrap()["error"].is_null());
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "hi\n");
}

#[test]
fn a_denied_ask_writes_nothing() {
    let mut e = engine(write_script);
    let target = e.ws.join("hello.txt");
    e.start();
    e.call("send", json!({"text": "write it"}));
    let ask = e
        .until("ask", |v| v["msg"] == "ask.approval")
        .pop()
        .unwrap();
    e.call("answer", json!({"id": ask["id"], "choice": "deny"}));
    e.until("task_done", is_task_done);
    assert!(!target.exists());
}

#[test]
fn cancel_withdraws_a_waiting_ask_and_never_approves_it() {
    let mut e = engine(write_script);
    let target = e.ws.join("hello.txt");
    e.start();
    e.call("send", json!({"text": "write it"}));
    let ask = e
        .until("ask", |v| v["msg"] == "ask.approval")
        .pop()
        .unwrap();
    e.call("cancel", Value::Null);
    let seen = e.until("task_done", is_task_done);
    assert!(
        seen.iter()
            .any(|v| v["msg"] == "withdraw" && v["id"] == ask["id"]),
        "{seen:?}"
    );
    assert!(!target.exists());
}

#[test]
fn bad_input_is_survived_and_a_second_start_is_refused() {
    let mut e = engine(|_| json!([{"text": "ok"}]));
    e.hello();
    for bad in [
        "not json",
        "{\"id\":1}",
        "{\"id\":2,\"method\":\"rm_rf\"}",
        "",
    ] {
        e.raw(bad);
    }
    let ws = e.ws.to_str().unwrap().to_string();
    // a relative or missing folder is refused with an error result, not a crash
    let id = e.call("start", json!({"cwd": "relative/dir"}));
    let (_, r) = e.result(id);
    assert!(r["error"].as_str().unwrap().contains("cwd"), "{r}");
    let id = e.call("send", json!({"text": "too early"}));
    let (_, r) = e.result(id);
    assert!(r["error"].as_str().unwrap().contains("start"), "{r}");
    let id = e.call("start", json!({"cwd": ws}));
    assert!(e.result(id).1["error"].is_null());
    let id = e.call("start", json!({"cwd": e.ws.to_str().unwrap()}));
    assert_eq!(e.result(id).1["error"], "already started");
}

#[test]
fn close_answers_then_the_process_exits() {
    let mut e = engine(|_| json!([{"text": "ok"}]));
    e.start();
    let id = e.call("close", Value::Null);
    let (_, r) = e.result(id);
    assert!(r["error"].is_null());
    drop(e.stdin.take());
    assert!(e.child.wait().unwrap().success());
}

#[test]
fn stdin_eof_ends_the_session_and_the_process() {
    let mut e = engine(|_| json!([{"text": "ok"}]));
    e.start();
    drop(e.stdin.take());
    let t0 = Instant::now();
    let status = loop {
        if let Some(s) = e.child.try_wait().unwrap() {
            break s;
        }
        assert!(t0.elapsed() < WAIT, "the engine did not exit on EOF");
        std::thread::sleep(Duration::from_millis(20));
    };
    assert!(status.success());
    // what is left on stdout (the end of the session) is still protocol lines only
    while e.line().is_some() {}
}

#[test]
fn the_mode_is_reported_and_changes_but_never_to_bypass() {
    let mut e = engine(|_| json!([{"text": "ok"}]));
    let info = e.start();
    assert_eq!(info["mode"], "default");
    assert_eq!(info["trusted"], false);
    let id = e.call("set_mode", json!({"mode": "plan"}));
    assert_eq!(e.result(id).1["ok"]["mode"], "plan");
    for bad in ["bypassPermissions", "nonsense"] {
        let id = e.call("set_mode", json!({"mode": bad}));
        assert!(e.result(id).1["error"].is_string(), "{bad}");
    }
}

#[test]
fn project_trust_flags_are_refused() {
    for flag in [["--trust-project"], ["--approve-mcp"]] {
        let mut c = Command::new(env!("CARGO_BIN_EXE_synt"));
        c.args(["engine", "--stdio"]).args(flag);
        if flag[0] == "--approve-mcp" {
            c.arg("db");
        }
        let o = c.stdin(Stdio::null()).output().unwrap();
        assert!(!o.status.success());
        assert!(o.stdout.is_empty(), "no protocol line before the refusal");
        assert!(String::from_utf8_lossy(&o.stderr).contains("not supported"));
    }
}

fn priced() -> Value {
    json!({"synt": {"pricing": {"fake/fake-model": {"input_per_mtok": 1.0, "output_per_mtok": 1.0}}}})
}

fn hello_txt_script(_ws: &Path) -> Value {
    // relative paths: the session's folder is whatever the engine chose (the opened folder or a worktree)
    json!([
        {"tools": [{"id": "r1", "name": "Read", "input": {"file_path": "a.txt"}}]},
        {"tools": [{"id": "e1", "name": "Edit", "input": {"file_path": "a.txt", "old_string": "hello", "new_string": "bye"}}]},
        {"text": "Done."}
    ])
}

/// What `synt engine --stdio` does when a desktop launches it: the edit lands in the folder that was
/// opened (no worktree under ~/.synt), with a cost cap passed by the host.
#[test]
fn a_desktop_hosted_engine_edits_the_opened_folder_not_a_worktree() {
    let mut e = engine_with(
        &[
            "--host",
            "desktop",
            "--max-cost-usd",
            "5",
            "--permission-mode",
            "acceptEdits",
        ],
        Some(priced()),
        true,
        hello_txt_script,
    );
    e.start();
    e.call("send", json!({"text": "change it"}));
    e.until("task_done", is_task_done);
    assert_eq!(
        std::fs::read_to_string(e.ws.join("a.txt")).unwrap(),
        "bye\n"
    );
    assert!(
        !e.home.join("worktrees").exists(),
        "no worktree for a desktop host"
    );
}

/// Without `--host` the engine is a headless worker: a worktree, and the file in the folder stays.
#[test]
fn a_headless_engine_without_a_host_uses_a_worktree() {
    let mut e = engine_with(
        &["--no-cost-cap", "--permission-mode", "acceptEdits"],
        None,
        true,
        hello_txt_script,
    );
    e.start();
    e.call("send", json!({"text": "change it"}));
    e.until("task_done", is_task_done);
    assert_eq!(
        std::fs::read_to_string(e.ws.join("a.txt")).unwrap(),
        "hello\n"
    );
    assert!(e.home.join("worktrees").is_dir());
}

#[test]
fn a_headless_engine_without_a_cost_cap_refuses_to_start_a_session() {
    let mut e = engine_with(&[], None, false, |_| json!([{"text": "x"}]));
    e.hello();
    let ws = e.ws.to_str().unwrap().to_string();
    let id = e.call("start", json!({"cwd": ws}));
    let (_, r) = e.result(id);
    assert!(
        r["error"].as_str().is_some_and(|e| e.contains("cost cap")),
        "{r}"
    );
}

/// `synt stop` from another process reaches the engine: the running task is cancelled.
#[test]
fn synt_stop_cancels_a_task_of_a_running_engine() {
    let mut e = engine_with(
        &["--no-cost-cap"],
        None,
        false,
        |_| json!([{"delay_ms": 60000, "text": "never"}]),
    );
    let info = e.start();
    let sid = info["sid"].as_str().unwrap().to_string();
    e.call("send", json!({"text": "slow"}));
    std::thread::sleep(Duration::from_millis(1500));
    let o = Command::new(env!("CARGO_BIN_EXE_synt"))
        .args(["stop", &sid])
        .env_clear()
        .env("HOME", e.home.parent().unwrap())
        .env("SYNT_HOME", &e.home)
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .output()
        .unwrap();
    assert_eq!(
        o.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&o.stderr)
    );
    let seen = e.until("task_done", is_task_done);
    let ended = seen
        .iter()
        .find(|v| v["type"] == "task.ended")
        .expect("task.ended");
    assert_eq!(ended["payload"]["status"], "cancelled", "{ended}");
}

/// A session nobody holds cannot be stopped, and saying so is not a success.
#[test]
fn synt_stop_on_a_session_no_process_holds_is_an_error_exit() {
    let mut e = engine_with(&["--no-cost-cap"], None, false, |_| json!([{"text": "x"}]));
    let info = e.start();
    let sid = info["sid"].as_str().unwrap().to_string();
    e.call("close", Value::Null);
    let _ = e.child.wait();
    let o = Command::new(env!("CARGO_BIN_EXE_synt"))
        .args(["stop", &sid])
        .env_clear()
        .env("HOME", e.home.parent().unwrap())
        .env("SYNT_HOME", &e.home)
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .output()
        .unwrap();
    assert_eq!(o.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&o.stderr).contains("no live process holds"));
}
