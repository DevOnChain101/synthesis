//! MCP tool search through the real binary: with `synthesis.mcp.toolSearch` = "on" (user settings)
//! a tool is refused until `ToolSearch` loaded it; a project settings file cannot switch that off.
#![cfg(all(feature = "test-provider", unix))]

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde_json::{Value, json};

fn server() -> PathBuf {
    static P: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
    P.get_or_init(|| {
        let exe = Path::new(env!("CARGO_BIN_EXE_synth"));
        let profile = exe.parent().unwrap();
        let p = profile.join("examples").join("atlassian_like_server");
        let mut c = Command::new(std::env::var("CARGO").unwrap_or_else(|_| "cargo".into()));
        c.args([
            "build",
            "-p",
            "synthesis-mcp",
            "--example",
            "atlassian_like_server",
        ]);
        if profile.file_name().is_some_and(|n| n == "release") {
            c.arg("--release");
        }
        assert!(c.status().unwrap().success(), "cannot build the example");
        p
    })
    .clone()
}

/// `(status, text)` of every `tool.completed` of a run with `script`.
fn run(script: Value, project_settings: Option<Value>) -> Vec<(String, String)> {
    let dir = tempfile::tempdir().unwrap();
    let (home, ws) = (dir.path().join("synthesis"), dir.path().join("ws"));
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(ws.join(".synthesis")).unwrap();
    let cfg = json!({"$schema": "config.json/v1", "mcpServers": {"atl": {"command": server()}}});
    std::fs::write(home.join("synthesis.json"), cfg.to_string()).unwrap();
    let user =
        json!({"permissions": {"allow": ["mcp__atl"]}, "synthesis": {"mcp": {"toolSearch": "on"}}});
    std::fs::write(home.join("settings.json"), user.to_string()).unwrap();
    if let Some(p) = project_settings {
        std::fs::write(ws.join(".synthesis/settings.json"), p.to_string()).unwrap();
    }
    let s = dir.path().join("script.json");
    std::fs::write(&s, script.to_string()).unwrap();
    let o = Command::new(env!("CARGO_BIN_EXE_synth"))
        .args(["run", "--cwd"])
        .arg(&ws)
        .arg("--provider")
        .arg(format!("fake:{}", s.display()))
        .args(["--prompt", "who am i", "--sandbox", "off", "--no-memory"])
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("HOME", dir.path())
        .env("SYNTHESIS_HOME", &home)
        .stdin(Stdio::null())
        .output()
        .unwrap();
    String::from_utf8_lossy(&o.stdout)
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .filter(|e| e["type"] == "tool.completed")
        .map(|e| {
            let p = &e["payload"];
            (
                p["status"].as_str().unwrap_or("").to_string(),
                p["preview"].as_str().unwrap_or("").to_string(),
            )
        })
        .collect()
}

fn call(name: &str, input: Value) -> Value {
    json!({"tools": [{"name": name, "input": input}]})
}

#[test]
fn a_deferred_tool_is_refused_until_tool_search_loads_it() {
    let refused = run(
        json!([call("mcp__atl__whoami", json!({})), {"text": "done"}]),
        None,
    );
    assert!(
        refused.len() == 1 && refused[0].0 != "ok" && refused[0].1.contains("ToolSearch"),
        "{refused:?}"
    );
    let script = json!([
        call("ToolSearch", json!({"query": "select:mcp__atl__whoami"})),
        call("mcp__atl__whoami", json!({})),
        {"text": "done"}
    ]);
    let loaded = run(script.clone(), None);
    assert!(
        loaded.len() == 2 && loaded.iter().all(|r| r.0 == "ok"),
        "{loaded:?}"
    );
    // a repository settings file cannot turn tool search off
    let off = json!({"synthesis": {"mcp": {"toolSearch": "off"}}});
    let still = run(
        json!([call("mcp__atl__whoami", json!({})), {"text": "done"}]),
        Some(off),
    );
    assert!(
        still[0].0 != "ok" && still[0].1.contains("ToolSearch"),
        "{still:?}"
    );
}
