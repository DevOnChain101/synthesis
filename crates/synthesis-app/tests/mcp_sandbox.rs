//! Stdio MCP servers run under the command sandbox: the same `confine` closure `synt run` hands
//! to the MCP manager, driven with a real shell. Unix only (bubblewrap on Linux, Seatbelt on
//! macOS); skipped where no sandbox can run, unless SYNT_REQUIRE_BWRAP=1 (then it fails).
#![cfg(unix)]

use std::collections::HashMap;
use std::net::TcpListener;
use std::path::Path;
use std::process::Command;

use synthesis_app::mcp::{ServerNet, ServerSandbox, confine};
use synthesis_mcp::McpServerConfig;
use synthesis_sandbox::{Guarantee, SandboxPolicy, SandboxedCommand};

/// Run `script` the way the manager would launch server `srv`; `None` = the launch was refused.
fn run(
    work: &Path,
    tmp_root: &Path,
    rules: ServerSandbox,
    strict: bool,
    script: &str,
) -> Option<(bool, String)> {
    let mut base = SandboxPolicy::standard(work, &tmp_root.join("cmd"));
    base.required = Guarantee::Weak;
    let f = confine(
        base,
        work.to_path_buf(),
        tmp_root.join("mcp"),
        HashMap::from([("srv".to_string(), rules)]),
        strict,
        Default::default(),
    );
    let cmd = SandboxedCommand {
        program: "/bin/bash".into(),
        args: vec!["-c".into(), script.into()],
        env: vec![("PATH".into(), "/usr/bin:/bin".into())],
        cwd: Some(work.to_path_buf()),
        clear_env: true,
    };
    let w = f.0(&McpServerConfig::stdio("srv", "bash", vec![]), cmd).ok()?;
    let mut c = Command::new(&w.program);
    c.args(&w.args).current_dir(work);
    if w.clear_env {
        c.env_clear();
    }
    let o = c.envs(w.env.iter().cloned()).output().unwrap();
    Some((
        o.status.success(),
        String::from_utf8_lossy(&o.stdout).into_owned(),
    ))
}

fn usable(work: &Path, tmp: &Path) -> bool {
    let ok = run(work, tmp, ServerSandbox::default(), true, "echo hi")
        .is_some_and(|(_, out)| out.trim() == "hi");
    assert!(
        ok || std::env::var_os("SYNT_REQUIRE_BWRAP").is_none_or(|v| v.is_empty()),
        "SYNT_REQUIRE_BWRAP is set but no sandbox can run a server here"
    );
    if !ok {
        eprintln!("skipped: no usable sandbox");
    }
    ok
}

#[test]
fn a_confined_server_reads_the_workspace_writes_only_its_temp_and_has_no_network() {
    // not under /tmp: bubblewrap masks /tmp, so a /tmp work dir would hide path mistakes
    let root = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let root = std::fs::canonicalize(root.path()).unwrap();
    let (work, tmp, extra) = (root.join("ws"), root.join("t"), root.join("declared"));
    for d in [&work, &tmp, &extra] {
        std::fs::create_dir_all(d).unwrap();
    }
    std::fs::write(work.join("file"), "secret-ws").unwrap();
    if !usable(&work, &tmp) {
        return;
    }
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = l.local_addr().unwrap().port();
    let probe = format!(
        r#"cat file; echo; echo x > "$TMPDIR/t" && echo tmp-ok; echo x > evil 2>/dev/null && echo ws-write; echo x > '{}/d' 2>/dev/null && echo declared-write; (exec 3<>/dev/tcp/127.0.0.1/{port}) 2>/dev/null && echo net"#,
        extra.display()
    );
    let go = |rules| run(&work, &tmp, rules, true, &probe).unwrap().1;

    let out = go(ServerSandbox::default());
    assert!(out.contains("secret-ws"), "positive control (read): {out}");
    assert!(out.contains("tmp-ok"), "positive control (temp): {out}");
    assert!(
        !out.contains("ws-write"),
        "workspace must be read-only: {out}"
    );
    assert!(
        !out.contains("declared-write"),
        "undeclared dir is not writable: {out}"
    );
    assert!(!out.contains("net"), "default network is none: {out}");
    assert!(!work.join("evil").exists());

    // positive controls for the same probe: the declared directory and `full` network work
    let out = go(ServerSandbox {
        network: ServerNet::Full,
        writable: vec![extra.clone()],
        ..Default::default()
    });
    assert!(out.contains("declared-write"), "declared writable: {out}");
    assert!(out.contains("net"), "network full reaches loopback: {out}");
}

#[test]
fn the_opt_out_runs_the_command_unchanged_and_hosts_without_hosts_fails_closed() {
    let root = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let root = std::fs::canonicalize(root.path()).unwrap();
    let (work, tmp) = (root.join("ws"), root.join("t"));
    for d in [&work, &tmp] {
        std::fs::create_dir_all(d).unwrap();
    }
    if !usable(&work, &tmp) {
        return;
    }
    let off = ServerSandbox {
        off: true,
        ..Default::default()
    };
    let (_, out) = run(&work, &tmp, off, true, "echo x > evil && echo wrote").unwrap();
    assert!(out.contains("wrote") && work.join("evil").exists());

    let bad = ServerSandbox {
        network: ServerNet::Hosts,
        ..Default::default()
    };
    assert!(
        run(&work, &tmp, bad.clone(), true, "echo x").is_none(),
        "strict refuses"
    );
    // best-effort falls back to unconfined (loudly) instead
    assert!(run(&work, &tmp, bad, false, "echo x").is_some());
}
