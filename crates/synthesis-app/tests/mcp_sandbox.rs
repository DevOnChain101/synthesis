//! Stdio MCP servers run under the command sandbox: the same `confine` closure `synt run` hands
//! to the MCP manager, driven with a real shell. Unix only (bubblewrap on Linux, Seatbelt on
//! macOS); skipped where no sandbox can run, unless SYNT_REQUIRE_BWRAP=1 (then it fails).
#![cfg(unix)]

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::Path;
use std::process::Command;
use std::sync::{Arc, Mutex};

use synthesis_app::mcp::{ServerNet, ServerSandbox, confine};
use synthesis_mcp::{Confine, McpServerConfig};
use synthesis_sandbox::{Guarantee, SandboxPolicy, SandboxedCommand};

fn build(
    work: &Path,
    tmp_root: &Path,
    cfg: &McpServerConfig,
    rules: ServerSandbox,
    strict: bool,
) -> (Confine, Arc<Mutex<Vec<String>>>) {
    let mut base = SandboxPolicy::standard(work, &tmp_root.join("cmd"));
    base.required = Guarantee::Weak;
    let fallbacks = Arc::<Mutex<Vec<String>>>::default();
    let f = confine(
        base,
        work.to_path_buf(),
        tmp_root.join("mcp"),
        std::slice::from_ref(cfg),
        HashMap::from([(cfg.name.clone(), rules)]),
        strict,
        Default::default(),
        fallbacks.clone(),
    );
    (f, fallbacks)
}

/// Launch `script` the way the manager would; `None` = the launch was refused. Like the manager
/// it runs the closure INSIDE a tokio runtime (an egress proxy started there would panic).
fn launch(f: &Confine, cfg: &McpServerConfig, work: &Path, script: &str) -> Option<(bool, String)> {
    let cmd = SandboxedCommand {
        program: "/bin/bash".into(),
        args: vec!["-c".into(), script.into()],
        env: vec![("PATH".into(), "/usr/bin:/bin".into())],
        cwd: Some(work.to_path_buf()),
        clear_env: true,
    };
    let rt = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();
    let w = rt.block_on(async { f.0(cfg, cmd) }).ok()?;
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

fn run(
    work: &Path,
    tmp_root: &Path,
    rules: ServerSandbox,
    strict: bool,
    script: &str,
) -> Option<(bool, String)> {
    let cfg = McpServerConfig::stdio("srv", "bash", vec![]);
    let (f, _) = build(work, tmp_root, &cfg, rules, strict);
    launch(&f, &cfg, work, script)
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
    // best-effort falls back to unconfined instead, and says so (reported as a notice)
    let cfg = McpServerConfig::stdio("srv", "bash", vec![]);
    let (f, fallbacks) = build(&work, &tmp, &cfg, bad, false);
    assert!(launch(&f, &cfg, &work, "echo x").is_some());
    assert!(fallbacks.lock().unwrap()[0].contains("UNCONFINED"));
}

fn tmp_tree() -> (std::path::PathBuf, std::path::PathBuf, std::path::PathBuf) {
    let root = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let root = std::fs::canonicalize(root.keep()).unwrap();
    let (work, tmp) = (root.join("ws"), root.join("t"));
    for d in [&work, &tmp] {
        std::fs::create_dir_all(d).unwrap();
    }
    (root, work, tmp)
}

#[test]
fn a_writable_root_over_the_workspace_keeps_git_hooks_and_config_read_only() {
    let (_root, work, tmp) = tmp_tree();
    std::fs::create_dir_all(work.join(".git/hooks")).unwrap();
    std::fs::write(work.join(".git/config"), "[core]").unwrap();
    if !usable(&work, &tmp) {
        return;
    }
    let rules = ServerSandbox {
        writable: vec![work.clone()],
        ..Default::default()
    };
    let probe = "echo x > ok && echo ws-ok; echo x > .git/hooks/pre-commit 2>/dev/null && echo HOOK; echo x >> .git/config 2>/dev/null && echo CONFIG";
    let (_, out) = run(&work, &tmp, rules, true, probe).unwrap();
    assert!(out.contains("ws-ok"), "positive control: {out}");
    assert!(!out.contains("HOOK") && !out.contains("CONFIG"), "{out}");
}

#[test]
fn a_project_server_stays_confined_whatever_the_user_set_for_its_name() {
    let (_root, work, tmp) = tmp_tree();
    if !usable(&work, &tmp) {
        return;
    }
    let mut cfg = McpServerConfig::stdio("srv", "bash", vec![]);
    cfg.scope = Some("project".into());
    let off = ServerSandbox {
        off: true,
        network: ServerNet::Full,
        ..Default::default()
    };
    let (f, _) = build(&work, &tmp, &cfg, off, true);
    let (_, out) = launch(
        &f,
        &cfg,
        &work,
        "echo x > evil 2>/dev/null && echo WROTE; echo end",
    )
    .unwrap();
    assert!(out.contains("end") && !out.contains("WROTE") && !work.join("evil").exists());
}

fn http_hello() -> u16 {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = l.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for mut s in l.incoming().flatten() {
            let mut b = [0u8; 1024];
            let _ = s.read(&mut b);
            let _ = s.write_all(
                b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\nConnection: close\r\n\r\nhello",
            );
        }
    });
    port
}

/// `network: hosts` end to end with the real per-server proxy, launched from inside a runtime.
/// Linux needs the sandbox helper next to `synt`, which a test binary does not have: there the
/// launch is refused (fail closed) and the test only checks that.
#[test]
fn network_hosts_reaches_only_the_listed_host_through_the_proxy() {
    let (_root, work, tmp) = tmp_tree();
    if !usable(&work, &tmp) {
        return;
    }
    let (allowed, other) = (http_hello(), http_hello());
    let rules = ServerSandbox {
        network: ServerNet::Hosts,
        hosts: vec![format!("127.0.0.1:{allowed}")],
        ..Default::default()
    };
    let curl = |port: u16, extra: &str| {
        format!("curl -sS --max-time 8 {extra} http://127.0.0.1:{port}/ 2>/dev/null")
    };
    let probe = format!(
        "{}; echo; {} >/dev/null && echo OTHER; {} >/dev/null && echo DIRECT",
        curl(allowed, "--noproxy '' -p"),
        curl(other, "--noproxy '' -p"),
        curl(allowed, "--noproxy '*'"),
    );
    let Some((_, out)) = run(&work, &tmp, rules, true, &probe) else {
        if !cfg!(target_os = "linux") {
            panic!("the launch must work where the proxy can be plumbed");
        }
        eprintln!("skipped: no helper next to the test binary (Linux)");
        return;
    };
    assert!(
        out.contains("hello"),
        "positive control, through the proxy: {out}"
    );
    assert!(!out.contains("OTHER"), "a host that is not listed: {out}");
    assert!(!out.contains("DIRECT"), "no route around the proxy: {out}");
}
