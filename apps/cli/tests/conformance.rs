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
