//! WSL hardening that needs the helper binary. Linux only.
//! NOT RUN on the macOS dev machine that wrote it (compiled via --target x86_64-unknown-linux-gnu).
//! The real-WSL2 test is `#[ignore]`: interop closure is NOT proven until it has been run on WSL2.
#![cfg(target_os = "linux")]

use std::process::Command;

use synthesis_sandbox::{
    BwrapBackend, NetworkPolicy, SandboxBackend, SandboxPolicy, SandboxedCommand,
};

const HELPER: &str = env!("CARGO_BIN_EXE_synthesis-sandbox-exec");

fn have_python() -> bool {
    Command::new("python3")
        .arg("-c")
        .arg("pass")
        .status()
        .is_ok_and(|s| s.success())
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

/// `socket(family, SOCK_STREAM)` through the helper's seccomp filter: the errno, or 0.
fn socket_errno(family: u32) -> i32 {
    let py = format!(
        "import socket,sys\ntry:\n socket.socket({family},socket.SOCK_STREAM)\nexcept OSError as e:\n sys.exit(e.errno)\n"
    );
    let o = Command::new(HELPER)
        .args(["--no-vsock", "--", "python3", "-c", &py])
        .output()
        .unwrap();
    assert_ne!(
        o.status.code(),
        Some(126),
        "seccomp not applied: {}",
        String::from_utf8_lossy(&o.stderr)
    );
    o.status.code().unwrap()
}

#[test]
fn helper_denies_af_vsock_and_keeps_other_families() {
    if !require(have_python(), "python3") {
        return;
    }
    assert_eq!(socket_errno(40), 97, "AF_VSOCK must fail with EAFNOSUPPORT");
    assert_eq!(socket_errno(1), 0, "AF_UNIX must still work");
    assert_eq!(socket_errno(2), 0, "AF_INET must still work");
}

#[test]
fn helper_refuses_to_exec_when_the_command_is_missing() {
    let o = Command::new(HELPER)
        .args(["--no-vsock", "--", "/nonexistent-synt-cmd"])
        .output()
        .unwrap();
    assert_eq!(o.status.code(), Some(127));
}

/// Needs a real WSL2 distro with interop enabled, bubblewrap, python3 and a mounted `C:`; run by
/// hand there: `cargo test -p synthesis-sandbox --test wsl -- --ignored --nocapture`.
#[test]
#[ignore = "needs a real WSL2 distro with interop enabled, bubblewrap and python3"]
fn real_wsl2_sandbox_closes_interop() {
    let t = tempfile::tempdir().unwrap();
    let ws = t.path().canonicalize().unwrap();
    let b = BwrapBackend::with_helper(HELPER.into()).expect("bubblewrap not usable");
    let script = "cmd=/mnt/c/Windows/System32/cmd.exe; \
        [ -e $cmd ] && echo CMD-VISIBLE; \
        ($cmd /c echo pwned) 2>&1; \
        (echo x > /mnt/c/synt-wsl-test.txt) 2>/dev/null && echo DRIVE-WRITTEN; \
        python3 -c 'import socket;socket.socket(40,1)' 2>&1 | grep -q 'not supported' && echo VSOCK-DENIED";
    let cmd = SandboxedCommand {
        program: "/bin/sh".into(),
        args: vec!["-c".into(), script.into()],
        cwd: Some(ws.clone()),
        ..Default::default()
    };
    let pol = SandboxPolicy {
        writable_roots: vec![ws],
        network: NetworkPolicy::Deny,
        ..Default::default()
    };
    let w = b.wrap(&pol, &cmd).unwrap();
    let o = Command::new(&w.program)
        .args(&w.args)
        .env_clear()
        .envs(w.env.iter().cloned())
        .output()
        .unwrap();
    let out = String::from_utf8_lossy(&o.stdout);
    assert!(
        !out.contains("pwned") && !out.contains("CMD-VISIBLE"),
        "cmd.exe reachable: {out}"
    );
    assert!(!out.contains("DRIVE-WRITTEN"), "wrote /mnt/c: {out}");
    assert!(out.contains("VSOCK-DENIED"), "AF_VSOCK not denied: {out}");
    assert!(!std::path::Path::new("/mnt/c/synt-wsl-test.txt").exists());
}
