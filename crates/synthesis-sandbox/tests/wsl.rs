//! WSL hardening against a real bubblewrap. Linux only; skipped (printed) when bwrap is unusable.
//! A fake WSL host is injected (`BwrapBackend::wrap_on`): a fake interop variable, a fake Windows
//! drive in a temp dir and a Windows directory on PATH. No real WSL is needed, and none was used.
//! NOT RUN on the macOS dev machine that wrote it (compiled via --target x86_64-unknown-linux-gnu).
#![cfg(target_os = "linux")]

use std::path::Path;
use std::process::{Command, Output};

use synthesis_platform::wsl::{WslHost, WslVersion};
use synthesis_sandbox::{BwrapBackend, NetworkPolicy, SandboxPolicy, SandboxedCommand};

fn sh(drive: &Path, ws: &Path, script: &str) -> Option<Output> {
    let b = BwrapBackend::detect().or_else(|| {
        eprintln!("skipped: bubblewrap is not usable here");
        None
    })?;
    let host = WslHost {
        version: WslVersion::V2,
        windows_mounts: vec![drive.to_path_buf()],
    };
    let path = std::env::join_paths([
        "/usr/bin",
        "/bin",
        &drive.join("Windows").display().to_string(),
    ])
    .unwrap();
    let cmd = SandboxedCommand {
        program: "/bin/sh".into(),
        args: vec!["-c".into(), script.into()],
        env: vec![
            ("WSL_INTEROP".into(), "/run/WSL/1_interop".into()),
            ("WSLENV".into(), "X".into()),
            ("PATH".into(), path),
        ],
        cwd: Some(ws.to_path_buf()),
        ..Default::default()
    };
    let pol = SandboxPolicy {
        writable_roots: vec![ws.to_path_buf()],
        network: NetworkPolicy::Allow,
        ..Default::default()
    };
    let w = b.wrap_on(&pol, &cmd, Some(&host)).unwrap();
    Some(
        Command::new(&w.program)
            .args(&w.args)
            .env_clear()
            .envs(w.env.iter().cloned())
            .output()
            .unwrap(),
    )
}

fn fake_drive() -> (tempfile::TempDir, std::path::PathBuf, std::path::PathBuf) {
    let t = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let drive = t.path().canonicalize().unwrap().join("c");
    let ws = drive.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    (t, drive, ws)
}

#[test]
fn child_has_no_interop_env_or_windows_path_and_empty_run() {
    let (_t, drive, ws) = fake_drive();
    let Some(o) = sh(
        &drive,
        &ws,
        "echo \"interop=${WSL_INTEROP-unset} env=${WSLENV-unset}\"; echo \"path=$PATH\"; echo \"run=$(ls -A /run | wc -l)\"",
    ) else {
        return;
    };
    let out = String::from_utf8_lossy(&o.stdout);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    assert!(out.contains("interop=unset env=unset"), "{out}");
    assert!(out.contains("path=/usr/bin:/bin\n"), "{out}");
    assert!(
        out.contains("run=0"),
        "/run is a fresh tmpfs (so /run/WSL cannot exist): {out}"
    );
}

#[test]
fn drive_is_readonly_except_the_workspace_inside_it() {
    let (_t, drive, ws) = fake_drive();
    let script = format!(
        "echo a > {ws}/ok.txt && echo ws-written; (echo b > {d}/evil.txt) 2>/dev/null || echo drive-denied",
        ws = ws.display(),
        d = drive.display()
    );
    let Some(o) = sh(&drive, &ws, &script) else {
        return;
    };
    let out = String::from_utf8_lossy(&o.stdout);
    assert!(
        out.contains("ws-written") && out.contains("drive-denied"),
        "{out}"
    );
    assert!(ws.join("ok.txt").exists() && !drive.join("evil.txt").exists());
}

/// Needs a real WSL2 distro with interop enabled; run by hand there: `cargo test -p synthesis-sandbox -- --ignored`.
#[test]
#[ignore = "needs a real WSL2 distro with interop enabled and bubblewrap"]
fn real_wsl_cmd_exe_is_unreachable_in_the_sandbox() {
    let host = WslHost::real().expect("not running in WSL");
    let t = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let ws = t.path().canonicalize().unwrap();
    let b = BwrapBackend::detect().expect("bubblewrap not usable");
    let cmd = SandboxedCommand {
        program: "/bin/sh".into(),
        args: vec![
            "-c".into(),
            "/mnt/c/Windows/System32/cmd.exe /c echo pwned".into(),
        ],
        cwd: Some(ws.clone()),
        ..Default::default()
    };
    let pol = SandboxPolicy {
        writable_roots: vec![ws],
        ..Default::default()
    };
    let w = b.wrap_on(&pol, &cmd, Some(host)).unwrap();
    let o = Command::new(&w.program)
        .args(&w.args)
        .env_clear()
        .output()
        .unwrap();
    assert!(!String::from_utf8_lossy(&o.stdout).contains("pwned"));
}
