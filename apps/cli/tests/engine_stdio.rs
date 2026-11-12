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
    engine_in(tempfile::tempdir().unwrap(), script)
}

/// [`engine`] in an existing folder (a second engine process on the same home and store).
fn engine_in(dir: tempfile::TempDir, script: impl FnOnce(&Path) -> Value) -> Engine {
    let root = std::fs::canonicalize(dir.path()).unwrap();
    let (ws, home) = (root.join("ws"), root.join("home"));
    std::fs::create_dir_all(&ws).unwrap();
    std::fs::create_dir_all(&home).unwrap();
    let script_path = root.join(format!(
        "script{}.json",
        std::process::id() as u64 + rand_tag()
    ));
    std::fs::write(&script_path, script(&ws).to_string()).unwrap();
    let mut c = Command::new(env!("CARGO_BIN_EXE_synt"));
    c.args(["engine", "--stdio", "--sandbox", "off", "--provider"])
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
        assert_eq!(h["protocol"], 2);
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

fn rand_tag() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| u64::from(d.subsec_nanos()))
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
    let mut e = engine(|_| json!([{"text": "one"}, {"text": "two"}, {"text": "three"}]));
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
fn store_requests_are_limited_to_fifty_a_second_exactly() {
    let (mut e, _) = used_engine();
    let ids: Vec<u64> = (0..51)
        .map(|_| e.call("usage.summary", json!({"tz_min": 0})))
        .collect();
    let got = e.results(&ids);
    let refused: Vec<usize> = ids
        .iter()
        .enumerate()
        .filter(|(_, id)| {
            got[*id]["error"]
                .as_str()
                .is_some_and(|t| t.contains("too many requests"))
        })
        .map(|(i, _)| i)
        .collect();
    assert_eq!(
        refused,
        vec![50],
        "the 51st request of the second is the first refused"
    );
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
    let mut e = engine(|_| json!([{"text": "ok"}]));
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
    let mut e = engine(|_| json!([{"text": "ok"}]));
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
    let mut e = engine(|_| json!([{"text": "ok"}]));
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
    c.args(["engine", "--stdio", "--sandbox", "off", "--provider"])
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
    let mut e = engine(write_script);
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
    let mut e = engine(|_| json!([{"text": "ok"}]));
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
    let mut e = engine(|_| json!([{"text": "ok"}]));
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
    let mut e = engine(|_| json!([{"text": "ok"}]));
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
