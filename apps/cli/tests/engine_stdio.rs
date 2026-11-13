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

/// The engine as the desktop starts it: `--host desktop` (a person at the window: the interactive behaviour,
/// no worktree, no required cost cap).
fn desktop_engine(script: impl FnOnce(&Path) -> Value) -> Engine {
    engine_with(&["--host", "desktop"], None, false, script)
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
        assert_eq!(h["protocol"], 3);
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
    // the two with a usable id are answered with an error (the client does not wait out a timeout)
    for want in [1, 2] {
        let r = e.line().unwrap();
        assert!(
            r["msg"] == "result" && r["id"] == want && r["error"].is_string(),
            "{r}"
        );
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

/// Restarts are limited to one a second.
fn gap() {
    std::thread::sleep(Duration::from_millis(1100));
}

impl Engine {
    /// A request and its result (events and notices in between are skipped).
    fn ask(&mut self, method: &str, params: Value) -> Value {
        let id = self.call(method, params);
        self.result(id).1
    }

    fn ok(&mut self, method: &str, params: Value) -> Value {
        let r = self.ask(method, params);
        assert!(r["error"].is_null(), "{method}: {r}");
        r["ok"].clone()
    }

    fn err(&mut self, method: &str, params: Value) -> String {
        let r = self.ask(method, params);
        r["error"]
            .as_str()
            .unwrap_or_else(|| panic!("{method} should fail: {r}"))
            .to_string()
    }
}

/// An engine that already ran one task (so the store holds a session with messages and events).
fn used_engine() -> (Engine, Value) {
    let mut e = desktop_engine(|_| json!([{"text": "one"}, {"text": "two"}, {"text": "three"}]));
    let info = e.start();
    e.call("send", json!({"text": "first"}));
    e.until("task_done", is_task_done);
    (e, info)
}

#[test]
fn the_store_methods_round_trip() {
    let (mut e, info) = used_engine();
    let sid = info["sid"].as_str().unwrap().to_string();
    let ws = e.ok("workspaces.list", Value::Null);
    let rows = ws["workspaces"].as_array().unwrap();
    let row = rows
        .iter()
        .find(|w| {
            w["sessions"]
                .as_array()
                .is_some_and(|s| s.iter().any(|s| s["id"] == sid.as_str()))
        })
        .unwrap_or_else(|| panic!("the session is listed: {ws}"));
    let key = row["key"].as_str().unwrap().to_string();
    let list = e.ok("sessions.list", json!({"workspace": key}));
    assert_eq!(list["sessions"][0]["id"], sid.as_str());
    let p = e.ok("session.preview", json!({"id": sid}));
    let evs = p["events"].as_array().unwrap();
    assert!(evs.iter().any(|v| v["type"] == "task.started"), "{p}");
    assert_eq!(p["truncated"], false);
    let u = e.ok("usage.summary", json!({"tz_min": 0}));
    assert!(u["days"].is_array());
    assert!(e.ok("session.rename", json!({"id": sid, "title": "my title"}))["title"] == "my title");
    let ws = e.ok("workspaces.list", Value::Null);
    assert!(ws.to_string().contains("my title"));
}

#[test]
fn hostile_parameters_are_refused_and_leak_no_host_path() {
    let (mut e, info) = used_engine();
    let sid = info["sid"].as_str().unwrap().to_string();
    let root = e.ws.parent().unwrap().to_str().unwrap().to_string();
    let long = "a".repeat(5000);
    let mut errors = vec![];
    for (m, p) in [
        ("session.preview", json!({"id": "../../etc/passwd"})),
        ("session.preview", json!({"id": ""})),
        ("session.preview", json!({"id": long})),
        ("session.preview", json!({"id": "nope"})),
        ("session.preview", json!({"id": "a\nb"})),
        ("sessions.list", json!({"workspace": "/etc"})),
        ("sessions.list", json!({"workspace": "x\u{0}y"})),
        ("sessions.list", json!({"workspace": long})),
        ("usage.summary", json!({"tz_min": 100_000})),
        ("session.resume", json!({"id": "../x"})),
        ("session.resume", json!({"id": "nope"})),
        ("session.close", json!({"id": "nope"})),
        ("session.rename", json!({"id": "nope", "title": "t"})),
        (
            "session.rename",
            json!({"id": sid, "title": "x".repeat(500)}),
        ),
        (
            "session.rename",
            json!({"id": sid, "title": "bad\u{202e}title"}),
        ),
        ("model.set", json!({"model": "--sandbox=off"})),
        ("model.set", json!({"model": ""})),
        ("trust.set", json!({"mode": "always"})),
        ("trust.set", json!({"mode": "bypass"})),
        ("trust.revoke", json!({"root": "/"})),
        ("trust.revoke", json!({"root": "/etc"})),
        ("workspace.close", json!({"workspace": ""})),
        ("compact", json!({"instructions": "i".repeat(20_000)})),
        ("session.preview", json!({})),
        ("session.preview", json!({"id": 5})),
    ] {
        let r = e.ask(m, p.clone());
        let t = r["error"]
            .as_str()
            .unwrap_or_else(|| panic!("{m} {p} should fail: {r}"));
        errors.push(t.to_string());
    }
    assert!(errors.iter().all(|t| !t.contains(&root)), "{errors:?}");
    // the session is untouched by all of that
    assert_eq!(e.ok("set_mode", json!({"mode": "plan"}))["mode"], "plan");
}

impl Engine {
    /// The results of all of `ids`, whatever order they come in (the reader answers some by itself).
    fn results(&self, ids: &[u64]) -> std::collections::HashMap<u64, Value> {
        let mut got = std::collections::HashMap::new();
        while got.len() < ids.len() {
            let v = self.line().expect("the engine ended");
            if v["msg"] == "result"
                && let Some(id) = v["id"].as_u64()
                && ids.contains(&id)
            {
                got.insert(id, v);
            }
        }
        got
    }
}

#[test]
fn a_flood_beyond_the_queue_is_refused_not_waited_for() {
    let (mut e, _) = used_engine();
    let ids: Vec<u64> = (0..300)
        .map(|_| e.call("usage.summary", json!({"tz_min": 0})))
        .collect();
    let got = e.results(&ids);
    assert!(got.values().any(|v| {
        v["error"]
            .as_str()
            .is_some_and(|t| t.contains("too many requests"))
    }));
    assert!(got.values().any(|v| v["error"].is_null()));
}

#[test]
fn bypass_and_plan_cannot_be_lifted_through_the_new_methods() {
    let (mut e, _) = used_engine();
    assert_eq!(e.ok("set_mode", json!({"mode": "plan"}))["mode"], "plan");
    // a restart returns to the default mode (never to bypass, never keeping a client-chosen one)
    let v = e.ok("session.new", Value::Null);
    assert_ne!(v["mode"], "bypassPermissions");
    assert_eq!(v["trusted"], false);
}

#[test]
fn session_new_and_resume_restart_in_place_in_the_same_folder() {
    let (mut e, info) = used_engine();
    let old = info["sid"].as_str().unwrap().to_string();
    let v = e.ok("session.new", Value::Null);
    let new = v["sid"].as_str().unwrap().to_string();
    assert_ne!(new, old);
    // a second restart within a second is refused, nothing is ended by the refusal
    assert!(
        e.err("session.resume", json!({"id": old}))
            .contains("too many restarts")
    );
    gap();
    let v = e.ok("session.resume", json!({"id": old}));
    assert_eq!(v["sid"], old.as_str());
    gap();
    // a session id that exists but is the engine's own empty one of another start cannot be closed by a stranger id
    assert!(e.err("session.close", json!({"id": new})).contains("own"));
}

#[test]
fn trust_is_decided_by_the_engine_and_only_for_the_exact_root() {
    let mut e = desktop_engine(|_| json!([{"text": "ok"}]));
    std::fs::create_dir_all(e.ws.join(".synt")).unwrap();
    std::fs::write(e.ws.join(".synt/settings.json"), "{}").unwrap();
    let info = e.start();
    assert_eq!(info["trusted"], false);
    let st = e.ok("trust.state", Value::Null);
    assert_eq!(st["trusted"], false);
    assert_eq!(st["remembered"], "no");
    assert!(st["refusal"].is_null(), "{st}");
    // Not now / deny changes nothing and restarts nothing
    let d = e.ok("trust.set", json!({"mode": "deny"}));
    assert_eq!(
        (d["trusted"].as_bool(), d["restarted"].as_bool()),
        (Some(false), Some(false))
    );
    // session-only: restarted trusted, grant for exactly this root
    let v = e.ok("trust.set", json!({"mode": "session"}));
    assert_eq!(v["trusted"], true, "{v}");
    let st = e.ok("trust.state", Value::Null);
    assert_eq!(st["trusted"], true);
    assert_eq!(st["session_grant"], st["root"]);
    assert_eq!(
        st["records"].as_array().unwrap().len(),
        0,
        "session-only writes nothing"
    );
    // revoke accepts only a root this engine listed (or its own grant), never a path
    assert!(
        e.err("trust.revoke", json!({"root": "/etc"}))
            .contains("not remembered")
    );
    let r = e.ok("trust.revoke", json!({"root": st["root"]}));
    assert_eq!(r["session_grant_ended"], true);
}

#[test]
fn remember_is_stored_with_the_engines_own_key_and_revoked() {
    let mut e = desktop_engine(|_| json!([{"text": "ok"}]));
    std::fs::create_dir_all(e.ws.join(".synt")).unwrap();
    std::fs::write(e.ws.join(".synt/settings.json"), "{}").unwrap();
    e.start();
    let v = e.ok("trust.set", json!({"mode": "remember"}));
    assert_eq!(v["trusted"], true, "{v}");
    let st = e.ok("trust.state", Value::Null);
    let rows = st["records"].as_array().unwrap();
    assert_eq!(rows.len(), 1, "{st}");
    assert_eq!(rows[0]["scope"], "folder");
    // a remembered grant is not kept in memory: the store is the only source
    assert!(st["session_grant"].is_null(), "{st}");
    let root = rows[0]["root"].as_str().unwrap().to_string();
    assert_eq!(e.ok("trust.revoke", json!({"root": root}))["revoked"], true);
    assert_eq!(
        e.ok("trust.state", Value::Null)["records"]
            .as_array()
            .unwrap()
            .len(),
        0
    );
}

#[test]
fn a_remembered_trust_is_not_honoured_by_a_new_engine_without_an_enforcing_sandbox() {
    let mut e = desktop_engine(|_| json!([{"text": "ok"}]));
    std::fs::create_dir_all(e.ws.join(".synt")).unwrap();
    std::fs::write(e.ws.join(".synt/settings.json"), "{}").unwrap();
    e.start();
    e.ok("trust.set", json!({"mode": "remember"}));
    let root = e.ws.parent().unwrap().to_path_buf();
    // the same home, a new process (the test engine runs with --sandbox off)
    drop(e.stdin.take());
    let _ = e.child.wait();
    let keep = tempfile::tempdir_in(&root).unwrap();
    drop(keep);
    // a second engine process on the same store: not trusted, and says why
    let mut e2 = engine_same_home(&root);
    let info = e2.start();
    assert_eq!(info["trusted"], false, "{info}");
    assert!(
        info["notices"].to_string().contains("not honoured"),
        "{info}"
    );
}

/// A new engine process on the home and workspace of an earlier one.
fn engine_same_home(root: &Path) -> Engine {
    engine_in_folder(root, "ws")
}

/// A new engine process on the home of an earlier one, working in `root/<folder>`.
fn engine_in_folder(root: &Path, folder: &str) -> Engine {
    let (ws, home) = (root.join(folder), root.join("home"));
    std::fs::create_dir_all(&ws).unwrap();
    let script_path = root.join("script-again.json");
    std::fs::write(&script_path, json!([{"text": "ok"}]).to_string()).unwrap();
    let mut c = Command::new(env!("CARGO_BIN_EXE_synt"));
    c.args([
        "engine",
        "--stdio",
        "--host",
        "desktop",
        "--sandbox",
        "off",
        "--provider",
    ])
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
        .env("HOME", root)
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
        _dir: tempfile::tempdir().unwrap(),
    }
}

fn trusted_config(e: &Engine) {
    std::fs::create_dir_all(e.ws.join(".synt")).unwrap();
    std::fs::write(e.ws.join(".synt/settings.json"), "{}").unwrap();
}

#[test]
fn a_session_of_another_folder_is_never_resumed() {
    let (mut a, info) = used_engine();
    let sid_a = info["sid"].as_str().unwrap().to_string();
    let root = a.ws.parent().unwrap().to_path_buf();
    drop(a.stdin.take());
    let _ = a.child.wait();
    // a second folder on the same store
    let mut b = engine_in_folder(&root, "ws2");
    b.hello();
    let cwd = b.ws.to_str().unwrap().to_string();
    let id = b.call("start", json!({"cwd": cwd, "resume": sid_a}));
    let err = b.result(id).1["error"].as_str().unwrap().to_string();
    assert!(err.contains("another folder"), "{err}");
    assert!(!err.contains(root.to_str().unwrap()), "no host path: {err}");
    // nothing started: a plain start works, and the in-place resume is refused too
    let id = b.call("start", json!({"cwd": cwd}));
    assert!(b.result(id).1["error"].is_null());
    let err = b.err("session.resume", json!({"id": sid_a}));
    assert!(
        err.starts_with("refused: ") && err.contains("another folder"),
        "{err}"
    );
}

#[test]
fn a_restart_during_a_task_is_refused_busy_and_the_task_goes_on() {
    let mut e = desktop_engine(write_script);
    e.start();
    e.call("send", json!({"text": "write it"}));
    let ask = e
        .until("ask", |v| v["msg"] == "ask.approval")
        .pop()
        .unwrap();
    let err = e.err("session.new", Value::Null);
    assert!(err.starts_with("refused: busy"), "{err}");
    let err = e.err("trust.set", json!({"mode": "session"}));
    assert!(err.starts_with("refused: "), "{err}");
    // the cancel is answered by the reader at once and ends the task
    e.call("cancel", Value::Null);
    let seen = e.until("task_done", is_task_done);
    assert!(
        seen.iter()
            .any(|v| v["msg"] == "withdraw" && v["id"] == ask["id"])
    );
    assert!(e.ok("clear", Value::Null).is_object());
}

#[test]
fn a_failed_restart_resumes_the_session_it_ended_untrusted_when_the_trust_was_the_failure() {
    let mut e = desktop_engine(|_| json!([{"text": "ok"}]));
    trusted_config(&e);
    let info = e.start();
    e.ok("trust.set", json!({"mode": "session"}));
    gap();
    let old = e.ok("session.new", Value::Null)["sid"]
        .as_str()
        .unwrap()
        .to_string();
    // the configuration changes under a trusted session; the model switch cannot start trusted
    std::fs::write(e.ws.join(".synt/settings.json"), r#"{"changed":true}"#).unwrap();
    gap();
    let v = e.ok("model.set", json!({"model": "a/b"}));
    // the session it ended is the one that continues (resumed), without trust, and the report says so
    assert_eq!(v["sid"], old.as_str(), "{v}");
    assert_ne!(v["sid"], info["sid"]);
    assert_eq!(
        (
            v["trusted"].as_bool(),
            v["notices"].to_string().contains("UNTRUSTED")
        ),
        (Some(false), true),
        "{v}"
    );
}

#[test]
fn a_restart_that_cannot_resume_anything_ends_the_engine_with_a_fatal_error() {
    let (mut e, _) = used_engine();
    // the folder vanishes: neither the new session nor the old one can start
    std::fs::remove_dir_all(&e.ws).unwrap();
    let err = e.err("session.new", Value::Null);
    assert!(err.starts_with("fatal: "), "{err}");
    drop(e.stdin.take());
    // and the engine is gone: nothing more is answered, a second `start` is not possible
    while e.line().is_some() {}
}

#[test]
fn a_grant_needs_a_fingerprint_a_session_grant_survives_restarts_and_a_changed_config_drops_it() {
    let mut e = desktop_engine(|_| json!([{"text": "ok"}]));
    trusted_config(&e);
    e.start();
    let v = e.ok("trust.set", json!({"mode": "session"}));
    assert_eq!(v["trusted"], true, "{v}");
    gap();
    // carried across a restart of this process, for the same root and an unchanged configuration
    let v = e.ok("session.new", Value::Null);
    assert_eq!(v["trusted"], true, "{v}");
    let st = e.ok("trust.state", Value::Null);
    assert_eq!(st["session_grant"], st["root"]);
    // the (trusted) agent edits the configuration it was trusted for: the next restart does not apply the
    // stale grant, it continues untrusted and says so; the grant is gone
    std::fs::write(e.ws.join(".synt/settings.json"), r#"{"changed":true}"#).unwrap();
    gap();
    let v = e.ok("session.new", Value::Null);
    assert_eq!(v["trusted"], false, "{v}");
    assert!(v["notices"].to_string().contains("UNTRUSTED"), "{v}");
    assert!(e.ok("trust.state", Value::Null)["session_grant"].is_null());
}

#[test]
fn parent_trust_of_the_home_folder_and_a_non_enforcing_sandbox_are_reported_by_the_engine() {
    let mut e = desktop_engine(|_| json!([{"text": "ok"}]));
    trusted_config(&e);
    e.start();
    let st = e.ok("trust.state", Value::Null);
    assert_eq!(
        st["enforcing"], false,
        "the test engine runs with --sandbox off"
    );
    // the project's parent is this test's home: never offered, never granted
    assert!(st["parent"].is_null(), "{st}");
    assert!(
        e.err("trust.set", json!({"mode": "parent"}))
            .contains("refused")
    );
}

#[test]
fn a_send_during_a_compaction_is_refused_busy_and_loses_nothing() {
    let mut e = desktop_engine(|ws| {
        json!([
            {"text": "first answer"},
            // the compaction's model turn waits until the file exists
            {"text": "summary", "after_file": ws.join("go")},
            {"text": "second answer"}
        ])
    });
    e.start();
    e.call("send", json!({"text": "one"}));
    e.until("task_done", is_task_done);
    assert_eq!(e.ok("compact", json!({}))["started"], false);
    // the compaction runs on a copy of the history and then replaces it: nothing may start meanwhile
    let err = e.err("send", json!({"text": "two"}));
    assert!(err.starts_with("refused: busy"), "{err}");
    assert!(e.err("session.new", Value::Null).contains("busy"));
    assert!(e.err("clear", Value::Null).contains("running"));
    std::fs::write(e.ws.join("go"), "").unwrap();
    e.until("compacted", |v| v["msg"] == "compacted");
    e.call("send", json!({"text": "two"}));
    let seen = e.until("task_done", is_task_done);
    assert!(seen.last().unwrap()["error"].is_null(), "{seen:?}");
}

#[test]
fn a_refused_trust_restart_writes_no_row() {
    let mut e = desktop_engine(write_script);
    trusted_config(&e);
    e.start();
    e.call("send", json!({"text": "write it"}));
    let ask = e
        .until("ask", |v| v["msg"] == "ask.approval")
        .pop()
        .unwrap();
    // busy: refused before the engine wrote anything
    let err = e.err("trust.set", json!({"mode": "remember"}));
    assert!(err.contains("busy"), "{err}");
    // an answer is served at once while other requests are queued behind a slow worker
    let id = e.call("answer", json!({"id": ask["id"], "choice": "deny"}));
    let r = e.results(&[id]);
    assert_eq!(r[&id]["ok"]["accepted"], true);
    e.until("task_done", is_task_done);
    assert_eq!(
        e.ok("trust.state", Value::Null)["records"]
            .as_array()
            .unwrap()
            .len(),
        0
    );
    // the restart gap: a restart right after another is refused before any row is written
    e.ok("session.new", Value::Null);
    let err = e.err("trust.set", json!({"mode": "remember"}));
    assert!(err.contains("too many restarts"), "{err}");
    assert_eq!(
        e.ok("trust.state", Value::Null)["records"]
            .as_array()
            .unwrap()
            .len(),
        0
    );
}

#[test]
fn workspace_close_hides_sessions_only_after_the_new_one_runs() {
    let mut e = desktop_engine(|_| json!([{"text": "ok"}]));
    trusted_config(&e);
    let info = e.start();
    let old = info["sid"].as_str().unwrap().to_string();
    e.call("send", json!({"text": "hi"}));
    e.until("task_done", is_task_done);
    let ws = e.ok("workspaces.list", Value::Null);
    let key = ws["workspaces"][0]["key"].as_str().unwrap().to_string();
    assert!(ws.to_string().contains(&old));
    // a restart that fails (the trusted configuration changed under a session grant) resumes the session:
    // it is still listed, nothing was hidden
    e.ok("trust.set", json!({"mode": "session"}));
    std::fs::write(e.ws.join(".synt/settings.json"), r#"{"changed":true}"#).unwrap();
    gap();
    let v = e.ok("workspace.close", json!({"workspace": key}));
    assert!(v["notices"].to_string().contains("UNTRUSTED"), "{v}");
    assert!(
        e.ok("workspaces.list", Value::Null)
            .to_string()
            .contains(&old),
        "still listed"
    );
    // a restart that succeeds hides it
    gap();
    let v = e.ok("workspace.close", json!({"workspace": key}));
    assert!(
        v["closed_report"]["closed"].to_string().contains(&old),
        "{v}"
    );
    assert!(
        !e.ok("workspaces.list", Value::Null)
            .to_string()
            .contains(&old)
    );
}

#[test]
fn a_restart_says_when_it_resets_the_permission_mode() {
    let (mut e, _) = used_engine();
    assert_eq!(e.ok("set_mode", json!({"mode": "plan"}))["mode"], "plan");
    let v = e.ok("session.new", Value::Null);
    assert_eq!(v["mode"], "default");
    let n = v["notices"].to_string();
    assert!(n.contains("again") && n.contains("plan"), "{n}");
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

/// `synt stop SID` as another process.
fn synt_stop(e: &Engine, sid: &str) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_synt"))
        .args(["stop", sid])
        .env_clear()
        .env("HOME", e.home.parent().unwrap())
        .env("SYNT_HOME", &e.home)
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .output()
        .unwrap()
}

#[test]
fn synt_stop_follows_the_new_session_after_a_restart_and_the_old_id_does_nothing() {
    let mut e = engine_with(
        &["--no-cost-cap"],
        None,
        false,
        |_| json!([{"delay_ms": 60000, "text": "never"}]),
    );
    let old = e.start()["sid"].as_str().unwrap().to_string();
    let new = e.ok("session.new", Value::Null)["sid"]
        .as_str()
        .unwrap()
        .to_string();
    assert_ne!(old, new);
    e.call("send", json!({"text": "slow"}));
    std::thread::sleep(Duration::from_millis(1500));
    // the ended session's id is held by nobody: an error, and the new session's task goes on
    assert_eq!(synt_stop(&e, &old).status.code(), Some(1));
    let t0 = Instant::now();
    while t0.elapsed() < Duration::from_secs(3) {
        if let Ok(l) = e.lines.recv_timeout(Duration::from_millis(200)) {
            assert!(
                !l.contains("task_done") && !l.contains("task.ended"),
                "the old id ended the new session's task: {l}"
            );
        }
    }
    assert!(e.child.try_wait().unwrap().is_none(), "the engine lives");
    // the new session's watcher is armed: its stop cancels its task
    assert_eq!(synt_stop(&e, &new).status.code(), Some(0));
    let seen = e.until("task_done", is_task_done);
    let ended = seen
        .iter()
        .find(|v| v["type"] == "task.ended")
        .expect("task.ended");
    assert_eq!(ended["payload"]["status"], "cancelled", "{ended}");
}

#[test]
fn a_resumed_session_continues_its_hash_chain_and_each_session_has_its_own_baseline() {
    // a governed (headless) engine in a git repository: the sessions run in worktrees
    let mut e = engine_with(
        &["--no-cost-cap"],
        None,
        true,
        |_| json!([{"text": "one"}, {"text": "two"}, {"text": "three"}]),
    );
    let a = e.start()["sid"].as_str().unwrap().to_string();
    e.call("send", json!({"text": "first"}));
    e.until("task_done", is_task_done);
    gap();
    let b = e.ok("session.new", Value::Null)["sid"]
        .as_str()
        .unwrap()
        .to_string();
    assert_ne!(a, b);
    e.call("send", json!({"text": "in the new one"}));
    e.until("task_done", is_task_done);
    gap();
    // the session of a worktree resumes in place (its folder is the repository the engine was opened in)
    let v = e.ok("session.resume", json!({"id": a}));
    assert_eq!(v["sid"], a.as_str(), "{v}");
    e.call("send", json!({"text": "second"}));
    e.until("task_done", is_task_done);
    // the event chain of the resumed session verifies across the restart, and the other one's too
    for sid in [&a, &b] {
        let o = Command::new(env!("CARGO_BIN_EXE_synt"))
            .args(["sessions", "verify", sid])
            .env_clear()
            .env("HOME", e.home.parent().unwrap())
            .env("SYNT_HOME", &e.home)
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .output()
            .unwrap();
        assert_eq!(
            o.status.code(),
            Some(0),
            "{sid}: {}",
            String::from_utf8_lossy(&o.stderr)
        );
    }
    // the verification baseline lives in the session's own artifact folder (`gate-baseline.json`, keyed by the
    // session id): each session has its own folder, so a resumed or new session never shares a baseline
    let dirs: Vec<_> = [&a, &b]
        .iter()
        .map(|sid| e.home.join("sessions").join(sid))
        .collect();
    assert!(dirs.iter().all(|d| d.is_dir()) && dirs[0] != dirs[1]);
}

// ---- protocol 3: files, images, rewind, clean, memory, settings

/// A scan or health check is limited to a few a second: tests wait it out between two of them.
fn calm() {
    std::thread::sleep(Duration::from_millis(300));
}

fn png_b64(n: usize) -> (String, Vec<u8>) {
    let mut v = b"\x89PNG\r\n\x1a\n\0\0\0\x0dIHDR".to_vec();
    v.extend(10u32.to_be_bytes());
    v.extend(10u32.to_be_bytes());
    v.resize(n, 9);
    (b64(&v), v)
}

fn b64(bytes: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for c in bytes.chunks(3) {
        let n = (u32::from(c[0]) << 16)
            | (u32::from(*c.get(1).unwrap_or(&0)) << 8)
            | u32::from(*c.get(2).unwrap_or(&0));
        for k in 0..4 {
            out.push(if k <= c.len() {
                T[(n >> (18 - 6 * k) & 63) as usize] as char
            } else {
                '='
            });
        }
    }
    out
}

#[test]
fn the_files_methods_list_read_and_refuse_what_must_not_leave() {
    let mut e = desktop_engine(|_| json!([{"text": "ok"}]));
    let outside = e.ws.parent().unwrap().join("outside.txt");
    std::fs::write(&outside, "TOPSECRET").unwrap();
    std::fs::create_dir_all(e.ws.join("src")).unwrap();
    std::fs::write(e.ws.join("src/a.rs"), "fn main() {}\n").unwrap();
    std::fs::write(e.ws.join(".env"), "KEY=1").unwrap();
    std::fs::write(e.ws.join("bin"), [0u8, 1, 2]).unwrap();
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(&outside, e.ws.join("link.txt")).unwrap();
        std::os::unix::fs::symlink("/etc", e.ws.join("etc")).unwrap();
        std::fs::hard_link(&outside, e.ws.join("hard.txt")).unwrap();
    }
    e.start();
    let l = e.ok("fs.list", json!({}));
    let names: Vec<&str> = l["rows"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r[1].as_str().unwrap())
        .collect();
    assert!(names.contains(&"src") && names.contains(&"a.rs"), "{l}");
    assert!(
        !names.contains(&"link.txt") && !names.contains(&"etc"),
        "links are not listed: {l}"
    );
    assert_eq!(
        l["trusted"], false,
        "an untrusted folder is walked, git is not run"
    );
    assert_eq!(
        e.ok("fs.read", json!({"path": "src/a.rs"}))["lines"][0],
        "fn main() {}"
    );
    let root = e.ws.parent().unwrap().to_str().unwrap().to_string();
    let mut errors = vec![];
    for p in [
        ".env",
        "bin",
        "src",
        "nope",
        "../outside.txt",
        "/etc/passwd",
        "src/../.env",
        ".git/config",
        "a\nb",
        "link.txt",
        "etc/hosts",
        "hard.txt",
    ] {
        let r = e.ask("fs.read", json!({"path": p}));
        if cfg!(not(unix)) && ["link.txt", "etc/hosts", "hard.txt"].contains(&p) {
            continue;
        }
        errors.push(
            r["error"]
                .as_str()
                .unwrap_or_else(|| panic!("{p} must fail: {r}"))
                .to_string(),
        );
    }
    assert!(
        errors
            .iter()
            .all(|t| !t.contains(&root) && !t.contains("TOPSECRET")),
        "{errors:?}"
    );
    for (m, p) in [
        ("fs.list", json!({"dir": "../.."})),
        ("fs.list", json!({"dir": "/"})),
        ("fs.list", json!({"dir": "nope"})),
        ("fs.list", json!({"offset": -1})),
        ("fs.read", json!({})),
        ("fs.read", json!({"path": 5})),
    ] {
        calm();
        assert!(e.ask(m, p.clone())["error"].is_string(), "{m} {p}");
    }
    // git only in a trusted folder: the engine says no, whatever the client asks
    calm();
    assert!(e.err("fs.git_diff", Value::Null).contains("not trusted"));
    assert!(e.err("fs.git_status", Value::Null).contains("not trusted"));
}

#[test]
fn git_runs_through_the_engine_only_after_it_trusted_the_folder() {
    let mut e = engine_with(
        &["--host", "desktop"],
        None,
        true,
        |_| json!([{"text": "ok"}]),
    );
    std::fs::create_dir_all(e.ws.join(".synt")).unwrap();
    std::fs::write(e.ws.join(".synt/settings.json"), "{}").unwrap();
    std::fs::write(e.ws.join("a.txt"), "hello\nchanged\n").unwrap();
    e.start();
    assert_eq!(
        e.ok("trust.set", json!({"mode": "session"}))["trusted"],
        true
    );
    gap();
    let s = e.ok("fs.git_status", Value::Null);
    assert!(s["files"].to_string().contains("a.txt"), "{s}");
    calm();
    let d = e.ok("fs.git_diff", Value::Null);
    assert!(d["diff"].to_string().contains("+changed"), "{d}");
    calm();
    assert_eq!(e.ok("fs.list", json!({}))["trusted"], true);
    // a request right after another scan is refused, not queued behind it
    assert!(e.err("fs.list", json!({})).contains("too many requests"));
}

#[test]
fn an_image_goes_in_chunks_is_checked_and_is_sent_by_the_id_the_engine_issued() {
    let mut e = desktop_engine(|_| json!([{"text": "I see it."}]));
    e.start();
    let (data, raw) = png_b64(4000);
    let (first, rest) = data.split_at(2000);
    let up = e.ok(
        "image.put",
        json!({"seq": 0, "mime": "image/png", "size": raw.len(), "data": first, "last": false}),
    );
    let upload = up["upload"].as_str().unwrap().to_string();
    // a forged or skipped upload id leaves nothing behind
    assert!(
        e.err(
            "image.put",
            json!({"upload": "x", "seq": 1, "data": rest, "last": true})
        )
        .contains("no such upload")
    );
    assert!(
        e.err(
            "image.put",
            json!({"upload": upload, "seq": 1, "data": rest, "last": true})
        )
        .contains("no such upload")
    );
    let up = e.ok(
        "image.put",
        json!({"seq": 0, "mime": "image/png", "size": raw.len(), "data": first, "last": false}),
    );
    let id = e.ok(
        "image.put",
        json!({"upload": up["upload"], "seq": 1, "data": rest, "last": true}),
    )["id"]
        .as_str()
        .unwrap()
        .to_string();
    // hostile uploads
    for p in [
        json!({"seq": 0, "mime": "image/svg+xml", "size": 10, "data": "AAAA", "last": true}),
        json!({"seq": 0, "mime": "image/png", "size": 10_000_000, "data": "AAAA", "last": false}),
        json!({"seq": 0, "mime": "image/png", "size": 3, "data": "!!!!", "last": true}),
        json!({"seq": 0, "mime": "image/png", "size": 3, "data": "AAAA", "last": true}),
        json!({"seq": 0, "mime": "image/png", "size": 3, "data": "A".repeat(600_000), "last": false}),
        json!({"seq": 7, "mime": "image/png", "size": 3, "data": "AAAA", "last": true}),
    ] {
        let r = e.ask("image.put", p.clone());
        assert!(r["error"].is_string(), "{p}: {r}");
    }
    // dimensions come from the header and are capped: 9000 pixels wide is refused
    let mut wide = b"\x89PNG\r\n\x1a\n\0\0\0\x0dIHDR".to_vec();
    wide.extend(9000u32.to_be_bytes());
    wide.extend(10u32.to_be_bytes());
    let r = e.ask(
        "image.put",
        json!({"seq": 0, "mime": "image/png", "size": wide.len(), "data": b64(&wide), "last": true}),
    );
    assert!(r["error"].as_str().unwrap().contains("pixels"), "{r}");
    // an id nobody issued is no image
    assert!(
        e.err("send", json!({"text": "look", "images": ["img-nope"]}))
            .contains("no such image")
    );
    e.call("send", json!({"text": "look", "images": [id]}));
    let seen = e.until("task_done", is_task_done);
    assert!(seen.last().unwrap()["error"].is_null(), "{seen:?}");
    let user = seen
        .iter()
        .find(|v| v["type"] == "user.message")
        .expect("user.message");
    assert!(user["payload"].to_string().contains("image"), "{user}");
    // the id is spent
    assert!(
        e.err("send", json!({"text": "again", "images": [id]}))
            .contains("no such image")
    );
    assert!(
        e.err("image.drop", json!({"id": "../x"}))
            .contains("session id")
    );
}

#[test]
fn a_rewind_needs_the_engines_dry_run_and_a_clean_needs_its_plan() {
    let mut e = desktop_engine(write_script);
    let target = e.ws.join("hello.txt");
    e.start();
    e.ok("set_mode", json!({"mode": "acceptEdits"}));
    e.call("send", json!({"text": "write it"}));
    e.until("task_done", is_task_done);
    assert!(target.exists());
    let cps = e.ok("rewind.list", Value::Null);
    assert_eq!(cps["checkpoints"].as_array().unwrap().len(), 1, "{cps}");
    // no dry run yet: nothing can be applied, and a made-up token is no better
    for t in ["rw-1", "", "../x"] {
        assert!(
            e.err("rewind.apply", json!({"token": t}))
                .contains("dry run")
        );
    }
    assert!(target.exists());
    assert!(
        e.err("rewind.dry_run", json!({"checkpoint": "a\nb"}))
            .contains("checkpoint")
    );
    let dry = e.ok("rewind.dry_run", json!({"checkpoint": null}));
    let entries = dry["entries"].as_array().unwrap();
    assert_eq!(entries.len(), 1, "{dry}");
    assert_eq!(entries[0]["kind"], "remove");
    let root = e.ws.parent().unwrap().to_str().unwrap().to_string();
    assert!(!dry.to_string().contains("TOKEN-LEAK"));
    // the token is single use: a wrong one spends it, and the right one then no longer works
    assert!(
        e.err("rewind.apply", json!({"token": "rw-wrong"}))
            .contains("no longer valid")
    );
    let e2 = e.err("rewind.apply", json!({"token": dry["token"]}));
    assert!(e2.contains("dry run") && !e2.contains(&root), "{e2}");
    assert!(target.exists(), "nothing was written");
    let dry = e.ok("rewind.dry_run", json!({"checkpoint": null}));
    let done = e.ok("rewind.apply", json!({"token": dry["token"]}));
    assert_eq!(
        (
            done["removed"].as_u64(),
            done["failed"].as_array().map(Vec::len)
        ),
        (Some(1), Some(0)),
        "{done}"
    );
    assert!(!target.exists(), "the file the run created is gone");
    assert!(
        e.err("rewind.apply", json!({"token": dry["token"]}))
            .contains("dry run")
    );
    // clean: the plan lists the three scopes; the own session is never in it; the token is single use
    calm();
    let plan = e.ok("clean.plan", Value::Null);
    let plans = plan["plans"].as_array().unwrap();
    assert_eq!(plans.len(), 3, "{plan}");
    assert!(
        plans.iter().all(|p| p["counts"]["sessions"] == 0),
        "the open session is kept: {plan}"
    );
    calm();
    assert!(
        e.err(
            "clean.apply",
            json!({"token": "cl-made-up", "backup": false})
        )
        .contains("no longer valid")
    );
    // a wrong token spends the plans: the old ones are dead, a new plan is needed
    let tok = plans[2]["token"].clone();
    calm();
    assert!(
        e.err("clean.apply", json!({"token": tok, "backup": false}))
            .contains("no longer valid")
    );
    calm();
    let plan = e.ok("clean.plan", Value::Null);
    let tok = plan["plans"][2]["token"].clone();
    calm();
    let r = e.ok("clean.apply", json!({"token": tok, "backup": false}));
    assert_eq!(r["deleted"], 0, "{r}");
    calm();
    assert!(
        e.err("clean.apply", json!({"token": tok, "backup": false}))
            .contains("no longer valid")
    );
    assert!(
        e.ok("workspaces.list", Value::Null)
            .to_string()
            .contains("sessions")
    );
}

#[test]
fn memory_effort_context_doctor_and_mcp_run_on_the_engines_own_files() {
    let mut e = desktop_engine(|_| json!([{"text": "ok"}]));
    e.start();
    let root = e.ws.parent().unwrap().to_str().unwrap().to_string();
    // memory: the person's note is saved, listed, searched and archived; a secret is refused
    let r = e.ok(
        "memory.add",
        json!({"text": "Tests run with cargo test", "user": false}),
    );
    assert_eq!(
        r["scope"], "user",
        "an untrusted folder only has the user scope: {r}"
    );
    let l = e.ok("memory.list", Value::Null);
    let id = l["records"][0]["id"].as_str().unwrap().to_string();
    assert_eq!(l["records"][0]["trust"], "owner");
    assert_eq!(
        e.ok("memory.retrieve", json!({"query": "cargo"}))["records"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert!(
        e.ok("memory.show", json!({"id": id}))["text"]
            .as_str()
            .unwrap()
            .contains("cargo test")
    );
    assert!(
        e.err(
            "memory.add",
            json!({"text": "k sk-live-abcdefgh12345678901234", "user": true})
        )
        .contains("secret")
    );
    assert!(
        e.err("memory.show", json!({"id": "../x"}))
            .contains("memory id")
    );
    e.ok("memory.forget", json!({"id": id}));
    assert!(
        e.ok("memory.list", Value::Null)["records"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    // `#` notes land in this machine's instruction file, bounded
    let n = e.ok("memory.note", json!({"text": "use tabs", "user": true}));
    assert!(n["text"].as_str().unwrap().contains("saved"), "{n}");
    assert!(
        std::fs::read_to_string(e.home.join("AGENTS.md"))
            .unwrap()
            .contains("use tabs")
    );
    assert!(
        e.err(
            "memory.note",
            json!({"text": "x".repeat(9000), "user": true})
        )
        .contains("bytes")
    );
    // effort: a tier the model does not declare is refused; "auto" is fine
    assert!(
        e.err("config.effort", json!({"effort": "ludicrous"}))
            .contains("tier")
    );
    assert!(e.ok("config.effort", json!({"effort": null}))["effort"].is_null());
    // context: nothing is declared in this home, so the engine refuses, saying nothing of its folders
    let c = e.err("context.set", json!({"value": "1m"}));
    assert!(!c.contains(&root), "{c}");
    assert!(
        e.err("context.set", json!({"value": "x".repeat(100)}))
            .contains("token")
    );
    calm();
    let d = e.ok("doctor.summary", Value::Null);
    assert!(
        d["rows"]
            .as_array()
            .unwrap()
            .iter()
            .all(|r| ["ok", "warn", "fail"].contains(&r["level"].as_str().unwrap()))
    );
    assert!(e.ok("mcp.list", Value::Null)["servers"].is_array());
    // mcp over the protocol: a URL server into the user file and nothing that runs code, carries environment
    // or headers, or replaces a server
    let url = "https://mcp.example.com/mcp";
    for p in [
        json!({"name": "a", "transport": "http", "command": ["sh", "-c", "id"]}),
        json!({"name": "a", "transport": "http", "url": url, "command": ["srv"]}),
        json!({"name": "a", "transport": "http", "url": url, "env": ["LD_PRELOAD=/tmp/x.so"]}),
        json!({"name": "a", "transport": "http", "url": url, "header": ["Authorization: Bearer ${SYNT_API_KEY}"]}),
        json!({"name": "a", "transport": "http", "url": "file:///etc/passwd"}),
        json!({"name": "../x", "transport": "http", "url": url}),
    ] {
        assert!(e.ask("mcp.add", p.clone())["error"].is_string(), "{p}");
    }
    assert!(!e.home.join("synt.json").exists(), "nothing was written");
    e.ok(
        "mcp.add",
        json!({"name": "a", "transport": "http", "url": url, "scope": "project", "force": true}),
    );
    let again = e.err(
        "mcp.add",
        json!({"name": "a", "transport": "http", "url": "https://evil.example/x", "force": true}),
    );
    assert!(
        again.contains("already exists") && !again.contains(&root),
        "{again}"
    );
    let file = std::fs::read_to_string(e.home.join("synt.json")).unwrap();
    assert!(file.contains("mcp.example.com") && !file.contains("evil.example"));
    assert!(
        !e.ws.join(".mcp.json").exists(),
        "a project-scope server is never written"
    );
    e.ok("mcp.remove", json!({"name": "a"}));
    assert!(e.err("mcp.remove", json!({"name": "a"})).len() > 3);
    // a `#` note for the project needs a folder the engine trusts; the user file does not
    assert!(
        e.err("memory.note", json!({"text": "n", "user": false}))
            .contains("trusted folder")
    );
    // a flood of heavy calls is refused, not run
    calm();
    e.ok("doctor.summary", Value::Null);
    assert!(
        e.err("doctor.summary", Value::Null)
            .contains("too many requests")
    );
}

#[test]
fn the_new_methods_need_a_session_and_survive_hostile_input() {
    let mut e = desktop_engine(|_| json!([{"text": "ok"}]));
    e.hello();
    // before `start` nothing of the folder is reachable
    for m in [
        "fs.git_diff",
        "rewind.list",
        "clean.plan",
        "memory.list",
        "mcp.list",
        "doctor.summary",
    ] {
        calm();
        let r = e.ask(m, Value::Null);
        assert!(
            r["error"].as_str().unwrap().contains("not started"),
            "{m}: {r}"
        );
    }
    calm();
    assert!(e.err("fs.list", json!({})).contains("not started"));
    calm();
    assert!(e.err("fs.list", json!({})).contains("not started"));
    let ws = e.ws.to_str().unwrap().to_string();
    e.ok("start", json!({"cwd": ws}));
    for (m, p) in [
        ("image.put", json!({"seq": 0, "data": 5, "last": true})),
        ("rewind.dry_run", json!({"checkpoint": 5})),
        ("rewind.apply", json!({})),
        ("clean.apply", json!({"token": "x"})),
        ("clean.apply", json!({"backup": true})),
        ("memory.add", json!({"text": "x"})),
        ("memory.retrieve", json!({"query": "a\u{0}b"})),
        (
            "mcp.add",
            json!({"name": "a", "transport": "ftp", "command": ["x"]}),
        ),
        (
            "mcp.add",
            json!({"name": "a", "transport": "http", "command": vec!["x"; 40]}),
        ),
        ("mcp.remove", json!({"name": "../x"})),
        ("config.effort", json!({"effort": 3})),
        ("context.set", json!({})),
    ] {
        let r = e.ask(m, p.clone());
        assert!(r["error"].is_string(), "{m} {p}: {r}");
    }
    // a protocol 2 client never gets this far: the engine says protocol 3 and the client compares exactly
    assert_eq!(e.ok("set_mode", json!({"mode": "plan"}))["mode"], "plan");
}
