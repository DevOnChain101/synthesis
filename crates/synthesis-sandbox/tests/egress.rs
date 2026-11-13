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
    run_in_denying(sock, work, program, args, &[])
}

fn run_in_denying(
    sock: &Path,
    work: &Path,
    program: &str,
    args: &[&str],
    deny: &[&Path],
) -> Output {
    let bwrap = BwrapBackend::with_helper(HELPER.into()).expect("bubblewrap");
    let policy = SandboxPolicy {
        writable_roots: vec![work.to_path_buf()],
        deny_paths: deny.iter().map(|p| p.to_path_buf()).collect(),
        network: NetworkPolicy::Proxied(EgressEndpoint::Unix(sock.to_path_buf())),
        env_passthrough: vec!["PATH".into(), "HTTPS_PROXY".into()],
        ..Default::default()
    };
    assert_eq!(
        bwrap.network_guarantee(&policy.network),
        synthesis_sandbox::Guarantee::Weak // not Strong: see the pathname-socket test below
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

fn python() -> Option<String> {
    tool("python3")
}

/// Connects to the AF_UNIX path in argv[1]; prints `ok` or `errno=<n>`.
const UNIX_CONNECT_PY: &str = "import socket,sys\ns=socket.socket(socket.AF_UNIX)\ntry:\n s.connect(sys.argv[1])\n print('ok')\nexcept OSError as e:\n print('errno=%d' % e.errno)\n";
const SOCKET_PY: &str = "import socket,sys\ntry:\n socket.socket(int(sys.argv[1]),socket.SOCK_STREAM)\n print('errno=0')\nexcept OSError as e:\n print('errno=%d' % e.errno)\n";

fn unix_listener(dir: &Path) -> (std::os::unix::net::UnixListener, std::path::PathBuf) {
    let path = dir.join("host.sock");
    let l = std::os::unix::net::UnixListener::bind(&path).unwrap();
    std::thread::spawn({
        let l = l.try_clone().unwrap();
        move || for _ in l.incoming() {}
    });
    (l, path)
}

#[test]
fn vsock_is_denied_inside_a_proxied_run_but_inet_and_unix_work() {
    let Some(py) = python() else {
        require(false, "python3");
        return;
    };
    if !require(BwrapBackend::detect().is_some(), "bubblewrap") {
        return;
    }
    let run_dir = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    let sock = run_dir.path().join("egress.sock");
    let errno = |family: &str| {
        let o = run_in(&sock, work.path(), &py, &["-c", SOCKET_PY, family]);
        String::from_utf8_lossy(&o.stdout).trim().to_owned()
    };
    assert_eq!(
        errno("40"),
        "errno=1",
        "AF_VSOCK must fail with EPERM (seccomp, not a missing module)"
    );
    assert_eq!(
        errno("2"),
        "errno=0",
        "AF_INET still works (loopback bridge)"
    );
    assert_eq!(errno("1"), "errno=0", "AF_UNIX still works");
}

/// What `--ro-bind / /` exposes, precisely: /tmp, /run, /var/run and the denied paths are masked
/// (a host socket there is unreachable); a pathname socket ANYWHERE else on the host filesystem
/// (the workspace parent, `$HOME` outside the denied credential dirs) can still be connect()ed:
/// a read-only bind does not stop connect(). That is why Proxied is Weak on Linux until the read
/// allow-list lands. The last assertion pins the residual: when it starts failing, the allow-list
/// landed; raise the guarantee and update the docs.
#[test]
fn host_pathname_unix_sockets_are_masked_only_in_tmp_run_and_denied_paths() {
    let Some(py) = python() else {
        require(false, "python3");
        return;
    };
    if !require(BwrapBackend::detect().is_some(), "bubblewrap") {
        return;
    }
    let run_dir = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    let proxy_sock = run_dir.path().join("egress.sock");
    let in_tmp = tempfile::tempdir().unwrap(); // the system temp dir (/tmp)
    let denied = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let exposed = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let reach = |dir: &Path, deny: &[&Path]| {
        let (_l, path) = unix_listener(dir);
        let o = run_in_denying(
            &proxy_sock,
            work.path(),
            &py,
            &["-c", UNIX_CONNECT_PY, path.to_str().unwrap()],
            deny,
        );
        String::from_utf8_lossy(&o.stdout).trim().to_owned()
    };
    assert_ne!(
        reach(in_tmp.path(), &[]),
        "ok",
        "a socket under /tmp must be hidden"
    );
    assert_ne!(
        reach(denied.path(), &[denied.path()]),
        "ok",
        "a socket in a denied dir must be hidden"
    );
    assert_eq!(
        reach(exposed.path(), &[]),
        "ok",
        "residual risk: other host pathname sockets are reachable"
    );
}
