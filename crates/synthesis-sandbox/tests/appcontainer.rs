//! Real enforcement tests through the `synthesis-sandbox-exec --appcontainer` helper. Windows only.
//!
//! These are the evidence for the Windows backend and they have only ever run on CI
//! (`windows-latest`): the filesystem and network guarantees are `Weak` because of that. Each
//! denial has a positive control (the same operation works where it must) so a broken harness
//! cannot pass for a working sandbox.
#![cfg(windows)]

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use synthesis_sandbox::{
    AppContainerBackend, NetworkPolicy, SandboxBackend, SandboxPolicy, SandboxedCommand,
};

const HELPER: &str = env!("CARGO_BIN_EXE_synthesis-sandbox-exec");
const CMD: &str = r"C:\Windows\System32\cmd.exe";

fn backend() -> AppContainerBackend {
    AppContainerBackend::new(PathBuf::from(HELPER))
}

/// Workspace dirs live under target/, never the system temp dir (the secret goes there).
fn ws() -> tempfile::TempDir {
    tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap()
}

fn canon(p: &Path) -> PathBuf {
    dunce::canonicalize(p).unwrap()
}

fn pol(w: &Path) -> SandboxPolicy {
    SandboxPolicy {
        writable_roots: vec![canon(w)],
        env_passthrough: ["SystemRoot", "ComSpec", "PATHEXT", "PATH"]
            .map(String::from)
            .to_vec(),
        ..Default::default()
    }
}

fn wrapped(policy: &SandboxPolicy, cwd: &Path, prog: &str, args: &[&str]) -> Command {
    let w = backend()
        .wrap(
            policy,
            &SandboxedCommand {
                program: prog.into(),
                args: args.iter().map(Into::into).collect(),
                cwd: Some(cwd.to_path_buf()),
                ..Default::default()
            },
        )
        .unwrap();
    let mut c = Command::new(&w.program);
    c.args(&w.args)
        .env_clear()
        .envs(w.env.iter().cloned())
        .current_dir(cwd);
    c
}

fn run(policy: &SandboxPolicy, cwd: &Path, prog: &str, args: &[&str]) -> Output {
    wrapped(policy, cwd, prog, args).output().unwrap()
}

fn sh(policy: &SandboxPolicy, cwd: &Path, script: &str) -> Output {
    run(policy, cwd, CMD, &["/c", script])
}

fn text(o: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&o.stdout),
        String::from_utf8_lossy(&o.stderr)
    )
}

fn icacls(p: &Path) -> String {
    let o = Command::new(r"C:\Windows\System32\icacls.exe")
        .arg(p)
        .output()
        .unwrap();
    String::from_utf8_lossy(&o.stdout).into_owned()
}

#[test]
fn the_container_starts_and_the_workspace_is_writable() {
    let w = ws();
    let o = sh(&pol(w.path()), w.path(), "echo hi> in.txt && type in.txt");
    assert!(o.status.success(), "{}", text(&o));
    assert!(text(&o).contains("hi"));
    assert!(w.path().join("in.txt").exists());
}

#[test]
fn a_secret_outside_the_grants_is_unreadable_but_readable_without_the_sandbox() {
    let w = ws();
    let outside = tempfile::tempdir().unwrap(); // %TEMP%: private to the user, never granted
    let f = outside.path().join("key.txt");
    std::fs::write(&f, "TOPSECRET").unwrap();
    let script = format!("type {}", f.display());
    let control = Command::new(CMD).args(["/c", &script]).output().unwrap();
    assert!(
        text(&control).contains("TOPSECRET"),
        "control: {}",
        text(&control)
    );
    let o = sh(&pol(w.path()), w.path(), &script);
    assert!(
        !o.status.success() && !text(&o).contains("TOPSECRET"),
        "{}",
        text(&o)
    );
}

#[test]
fn writing_outside_the_grants_is_denied() {
    let (w, other) = (ws(), tempfile::tempdir().unwrap());
    let target = other.path().join("out.txt");
    let o = sh(
        &pol(w.path()),
        w.path(),
        &format!("echo x> {}", target.display()),
    );
    assert!(!o.status.success() && !target.exists(), "{}", text(&o));
    // the same directory is writable once it is granted
    let mut p = pol(w.path());
    p.writable_roots.push(canon(other.path()));
    let ok = sh(&p, w.path(), &format!("echo x> {}", target.display()));
    assert!(ok.status.success() && target.exists(), "{}", text(&ok));
}

#[test]
fn a_read_root_is_readable_not_writable() {
    let (w, tools) = (ws(), ws());
    std::fs::write(tools.path().join("t.txt"), "TOOL").unwrap();
    let mut p = pol(w.path());
    p.readable_roots.push(canon(tools.path()));
    let t = tools.path().join("t.txt");
    assert!(text(&sh(&p, w.path(), &format!("type {}", t.display()))).contains("TOOL"));
    let bad = sh(
        &p,
        w.path(),
        &format!("echo x> {}", tools.path().join("n.txt").display()),
    );
    assert!(!bad.status.success() && !tools.path().join("n.txt").exists());
}

/// A workspace with a denied dir, a read-only dir and a guarded parent, plus the policy for it.
fn guarded() -> (tempfile::TempDir, PathBuf, SandboxPolicy) {
    let w = ws();
    let r = canon(w.path());
    std::fs::create_dir_all(r.join("secret")).unwrap();
    std::fs::create_dir_all(r.join(".git/hooks")).unwrap();
    std::fs::write(r.join("secret/k"), "TOPSECRET").unwrap();
    std::fs::write(r.join("open"), "fine").unwrap();
    std::fs::write(r.join(".git/hooks/pre-commit"), "orig").unwrap();
    let mut p = pol(&r);
    p.deny_paths = vec![r.join("secret")];
    p.readonly_paths = vec![r.join(".git/hooks")];
    (w, r, p)
}

/// The `net_probe` example (built next to the helper); copied into `dir` so it may run there.
fn probe_in(dir: &Path) -> PathBuf {
    let from = Path::new(HELPER)
        .parent()
        .unwrap()
        .join("examples")
        .join("net_probe.exe");
    assert!(from.exists(), "build the example first: {}", from.display());
    let to = dir.join("net_probe.exe");
    std::fs::copy(from, &to).unwrap();
    to
}

/// The ACLs of the guarded paths while a container is running (what the helper really set),
/// read from the host side, for failure messages.
fn acl_dump(p: &SandboxPolicy, r: &Path) -> String {
    let probe = probe_in(r);
    let mut c = wrapped(p, r, probe.to_str().unwrap(), &["sleep", "6"]);
    let mut child = c.spawn().unwrap();
    std::thread::sleep(std::time::Duration::from_secs(2));
    let mut out = String::new();
    for rel in [
        "",
        "secret",
        r"secret\k",
        ".git",
        r".git\hooks",
        r".git\hooks\pre-commit",
    ] {
        out.push_str(&format!("-- {rel}\n{}\n", icacls(&r.join(rel))));
    }
    let _ = child.kill();
    let _ = child.wait();
    out
}

#[test]
fn a_denied_path_inside_a_writable_root_is_unreadable_and_unwritable() {
    let (_w, r, p) = guarded();
    assert!(text(&sh(&p, &r, "type open")).contains("fine"), "control");
    let s = sh(&p, &r, r"type secret\k");
    assert!(
        !s.status.success() && !text(&s).contains("TOPSECRET"),
        "{}\nACLs during the run:\n{}",
        text(&s),
        acl_dump(&p, &r)
    );
    assert!(!sh(&p, &r, r"echo x> secret\new").status.success());
    assert!(!r.join(r"secret\new").exists());
}

#[test]
fn a_readonly_path_inside_a_writable_root_is_readable_not_writable() {
    let (_w, r, p) = guarded();
    let dump = || acl_dump(&p, &r);
    assert!(
        text(&sh(&p, &r, r"type .git\hooks\pre-commit")).contains("orig"),
        "control"
    );
    assert!(
        !sh(&p, &r, r"echo evil> .git\hooks\pre-commit")
            .status
            .success(),
        "{}",
        dump()
    );
    assert!(
        !sh(&p, &r, r"del .git\hooks\pre-commit").status.success(),
        "{}",
        dump()
    );
    assert!(
        !sh(&p, &r, r"echo evil> .git\hooks\new-hook")
            .status
            .success(),
        "{}",
        dump()
    );
    assert_eq!(
        std::fs::read_to_string(r.join(".git/hooks/pre-commit"))
            .unwrap()
            .trim(),
        "orig"
    );
    assert!(!r.join(".git/hooks/new-hook").exists());
}

#[test]
fn the_children_of_a_guarded_directory_cannot_be_renamed_away() {
    let (_w, r, p) = guarded();
    let o = sh(&p, &r, r"ren .git\hooks hooks.bak");
    assert!(
        !o.status.success() && r.join(".git/hooks").exists(),
        "{}\n{}",
        text(&o),
        acl_dump(&p, &r)
    );
}

#[test]
fn no_network_the_hosts_loopback_included() {
    let w = ws();
    let probe = Path::new(env!("CARGO_BIN_EXE_synthesis-sandbox-exec"))
        .parent()
        .unwrap()
        .join("examples")
        .join("net_probe.exe");
    assert!(
        probe.exists(),
        "build the example first: {}",
        probe.display()
    );
    let local = probe_in(w.path()); // inside the workspace grant: executable there
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap(); // a "service" of the host
    let addr = listener.local_addr().unwrap().to_string();
    let connect = |c: &mut Command| c.args(["connect", &addr]).output().unwrap();
    let control = connect(&mut Command::new(&local));
    assert!(control.status.success(), "control: {}", text(&control));
    let o = connect(&mut wrapped(
        &pol(w.path()),
        w.path(),
        local.to_str().unwrap(),
        &[],
    ));
    // the program must have STARTED in the container (126 = the helper could not), and failed
    assert!(
        o.status.code().is_some_and(|c| c != 0 && c != 126),
        "{}",
        text(&o)
    );
    assert!(
        !text(&o).contains("synthesis-sandbox-exec:"),
        "{}",
        text(&o)
    );
    // outside the host: loopback inside the container itself may work, a remote address must not
    let r = run(
        &pol(w.path()),
        w.path(),
        local.to_str().unwrap(),
        &["loopback"],
    );
    assert!(
        r.status.success() && !text(&r).contains("REMOTE_CONNECTED"),
        "{}",
        text(&r)
    );
}

#[test]
fn loopback_and_proxied_are_refused_not_silently_weakened() {
    let w = ws();
    for net in [
        NetworkPolicy::Loopback,
        NetworkPolicy::Proxied(synthesis_sandbox::EgressEndpoint::Tcp {
            port: 1,
            token: String::new(),
        }),
    ] {
        let p = SandboxPolicy {
            network: net,
            ..pol(w.path())
        };
        let r = backend().wrap(
            &p,
            &SandboxedCommand {
                program: CMD.into(),
                ..Default::default()
            },
        );
        assert!(matches!(
            r,
            Err(synthesis_sandbox::SandboxError::Unsupported(_))
        ));
    }
}

#[test]
fn temp_is_private_and_the_policy_tmp_dir_is_writable() {
    let (w, t) = (ws(), ws());
    let mut p = pol(w.path());
    p.tmp_dir = Some(canon(t.path()));
    let o = sh(
        &p,
        w.path(),
        &format!(
            "echo %TEMP%> where.txt && echo y> {}",
            t.path().join("made.txt").display()
        ),
    );
    assert!(
        o.status.success() && t.path().join("made.txt").exists(),
        "{}",
        text(&o)
    );
    // the system gives each container its own TEMP; it is never the user's
    let got = std::fs::read_to_string(w.path().join("where.txt")).unwrap();
    let host = std::env::temp_dir().display().to_string().to_lowercase();
    assert!(
        !got.trim()
            .to_lowercase()
            .starts_with(host.trim_end_matches('\\')),
        "TEMP in the container: {got}"
    );
}

#[test]
fn grandchildren_stay_confined() {
    let (w, other) = (ws(), tempfile::tempdir().unwrap());
    let target = other.path().join("g.txt");
    let s = format!("{CMD} /c echo x> {}", target.display());
    let _ = sh(&pol(w.path()), w.path(), &s);
    assert!(!target.exists());
    // control: the same nested command writes there once the directory is granted
    let mut p = pol(w.path());
    p.writable_roots.push(canon(other.path()));
    let ok = sh(&p, w.path(), &s);
    assert!(target.exists(), "control: {}", text(&ok));
}

#[test]
fn the_process_limit_applies_when_exec_is_not_allowed() {
    let w = ws();
    let nested = format!("{CMD} /c echo ok> nested.txt");
    // control: a child process is allowed by default
    // (the outer cmd creates nested.txt for the redirect even when the inner one cannot start,
    // so the CONTENT is the evidence)
    let said = |p: &SandboxPolicy| {
        let o = sh(p, w.path(), &nested);
        let got = std::fs::read_to_string(w.path().join("nested.txt")).unwrap_or_default();
        let _ = std::fs::remove_file(w.path().join("nested.txt"));
        (got.trim().to_string(), text(&o))
    };
    let (got, msg) = said(&pol(w.path()));
    assert_eq!(got, "ok", "control: {msg}");
    let mut p = pol(w.path());
    p.allow_exec = false; // Job Object ActiveProcessLimit = 1
    let (got, msg) = said(&p);
    assert_ne!(got, "ok", "the nested process ran: {msg}");
}

#[test]
fn acls_are_restored_after_the_run() {
    let w = ws();
    let before = icacls(w.path());
    let o = sh(&pol(w.path()), w.path(), "echo hi> made.txt");
    assert!(o.status.success(), "{}", text(&o));
    // no per-run container SID (S-1-15-2-...) left on the directory or on the file it created
    for p in [w.path().to_path_buf(), w.path().join("made.txt")] {
        let a = icacls(&p);
        assert!(
            !a.contains("S-1-15-2-") && !a.contains("APPLICATION PACKAGE"),
            "{a}"
        );
    }
    assert_eq!(
        before.replace(&w.path().display().to_string(), ""),
        icacls(w.path()).replace(&w.path().display().to_string(), "")
    );
}

#[test]
fn the_helper_fails_closed() {
    // a granted root that does not exist: nothing runs, exit 126
    let w = ws();
    let mut p = pol(w.path());
    p.writable_roots.push(w.path().join("does-not-exist"));
    let o = sh(&p, w.path(), "echo hi> ran.txt");
    assert_eq!(o.status.code(), Some(126), "{}", text(&o));
    assert!(!w.path().join("ran.txt").exists());
    // a bad spec or missing arguments
    for args in [
        vec![],
        vec!["--appcontainer", "{}"],
        vec!["--appcontainer", "not json"],
        vec!["x"],
    ] {
        let o = Command::new(HELPER).args(args).output().unwrap();
        assert_eq!(o.status.code(), Some(126), "{}", text(&o));
    }
}

#[test]
fn the_exit_code_comes_through() {
    let w = ws();
    assert_eq!(
        sh(&pol(w.path()), w.path(), "exit 7").status.code(),
        Some(7)
    );
}
