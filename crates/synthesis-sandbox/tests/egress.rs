//! `NetworkPolicy::Proxied` end to end with the real egress proxy: the command can reach the
//! allowed local literal through the proxy and nothing else. Linux (bubblewrap + helper bridge).
//! Set SYNT_REQUIRE_BWRAP=1 to fail instead of skip when bubblewrap or curl is unusable.
#![cfg(target_os = "linux")]

use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::Path;
use std::process::{Command, Output};
use std::sync::Arc;

use synthesis_egress::{Allowlist, Proxy, system_lookup};
use synthesis_sandbox::{
    BwrapBackend, EgressEndpoint, NetworkPolicy, SandboxBackend, SandboxPolicy, SandboxedCommand,
};

const HELPER: &str = env!("CARGO_BIN_EXE_synthesis-sandbox-exec");

fn tool(name: &str) -> Option<String> {
    ["/usr/bin", "/bin"]
        .iter()
        .map(|d| format!("{d}/{name}"))
        .find(|p| Path::new(p).is_file())
}

fn require(ok: bool, what: &str) -> bool {
    if !ok {
        assert!(
            std::env::var_os("SYNT_REQUIRE_BWRAP").is_none_or(|v| v.is_empty()),
            "SYNT_REQUIRE_BWRAP is set but {what} is not usable here"
        );
        eprintln!("skipped: {what} is not usable here");
    }
    ok
}

/// A host HTTP server that answers every request with `hello`.
fn http_server() -> u16 {
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

fn run_in(sock: &Path, work: &Path, program: &str, args: &[&str]) -> Output {
    let bwrap = BwrapBackend::with_helper(HELPER.into()).expect("bubblewrap");
    let policy = SandboxPolicy {
        writable_roots: vec![work.to_path_buf()],
        network: NetworkPolicy::Proxied(EgressEndpoint::Unix(sock.to_path_buf())),
        env_passthrough: vec!["PATH".into(), "HTTPS_PROXY".into()],
        ..Default::default()
    };
    assert_eq!(
        bwrap.network_guarantee(&policy.network),
        synthesis_sandbox::Guarantee::Strong
    );
    let c = SandboxedCommand {
        program: program.into(),
        args: args.iter().map(Into::into).collect(),
        cwd: Some(work.to_path_buf()),
        ..Default::default()
    };
    let w = bwrap.wrap(&policy, &c).unwrap();
    Command::new(&w.program)
        .args(&w.args)
        .env_clear()
        .envs(w.env.iter().cloned())
        // a hostile host proxy setting must not reach the command
        .env("HTTPS_PROXY", "http://127.0.0.1:1")
        .output()
        .unwrap()
}

#[test]
fn only_the_allowed_literal_is_reachable_through_the_bridge() {
    let (Some(curl), Some(getent)) = (tool("curl"), tool("getent")) else {
        require(false, "curl and getent");
        return;
    };
    if !require(BwrapBackend::detect().is_some(), "bubblewrap") {
        return;
    }
    let port = http_server();
    let other = http_server();
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();
    let run_dir = tempfile::tempdir().unwrap(); // 0700
    let work = tempfile::tempdir().unwrap();
    let sock = run_dir.path().join("egress.sock");
    let allow = Allowlist::parse(&[format!("127.0.0.1:{port}")]).unwrap();
    let _proxy = rt
        .block_on(async {
            Proxy::new(allow, system_lookup(), Arc::new(|_| {}))
                .serve_unix(&sock)
                .await
        })
        .unwrap();
    let go = |args: &[&str]| run_in(&sock, work.path(), &curl, args);
    let url = format!("http://127.0.0.1:{port}/");

    // through the bridge and the proxy (a tunnel: NO_PROXY would otherwise skip 127.0.0.1)
    let o = go(&["-sS", "--max-time", "10", "--noproxy", "", "-p", &url]);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    assert_eq!(o.stdout, b"hello");
    // another host port is not allowed: the proxy answers 403
    let o = go(&[
        "-sS",
        "--max-time",
        "10",
        "--noproxy",
        "",
        "-p",
        &format!("http://127.0.0.1:{other}/"),
    ]);
    assert!(!o.status.success());
    // no direct route: neither the Internet nor the host's loopback
    for direct in ["http://1.1.1.1/".to_string(), url.clone()] {
        let o = go(&["-sS", "--max-time", "5", "--noproxy", "*", &direct]);
        assert!(
            !o.status.success(),
            "{direct} must be unreachable without the proxy"
        );
    }
    // no DNS either
    let o = run_in(&sock, work.path(), &getent, &["hosts", "example.com"]);
    assert!(!o.status.success());
}
