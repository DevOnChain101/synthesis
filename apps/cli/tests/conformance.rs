//! Gaps from docs/research/conformance-2026-10-31.md, end to end through the real binary with a
//! scripted model (feature `test-provider`).
#![cfg(all(feature = "test-provider", unix))]

use std::path::PathBuf;
use std::process::{Command, Output, Stdio};

use serde_json::{Value, json};

struct Rig {
    dir: tempfile::TempDir,
}

impl Rig {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("ws")).unwrap();
        std::fs::write(dir.path().join("ws/a.txt"), "hello\n").unwrap();
        Rig { dir }
    }
    fn ws(&self) -> PathBuf {
        self.dir.path().join("ws")
    }
    fn home(&self) -> PathBuf {
        self.dir.path().join("synthesis")
    }
    /// User-level settings.json (project settings are untrusted).
    fn settings(&self, v: Value) {
        std::fs::create_dir_all(self.home()).unwrap();
        std::fs::write(self.home().join("settings.json"), v.to_string()).unwrap();
    }
    fn base(&self) -> Command {
        let mut c = Command::new(env!("CARGO_BIN_EXE_synth"));
        c.env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("HOME", self.dir.path())
            .env("SYNTHESIS_HOME", self.home())
            .stdin(Stdio::null());
        c
    }
    fn synth(&self, args: &[&str]) -> Output {
        self.base().args(args).output().unwrap()
    }
    fn run_cmd(&self, script: Value, args: &[&str]) -> Command {
        let s = self.dir.path().join("script.json");
        std::fs::write(&s, script.to_string()).unwrap();
        let mut c = self.base();
        c.args(["run", "--cwd"])
            .arg(self.ws())
            .arg("--provider")
            .arg(format!("fake:{}", s.display()))
            .args(["--sandbox", "off"])
            .args(args);
        c
    }
    fn run(&self, script: Value, args: &[&str]) -> (Output, Vec<Value>) {
        let o = self
            .run_cmd(script, args)
            .args(["--prompt", "do it"])
            .output()
            .unwrap();
        let ev = events(&o.stdout);
        (o, ev)
    }
}

fn events(stdout: &[u8]) -> Vec<Value> {
    String::from_utf8_lossy(stdout)
        .lines()
        .filter(|l| !l.is_empty())
        .map(|l| serde_json::from_str(l).unwrap_or_else(|e| panic!("not JSON ({e}): {l}")))
        .collect()
}

fn bash(cmd: &str) -> Value {
    json!([{"tools": [{"name": "Bash", "input": {"command": cmd}}]}, {"text": "done"}])
}

#[test]
fn deny_with_space_star_blocks_under_bypass() {
    let rig = Rig::new();
    rig.settings(json!({"permissions": {"deny": ["Bash(touch *)"]}}));
    let (_, ev) = rig.run(
        bash("touch made.txt"),
        &["--permission-mode", "bypassPermissions"],
    );
    assert!(
        !rig.ws().join("made.txt").exists(),
        "the denied command ran"
    );
    let done = ev.iter().rfind(|e| e["type"] == "tool.completed").unwrap();
    assert_eq!(done["payload"]["status"], "denied");
}

#[test]
fn edit_deny_blocks_redirect_under_bypass() {
    let rig = Rig::new();
    rig.settings(json!({"permissions": {"deny": ["Edit(a.txt)"]}}));
    rig.run(
        bash("echo x > a.txt"),
        &["--permission-mode", "bypassPermissions"],
    );
    assert_eq!(
        std::fs::read_to_string(rig.ws().join("a.txt")).unwrap(),
        "hello\n"
    );
}

#[test]
fn allow_with_space_star_lets_the_command_run() {
    let rig = Rig::new();
    rig.settings(json!({"permissions": {"allow": ["Bash(touch *)"]}}));
    rig.run(bash("touch made.txt"), &[]);
    assert!(rig.ws().join("made.txt").exists());
}

#[test]
fn mcp_add_then_run_works_on_a_fresh_home() {
    let rig = Rig::new();
    let o = rig.synth(&["mcp", "add", "demo", "--", "echo", "hi"]);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    for args in [&["mcp", "list"][..], &["models"], &["doctor"]] {
        let o = rig.synth(args);
        assert_ne!(
            o.status.code(),
            Some(2),
            "{args:?}: {}",
            String::from_utf8_lossy(&o.stderr)
        );
    }
    let (o, ev) = rig.run(json!([{"text": "ok"}]), &[]);
    assert_eq!(
        o.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&o.stderr)
    );
    assert!(!ev.is_empty());
}

fn text_script(t: &str) -> Value {
    json!([{"text": t}])
}

fn session_id(ev: &[Value]) -> String {
    ev[0]["session_id"].as_str().unwrap().to_string()
}

fn messages(rig: &Rig, sid: &str) -> i64 {
    let c = rusqlite::Connection::open(rig.home().join("synthesis.db")).unwrap();
    c.query_row(
        "SELECT count(*) FROM messages WHERE session_id=?1",
        [sid],
        |r| r.get(0),
    )
    .unwrap()
}

#[test]
fn run_resume_continues_a_stored_session() {
    let rig = Rig::new();
    let (_, ev1) = rig.run(text_script("first"), &[]);
    let sid = session_id(&ev1);
    assert_eq!(messages(&rig, &sid), 2);
    let (o, ev2) = rig.run(text_script("second"), &["--resume", &sid]);
    assert_eq!(
        o.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&o.stderr)
    );
    assert_eq!(session_id(&ev2), sid, "same session");
    assert_eq!(ev2[0]["payload"]["source"], "resume");
    assert_eq!(messages(&rig, &sid), 4, "history kept, new turn appended");
}

#[test]
fn run_continue_picks_the_latest_session_and_fork_starts_a_new_one() {
    let rig = Rig::new();
    let (_, ev1) = rig.run(text_script("first"), &[]);
    let sid = session_id(&ev1);
    let (_, ev2) = rig.run(text_script("second"), &["--continue"]);
    assert_eq!(session_id(&ev2), sid);
    let (o, ev3) = rig.run(text_script("third"), &["--continue", "--fork-session"]);
    assert_eq!(
        o.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&o.stderr)
    );
    let forked = session_id(&ev3);
    assert_ne!(forked, sid);
    assert_eq!(messages(&rig, &sid), 4, "the original is untouched");
    assert_eq!(
        messages(&rig, &forked),
        6,
        "history copied, new turn appended"
    );
}

#[test]
fn run_resume_by_id_is_limited_to_the_current_workspace_unless_asked() {
    let rig = Rig::new();
    let (_, ev1) = rig.run(text_script("first"), &[]);
    let sid = session_id(&ev1);
    let other = rig.dir.path().join("other");
    std::fs::create_dir_all(&other).unwrap();
    let script = rig.dir.path().join("s2.json");
    std::fs::write(&script, text_script("second").to_string()).unwrap();
    let go = |extra: &[&str]| {
        rig.base()
            .args(["run", "--cwd"])
            .arg(&other)
            .arg("--provider")
            .arg(format!("fake:{}", script.display()))
            .args(["--sandbox", "off", "--prompt", "x", "--resume", &sid])
            .args(extra)
            .output()
            .unwrap()
    };
    let o = go(&[]);
    assert_eq!(
        o.status.code(),
        Some(2),
        "{}",
        String::from_utf8_lossy(&o.stderr)
    );
    assert_eq!(messages(&rig, &sid), 2, "nothing was loaded or appended");
    let o = go(&["--resume-any-workspace"]);
    assert_eq!(
        o.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&o.stderr)
    );
}

#[test]
fn run_resume_of_an_unknown_session_is_a_usage_error() {
    let rig = Rig::new();
    let (o, _) = rig.run(text_script("x"), &["--resume", "nope"]);
    assert_eq!(o.status.code(), Some(2));
}

#[test]
fn output_format_text_prints_only_the_final_message() {
    let rig = Rig::new();
    let o = rig
        .run_cmd(text_script("the answer"), &["--output-format", "text"])
        .args(["--prompt", "do it"])
        .output()
        .unwrap();
    assert_eq!(String::from_utf8_lossy(&o.stdout), "the answer\n");
}

#[test]
fn output_format_json_has_result_session_id_usage_and_exit_code_matches() {
    let rig = Rig::new();
    let o = rig
        .run_cmd(
            json!([{"text": "the answer", "usage": [3, 4]}]),
            &["--output-format", "json"],
        )
        .args(["--prompt", "do it"])
        .output()
        .unwrap();
    let v: Value = serde_json::from_slice(&o.stdout).unwrap();
    assert_eq!(v["result"], "the answer");
    assert_eq!(v["status"], "completed");
    assert_eq!(v["usage"]["output_tokens"], 4);
    assert!(v["session_id"].as_str().is_some_and(|s| !s.is_empty()));
    assert_eq!(v["exit_code"].as_i64(), o.status.code().map(i64::from));
}

#[test]
fn json_schema_accepts_matching_and_rejects_mismatching_answers() {
    let rig = Rig::new();
    let schema = r#"{"type":"object","required":["n"],"properties":{"n":{"type":"integer"}}}"#;
    let args = ["--output-format", "json", "--json-schema", schema];
    let raw = |t: &str| {
        rig.run_cmd(text_script(t), &args)
            .args(["--prompt", "do it"])
            .output()
            .unwrap()
    };
    let o = raw("{\"n\": 3}");
    let v: Value = serde_json::from_slice(&o.stdout).unwrap();
    assert_eq!(v["structured_output"]["n"], 3);
    assert_eq!(o.status.code(), Some(0));
    let o = raw("{\"n\": \"x\"}");
    assert_eq!(o.status.code(), Some(1));
}

#[test]
fn session_start_and_end_hooks_fire() {
    let rig = Rig::new();
    let h = |f: &str| json!([{"hooks": [{"type": "command", "command": format!("cat > $SYNTHESIS_PROJECT_DIR/{f}")}]}]);
    rig.settings(json!({"hooks": {"SessionStart": h("start.json"), "SessionEnd": h("end.json")}}));
    let (_, ev) = rig.run(text_script("ok"), &[]);
    for (f, name) in [("start.json", "SessionStart"), ("end.json", "SessionEnd")] {
        let v: Value =
            serde_json::from_str(&std::fs::read_to_string(rig.ws().join(f)).unwrap()).unwrap();
        assert_eq!(v["hook_event_name"], name);
    }
    let ran: Vec<_> = ev.iter().filter(|e| e["type"] == "hook.ran").collect();
    assert_eq!(ran.len(), 2);
    assert!(
        ev.last().unwrap()["type"]
            .as_str()
            .unwrap()
            .starts_with("session.")
    );
}

#[test]
fn flag_aliases_and_manual_mode_are_accepted() {
    let rig = Rig::new();
    let (o, _) = rig.run(
        text_script("ok"),
        &[
            "--permission-mode",
            "manual",
            "--allowedTools",
            "Read",
            "--disallowedTools",
            "Bash(rm *)",
            "--append-system-prompt",
            "hi",
            "--bare",
        ],
    );
    assert_eq!(
        o.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&o.stderr)
    );
}

#[test]
fn stdin_over_10mb_is_a_usage_error() {
    use std::io::Write;
    let rig = Rig::new();
    let s = rig.dir.path().join("script.json");
    std::fs::write(&s, text_script("x").to_string()).unwrap();
    let mut c = rig.base();
    c.args(["run", "--cwd"])
        .arg(rig.ws())
        .arg("--provider")
        .arg(format!("fake:{}", s.display()))
        .args(["--sandbox", "off"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut ch = c.spawn().unwrap();
    let mut si = ch.stdin.take().unwrap();
    let t = std::thread::spawn(move || {
        let _ = si.write_all(&vec![b'a'; 11 << 20]);
    });
    let o = ch.wait_with_output().unwrap();
    let _ = t.join();
    assert_eq!(o.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&o.stderr).contains("10 MB"));
}

#[test]
fn failed_mcp_server_is_a_notice_event_and_required_mcp_exits_2() {
    let rig = Rig::new();
    let o = rig.synth(&["mcp", "add", "bad", "--", "/nonexistent/mcp-server"]);
    assert!(o.status.success());
    let (o, ev) = rig.run(text_script("ok"), &[]);
    assert_eq!(o.status.code(), Some(0));
    assert!(
        ev.iter().any(|e| e["type"] == "notice"
            && e["payload"]["text"]
                .as_str()
                .is_some_and(|t| t.contains("failed to start"))),
        "no notice event for the failed server"
    );
    let (o, _) = rig.run(text_script("ok"), &["--require-mcp", "bad"]);
    assert_eq!(o.status.code(), Some(2));
    let (o, _) = rig.run(text_script("ok"), &["--require-mcp", "missing"]);
    assert_eq!(o.status.code(), Some(2));
}

#[test]
fn closed_stderr_does_not_panic() {
    let rig = Rig::new();
    let s = rig.dir.path().join("script.json");
    std::fs::write(&s, text_script("x").to_string()).unwrap();
    let mut c = std::process::Command::new("sh");
    c.env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("HOME", rig.dir.path())
        .env("SYNTHESIS_HOME", rig.home())
        .stdin(Stdio::null())
        .arg("-c")
        .arg("exec \"$0\" run --cwd \"$1\" --provider \"fake:$2\" --prompt hi --sandbox off 2>&-")
        .arg(env!("CARGO_BIN_EXE_synth"))
        .arg(rig.ws())
        .arg(&s);
    let out = c.output().unwrap();
    assert_eq!(
        out.status.code(),
        Some(0),
        "stdout: {}",
        String::from_utf8_lossy(&out.stdout)
    );
}

#[test]
fn stderr_pipe_closed_by_the_reader_does_not_panic() {
    let rig = Rig::new();
    let s = rig.dir.path().join("script.json");
    std::fs::write(&s, text_script("x").to_string()).unwrap();
    let mut c = rig.base();
    c.args(["run", "--cwd"])
        .arg(rig.ws())
        .arg("--provider")
        .arg(format!("fake:{}", s.display()))
        .args(["--prompt", "hi", "--sandbox", "off"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut ch = c.spawn().unwrap();
    drop(ch.stderr.take()); // the reader goes away: every later write to stderr is EPIPE
    let out = ch.wait_with_output().unwrap();
    assert_eq!(
        out.status.code(),
        Some(0),
        "a dead stderr must not change the result"
    );
}

#[test]
fn checkpoint_is_taken_before_the_first_write() {
    let rig = Rig::new();
    let script = json!([
        {"tools": [{"name": "Read", "input": {"file_path": "a.txt"}}]},
        {"tools": [{"name": "Edit", "input": {"file_path": "a.txt", "old_string": "hello", "new_string": "bye"}}]},
        {"tools": [{"name": "Edit", "input": {"file_path": "a.txt", "old_string": "bye", "new_string": "ciao"}}]},
        {"text": "done"}
    ]);
    let (_, ev) = rig.run(script, &["--permission-mode", "acceptEdits"]);
    let sid = session_id(&ev);
    let c = rusqlite::Connection::open(rig.home().join("synthesis.db")).unwrap();
    let n: i64 = c
        .query_row(
            "SELECT count(*) FROM checkpoints WHERE session_id=?1",
            [&sid],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        n, 1,
        "one restore point per file, taken before its first write"
    );
}

#[test]
fn settings_flag_adds_a_layer_and_a_bad_file_is_a_usage_error() {
    let rig = Rig::new();
    let f = rig.dir.path().join("extra.json");
    std::fs::write(
        &f,
        json!({"permissions": {"deny": ["Bash(touch *)"]}}).to_string(),
    )
    .unwrap();
    let args = [
        "--permission-mode",
        "bypassPermissions",
        "--settings",
        f.to_str().unwrap(),
    ];
    rig.run(bash("touch made.txt"), &args);
    assert!(
        !rig.ws().join("made.txt").exists(),
        "the --settings deny rule did not apply"
    );
    std::fs::write(&f, "{not json").unwrap();
    let (o, _) = rig.run(text_script("x"), &args);
    assert_eq!(o.status.code(), Some(2));
}

#[test]
fn rewind_restores_changed_files_and_removes_created_ones_only_with_apply() {
    let rig = Rig::new();
    let script = json!([
        {"tools": [{"name": "Read", "input": {"file_path": "a.txt"}}]},
        {"tools": [{"name": "Edit", "input": {"file_path": "a.txt", "old_string": "hello", "new_string": "bye"}}]},
        {"tools": [{"name": "Write", "input": {"file_path": "b.txt", "content": "new\n"}}]},
        {"text": "done"}
    ]);
    let (_, ev) = rig.run(script, &["--permission-mode", "acceptEdits"]);
    let sid = session_id(&ev);
    let read = |f: &str| std::fs::read_to_string(rig.ws().join(f)).unwrap();
    assert_eq!(read("a.txt"), "bye\n");
    let o = rig.synth(&["rewind", &sid]);
    assert_eq!(
        o.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&o.stderr)
    );
    assert!(String::from_utf8_lossy(&o.stdout).contains("dry run"));
    assert_eq!(read("a.txt"), "bye\n", "a dry run changes nothing");
    let o = rig.synth(&["rewind", &sid, "--apply"]);
    assert_eq!(
        o.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&o.stderr)
    );
    assert_eq!(read("a.txt"), "hello\n");
    assert!(!rig.ws().join("b.txt").exists());
    assert_eq!(
        rig.synth(&["rewind", "no-such-session"]).status.code(),
        Some(2)
    );
}

#[test]
fn rewind_refuses_files_changed_since_the_run_and_symlinks() {
    let rig = Rig::new();
    let script = json!([
        {"tools": [{"name": "Read", "input": {"file_path": "a.txt"}}]},
        {"tools": [{"name": "Edit", "input": {"file_path": "a.txt", "old_string": "hello", "new_string": "bye"}}]},
        {"tools": [{"name": "Write", "input": {"file_path": "b.txt", "content": "new\n"}}]},
        {"text": "done"}
    ]);
    let (_, ev) = rig.run(script, &["--permission-mode", "acceptEdits"]);
    let sid = session_id(&ev);
    let (a, b) = (rig.ws().join("a.txt"), rig.ws().join("b.txt"));
    std::fs::write(&a, "later edit\n").unwrap();
    std::fs::write(&b, "later edit\n").unwrap();
    let text = |o: &Output| String::from_utf8_lossy(&o.stdout).into_owned();
    // the dry run lists them; --apply leaves both alone
    let o = rig.synth(&["rewind", &sid]);
    assert!(text(&o).contains("changed since the run"), "{}", text(&o));
    assert_eq!(o.status.code(), Some(1));
    let o = rig.synth(&["rewind", &sid, "--apply"]);
    assert_eq!(o.status.code(), Some(1));
    assert_eq!(std::fs::read_to_string(&a).unwrap(), "later edit\n");
    assert!(b.exists());
    // a symlink at the final component is never followed, even with --force
    let outside = rig.ws().join("..").join("outside.txt");
    std::fs::write(&outside, "keep").unwrap();
    std::fs::remove_file(&a).unwrap();
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(&outside, &a).unwrap();
        let o = rig.synth(&["rewind", &sid, "--apply", "--force"]);
        assert!(text(&o).contains("symlink"), "{}", text(&o));
        assert_eq!(std::fs::read_to_string(&outside).unwrap(), "keep");
        std::fs::remove_file(&a).unwrap();
    }
    // --force restores the rest
    let o = rig.synth(&["rewind", &sid, "--apply", "--force"]);
    assert_eq!(o.status.code(), Some(0), "{}", text(&o));
    assert_eq!(std::fs::read_to_string(&a).unwrap(), "hello\n");
    assert!(!b.exists());
}
