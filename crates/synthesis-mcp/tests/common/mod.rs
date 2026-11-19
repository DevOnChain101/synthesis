#![allow(dead_code)]
use std::path::PathBuf;
use std::time::{Duration, Instant};

use serde_json::Value;
use synthesis_config::Env;
use synthesis_mcp::{McpManager, McpOptions, McpServerConfig, McpToolResult};
use tokio_util::sync::CancellationToken;

pub fn echo_server_path() -> PathBuf {
    example_path("echo_server")
}

pub fn example_path(name: &str) -> PathBuf {
    // The final example is in target/<profile>/examples, an ancestor's child: the test exe is in
    // target/<profile>/deps (old layout) or target/<profile>/build/<pkg>/<hash> (new build-dir layout).
    let file = if cfg!(windows) {
        format!("{name}.exe")
    } else {
        name.to_string()
    };
    let exe = std::env::current_exe().unwrap();
    exe.ancestors()
        .map(|a| a.join("examples").join(&file))
        .find(|p| p.exists())
        .unwrap_or_else(|| {
            panic!(
                "build the example first: {file} not found above {}",
                exe.display()
            )
        })
}

pub fn echo_cfg(name: &str, extra: &[&str]) -> McpServerConfig {
    McpServerConfig::stdio(
        name,
        echo_server_path().to_string_lossy(),
        extra.iter().map(|s| s.to_string()).collect(),
    )
}

pub fn opts(env: Env) -> McpOptions {
    McpOptions {
        cwd: std::env::temp_dir(),
        env,
        ..McpOptions::default()
    }
}

/// PATH (needed by `sleep` in `spawn_child`) is the only ambient value the tests inject.
pub fn base_env() -> Env {
    Env::from_pairs([
        ("PATH", std::env::var("PATH").unwrap_or_default()),
        (
            "SystemRoot",
            std::env::var("SystemRoot").unwrap_or_default(),
        ),
    ])
}

pub async fn call(m: &McpManager, server: &str, tool: &str, args: Value) -> McpToolResult {
    m.call_tool(server, tool, args, &CancellationToken::new())
        .await
        .unwrap()
}

pub fn pid_alive(pid: u32) -> bool {
    use std::process::{Command, Stdio};
    if cfg!(windows) {
        let out = Command::new("tasklist")
            .args(["/FI", &format!("PID eq {pid}"), "/NH"])
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout).contains(&pid.to_string())
    } else {
        Command::new("kill")
            .args(["-0", &pid.to_string()])
            .stderr(Stdio::null())
            .status()
            .unwrap()
            .success()
    }
}

pub async fn wait_until(what: &str, mut f: impl FnMut() -> bool) {
    let start = Instant::now();
    while !f() {
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "timed out waiting for {what}"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}
