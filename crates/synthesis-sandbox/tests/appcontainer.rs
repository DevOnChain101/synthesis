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
    // not the default (`~/.synt` on the runner: a marker that other tests' project-root discovery finds)
    let shared = Path::new(env!("CARGO_TARGET_TMPDIR")).join("sandbox-journal");
    wrapped_in(policy, cwd, prog, args, Some(&shared))
}

/// With `journal`, the run keeps its crash-recovery journal there instead of the shared default,
/// so that runs of OTHER tests (which recover any dead run they find) cannot touch it.
fn wrapped_in(
    policy: &SandboxPolicy,
    cwd: &Path,
    prog: &str,
    args: &[&str],
    journal: Option<&Path>,
) -> Command {
    let mut w = backend()
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
    if let Some(j) = journal {
        let mut spec: serde_json::Value =
            serde_json::from_str(w.args[1].to_str().unwrap()).unwrap();
        spec["journal_dir"] = serde_json::json!(j);
        w.args[1] = spec.to_string().into();
    }
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
    // (`del` does not report failure through its exit status: look at the file)
    let _ = sh(&p, &r, r"del .git\hooks\pre-commit");
    assert!(r.join(".git/hooks/pre-commit").exists(), "{}", dump());
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
fn the_guarded_directory_itself_cannot_be_renamed_or_replaced() {
    let (_w, r, p) = guarded();
    let o = sh(&p, &r, r"ren .git .git2");
    assert!(
        !o.status.success() && r.join(".git/hooks").exists() && !r.join(".git2").exists(),
        "{}\n{}",
        text(&o),
        acl_dump(&p, &r)
    );
    // control: files below it are still writable (git needs its lock files and index)
    let mk = sh(&p, &r, r"echo x> .git\index.lock");
    assert!(
        mk.status.success() && r.join(".git/index.lock").exists(),
        "control (create): {}\n{}",
        text(&mk),
        acl_dump(&p, &r)
    );
    // Deleting files the container created is NOT asserted: on CI `del` of such a file failed with
    // access denied even in the workspace root (known limitation, see the spec).
    // and the read-only child stays read-only
    assert!(!sh(&p, &r, r"echo evil> .git\hooks\h2").status.success());
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
    let local = probe_in(w.path()); // inside the workspace grant: executable there
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap(); // a "service" of the host
    let addr = listener.local_addr().unwrap().to_string();
    let control = Command::new(&local)
        .args(["connect", &addr])
        .output()
        .unwrap();
    assert!(control.status.success(), "control: {}", text(&control));
    let o = run(
        &pol(w.path()),
        w.path(),
        local.to_str().unwrap(),
        &["connect", &addr],
    );
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
    let probe = probe_in(w.path());
    let target = other.path().join("g.txt");
    let spawn = |p: &SandboxPolicy| {
        run(
            p,
            w.path(),
            probe.to_str().unwrap(),
            &["spawn", target.to_str().unwrap()],
        )
    };
    // the child STARTED (not 3) and its write outside the grants failed (1)
    let o = spawn(&pol(w.path()));
    assert_eq!(o.status.code(), Some(1), "{}", text(&o));
    assert!(!target.exists());
    // control: the same child writes there once the directory is granted
    let mut p = pol(w.path());
    p.writable_roots.push(canon(other.path()));
    let ok = spawn(&p);
    assert!(
        ok.status.success() && target.exists(),
        "control: {}",
        text(&ok)
    );
}

#[test]
fn the_process_limit_applies_when_exec_is_not_allowed() {
    let w = ws();
    let probe = probe_in(w.path());
    let made = w.path().join("g.txt");
    let spawn = |p: &SandboxPolicy| {
        let _ = std::fs::remove_file(&made);
        run(
            p,
            w.path(),
            probe.to_str().unwrap(),
            &["spawn", made.to_str().unwrap()],
        )
    };
    // control: a child process is allowed by default
    let c = spawn(&pol(w.path()));
    assert!(c.status.success() && made.exists(), "control: {}", text(&c));
    let mut p = pol(w.path());
    p.allow_exec = false; // Job Object ActiveProcessLimit = 1
    let o = spawn(&p);
    assert_eq!(
        o.status.code(),
        Some(3),
        "the child must not start: {}",
        text(&o)
    );
    assert!(!made.exists());
}

/// `icacls <dir> /T` with the directory's own path taken out, so two runs compare equal.
fn acl_tree(d: &Path) -> String {
    let o = Command::new(r"C:\Windows\System32\icacls.exe")
        .arg(d)
        .arg("/T")
        .output()
        .unwrap();
    String::from_utf8_lossy(&o.stdout).replace(&d.display().to_string(), "<ws>")
}

#[test]
fn acls_are_restored_after_the_run_cuts_included() {
    let (_w, r, p) = guarded();
    let before = acl_tree(&r);
    let o = sh(&p, &r, "echo hi> made.txt");
    assert!(o.status.success(), "{}", text(&o));
    assert!(r.join("made.txt").exists());
    // no per-run container SID (S-1-15-2-...) is left on any path, the file the run created included
    let with_new_file = acl_tree(&r);
    assert!(!with_new_file.contains("S-1-15-2-"), "{with_new_file}");
    // and everything else is exactly as it was
    std::fs::remove_file(r.join("made.txt")).unwrap();
    let after = acl_tree(&r);
    assert_eq!(before, after, "before:\n{before}\nafter:\n{after}");
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

/// Spawn a long-lived container that keeps trying to write into the read-only hooks directory.
fn long_run(p: &SandboxPolicy, r: &Path, secs: &str) -> std::process::Child {
    let probe = probe_in(r);
    let target = r.join(r".git\hooks\planted");
    let mut c = wrapped(
        p,
        r,
        probe.to_str().unwrap(),
        &["tryw", target.to_str().unwrap(), secs],
    );
    c.stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    c.spawn().unwrap()
}

#[test]
fn an_overlapping_run_fails_closed_and_does_not_lift_the_protection_of_the_first() {
    let (_w, r, p) = guarded();
    let before = acl_tree(&r.join(".git"));
    let mut b = long_run(&p, &r, "9");
    std::thread::sleep(std::time::Duration::from_secs(3));
    // a second run that needs the same cuts: refused, before it changes anything
    let a = sh(&p, &r, "echo hi");
    assert_eq!(a.status.code(), Some(126), "{}", text(&a));
    assert!(
        text(&a).contains("another sandboxed command"),
        "{}",
        text(&a)
    );
    // the first run kept trying to write into hooks the whole time and never could
    let code = b.wait().unwrap().code();
    assert_eq!(
        code,
        Some(1),
        "the first run was able to write into the read-only dir"
    );
    assert!(!r.join(r".git\hooks\planted").exists());
    // both runs are gone: everything is as it was
    assert_eq!(before, acl_tree(&r.join(".git")));
    // and the next run works again
    assert!(sh(&p, &r, "echo hi").status.success());
}

#[test]
fn a_launcher_killed_outright_is_recovered_by_the_next_run() {
    let (_w, r, p) = guarded();
    let j = tempfile::tempdir().unwrap(); // not the shared journal: other tests would recover it
    let probe = probe_in(&r);
    let before = acl_tree(&r);
    let mut c = wrapped_in(
        &p,
        &r,
        probe.to_str().unwrap(),
        &["sleep", "30"],
        Some(j.path()),
    );
    c.stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    let mut b = c.spawn().unwrap();
    std::thread::sleep(std::time::Duration::from_secs(3));
    b.kill().unwrap(); // TerminateProcess: no cleanup runs
    let _ = b.wait();
    std::thread::sleep(std::time::Duration::from_secs(1));
    // the honest part: the protections and the dead SID's ACEs are still there, and so is the journal
    assert_ne!(
        before,
        acl_tree(&r),
        "nothing was left behind, the kill did not test anything"
    );
    assert!(std::fs::read_dir(j.path()).unwrap().count() == 1);
    // the next sandboxed run restores them (and deletes the dead run's profile and journal)
    let o = wrapped_in(&p, &r, CMD, &["/c", "echo hi"], Some(j.path()))
        .output()
        .unwrap();
    assert!(o.status.success(), "{}", text(&o));
    assert_eq!(
        before,
        acl_tree(&r),
        "recovery did not restore the original ACLs"
    );
    assert_eq!(
        std::fs::read_dir(j.path()).unwrap().count(),
        0,
        "journal left"
    );
}

/// `mklink` from the host side.
fn mklink(kind: &str, link: &Path, target: &Path) {
    let o = Command::new(CMD)
        .args([
            "/c",
            "mklink",
            kind,
            &link.display().to_string(),
            &target.display().to_string(),
        ])
        .output()
        .unwrap();
    assert!(o.status.success(), "mklink {kind}: {}", text(&o));
}

#[test]
fn a_junction_from_the_workspace_to_an_outside_dir_gives_no_access_and_changes_no_acl() {
    let (w, outside) = (ws(), ws());
    std::fs::write(outside.path().join("key.txt"), "TOPSECRET").unwrap();
    mklink("/J", &w.path().join("jn"), outside.path());
    let before = acl_tree(outside.path());
    let o = sh(&pol(w.path()), w.path(), r"type jn\key.txt");
    assert!(!text(&o).contains("TOPSECRET"), "{}", text(&o));
    let after = acl_tree(outside.path());
    assert!(!after.contains("S-1-15-2-"), "{after}");
    assert_eq!(before, after);
    // control: the workspace's own files are readable
    std::fs::write(w.path().join("own.txt"), "mine").unwrap();
    assert!(text(&sh(&pol(w.path()), w.path(), "type own.txt")).contains("mine"));
}

#[test]
fn a_hard_link_from_the_workspace_to_an_outside_file_is_restored_after_the_run() {
    let (w, outside) = (ws(), ws());
    let f = outside.path().join("key.txt");
    std::fs::write(&f, "TOPSECRET").unwrap();
    mklink("/H", &w.path().join("hl.txt"), &f);
    let before = acl_tree(outside.path());
    // A hard link shares the file's DACL, so the grant on the workspace reaches the file. What the
    // run could read is RECORDED here (docs/specs/sandbox-hardening.md section 5), the ACLs must
    // be back to the original afterwards.
    let o = sh(&pol(w.path()), w.path(), "type hl.txt");
    eprintln!(
        "HARDLINK read during the run: {}",
        text(&o).contains("TOPSECRET")
    );
    let after = acl_tree(outside.path());
    assert!(!after.contains("S-1-15-2-"), "{after}");
    assert_eq!(before, after);
    let c = Command::new(CMD)
        .args(["/c", "type", &f.display().to_string()])
        .output()
        .unwrap();
    assert!(
        text(&c).contains("TOPSECRET"),
        "control: the owner still reads it"
    );
}

fn plain_run(p: &SandboxPolicy, r: &Path, j: &Path) -> Output {
    wrapped_in(p, r, CMD, &["/c", "echo hi"], Some(j))
        .output()
        .unwrap()
}

#[test]
fn an_unreadable_or_invalid_journal_is_a_conflict_for_cuts_and_is_kept() {
    let (_w, r, p) = guarded();
    let j = tempfile::tempdir().unwrap();
    // a policy without cuts is not affected
    let plain = pol(&r);
    for (name, body) in [
        ("garbage.json", "not json".to_string()),
        // well-formed, but names a profile that is not ours and a path outside any drive letter
        (
            "synt.sb.1.evil.json",
            serde_json::json!({"profile": "evil", "sid": "S-1-5-18",
                "entries": [[r"\\server\share\x", "revoke"]]})
            .to_string(),
        ),
    ] {
        let f = j.path().join(name);
        std::fs::write(&f, body).unwrap();
        let o = plain_run(&p, &r, j.path());
        assert_eq!(o.status.code(), Some(126), "{name}: {}", text(&o));
        assert!(
            text(&o).contains("unreadable or invalid sandbox journal"),
            "{}",
            text(&o)
        );
        assert!(f.exists(), "{name} was dropped");
        assert!(
            plain_run(&plain, &r, j.path()).status.success(),
            "{name}: no cuts"
        );
        assert!(f.exists(), "{name} was dropped by a run without cuts");
        std::fs::remove_file(f).unwrap();
    }
    assert!(plain_run(&p, &r, j.path()).status.success());
}

#[test]
fn a_journal_whose_undo_fails_is_kept() {
    let (_w, r, p) = guarded();
    let j = tempfile::tempdir().unwrap();
    // a dead run (no such mutex holder) whose saved descriptor cannot be applied
    let f = j.path().join("synt.sb.1.dead.json");
    std::fs::write(
        &f,
        serde_json::json!({"profile": "synt.sb.1.dead", "sid": "S-1-15-2-1-2-3-4-5-6-7",
            "entries": [[r.join("open").display().to_string(), {"restore": {"sddl": "D:(not valid)"}}]]})
        .to_string(),
    )
    .unwrap();
    let o = plain_run(&p, &r, j.path());
    assert!(
        o.status.success(),
        "the new run itself is fine: {}",
        text(&o)
    );
    assert!(f.exists(), "a journal of a failed recovery was deleted");
}
