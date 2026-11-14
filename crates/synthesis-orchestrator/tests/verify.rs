//! The independent runner's pieces against real git and (where the host confines) the real
//! sandbox: a fresh checkout is only the commit's tree, changes earn flags, a check runs with the
//! source read-only and no network, and a check that cannot run is an error, never a pass.

#[macro_use]
mod common;
use common::*;

use std::fs;
use std::path::Path;
use std::process::Command;
#[cfg(unix)]
use std::sync::atomic::AtomicBool;
use std::time::Duration;

#[cfg(unix)]
use synthesis_core::outcome::ContractCheck;
use synthesis_orchestrator::*;

fn commit_all(repo: &Path, msg: &str) -> String {
    g(repo, &["add", "-A"]);
    g(
        repo,
        &[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "commit",
            "-qm",
            msg,
        ],
    );
    g(repo, &["rev-parse", "HEAD"])
}

#[test]
fn a_fresh_checkout_is_the_commits_tree_and_nothing_else() {
    need_git!();
    let r = rig();
    let repo = &r.repo;
    fs::create_dir_all(repo.join("src")).unwrap();
    fs::write(repo.join("src/a.rs"), "fn a() {}\n").unwrap();
    // an export-ignore rule must not hide a file from the checkout (a worker could hide a failing test)
    fs::write(repo.join(".gitattributes"), "src/hidden.rs export-ignore\n").unwrap();
    fs::write(repo.join("src/hidden.rs"), "// failing test lives here\n").unwrap();
    let commit = commit_all(repo, "work");
    // uncommitted and untracked files of the worktree are not in the commit
    fs::write(
        repo.join("src/a.rs"),
        "fn a() { /* edited after the commit */ }\n",
    )
    .unwrap();
    fs::write(repo.join("untracked.txt"), "x").unwrap();
    let dest = r.base.join("verify-src");
    fresh_checkout(repo, &commit, &dest).unwrap();
    assert_eq!(
        fs::read_to_string(dest.join("src/a.rs")).unwrap(),
        "fn a() {}\n"
    );
    assert!(
        dest.join("src/hidden.rs").is_file(),
        "export-ignore is not honoured"
    );
    assert!(!dest.join("untracked.txt").exists());
    assert!(
        !dest.join(".git").exists(),
        "no repository, no hooks, no config"
    );
    assert!(
        !dest.with_extension("index").exists(),
        "the temporary index is gone"
    );
    // anything that is not a full commit id is refused (no option injection)
    for bad in ["HEAD", "--help", "abc", "", &format!("{commit}x")] {
        assert!(fresh_checkout(repo, bad, &dest).is_err(), "{bad:?}");
    }
    // a second checkout replaces the first
    fs::write(dest.join("stale"), "x").unwrap();
    fresh_checkout(repo, &commit, &dest).unwrap();
    assert!(!dest.join("stale").exists());
}

#[test]
fn changes_to_definitions_and_weakened_tests_earn_flags() {
    need_git!();
    let r = rig();
    let repo = &r.repo;
    fs::create_dir_all(repo.join("tests")).unwrap();
    fs::write(
        repo.join("package.json"),
        "{\"scripts\":{\"test\":\"node --test\"}}\n",
    )
    .unwrap();
    fs::write(
        repo.join("tests/calc.test.js"),
        "assert.equal(add(1,2),3);\nassert.equal(add(2,2),4);\n",
    )
    .unwrap();
    let base = commit_all(repo, "base");
    // plain source change: no flag
    fs::create_dir_all(repo.join("src")).unwrap();
    fs::write(repo.join("src/lib.js"), "x").unwrap();
    let plain = commit_all(repo, "src");
    assert!(change_flags(repo, &base, &plain).unwrap().is_empty());
    assert_eq!(changed_files(repo, &base, &plain).unwrap(), ["src/lib.js"]);
    // a definition file
    fs::write(
        repo.join("package.json"),
        "{\"scripts\":{\"test\":\"true\"}}\n",
    )
    .unwrap();
    let def = commit_all(repo, "defs");
    assert_eq!(
        change_flags(repo, &plain, &def).unwrap(),
        ["definitions_changed"]
    );
    // a weakened test: an assertion removed
    fs::write(
        repo.join("tests/calc.test.js"),
        "assert.equal(add(1,2),3);\n",
    )
    .unwrap();
    let weak = commit_all(repo, "weaken");
    assert!(
        change_flags(repo, &def, &weak)
            .unwrap()
            .contains(&"tests_weakened")
    );
    // the whole range collects both
    let both = change_flags(repo, &base, &weak).unwrap();
    assert_eq!(both, ["definitions_changed", "tests_weakened"]);
}

#[cfg(unix)]
fn check(id: &str, command: &str) -> ContractCheck {
    ContractCheck {
        id: id.into(),
        command: command.into(),
    }
}

/// The real sandbox, where this host has one (macOS Seatbelt, Linux bubblewrap); elsewhere the
/// runner must refuse (and the test says so).
#[cfg(unix)]
#[test]
fn a_check_runs_sandboxed_with_a_read_only_source_and_a_private_tmp() {
    need_git!();
    let r = rig();
    let runner = SandboxRunner::detect(r.home.clone());
    let fp = match runner.fingerprint() {
        Ok(fp) => fp,
        Err(e) => {
            assert!(
                std::env::var_os("SYNT_REQUIRE_BWRAP").is_none_or(|v| v.is_empty()),
                "SYNT_REQUIRE_BWRAP is set but the host cannot confine: {e}"
            );
            println!("SKIP: {e}");
            return;
        }
    };
    assert!(fp.contains("sandbox="), "{fp}");
    fs::write(r.repo.join("a.txt"), "hello\n").unwrap();
    let commit = commit_all(&r.repo, "c");
    let src = r.base.join("run/src");
    let scratch = r.base.join("run");
    fresh_checkout(&r.repo, &commit, &src).unwrap();
    let stop = AtomicBool::new(false);
    let run = |id: &str, cmd: &str, t: u64| {
        runner.run(
            &src,
            &scratch,
            &check(id, cmd),
            Duration::from_secs(t),
            &stop,
        )
    };
    // positive controls: it can read the source and write its private temp dir
    let ok = run(
        "ok",
        "test -f a.txt && echo hi > \"$TMPDIR/x\" && test \"$HOME\" = \"$TMPDIR\"",
        20,
    );
    assert_eq!(
        ok.result(),
        "passed",
        "{:?} {}",
        ok.problem,
        String::from_utf8_lossy(&ok.output)
    );
    assert_eq!(ok.command_hash.len(), 64);
    // the source is read-only: the same shape of command, writing into it, fails
    let ro = run("ro", "echo x > a.txt", 20);
    assert_eq!(ro.result(), "failed");
    assert_eq!(fs::read_to_string(src.join("a.txt")).unwrap(), "hello\n");
    let ro2 = run("ro2", "touch newfile", 20);
    assert_eq!(ro2.result(), "failed");
    assert!(!src.join("newfile").exists());
    // Synthesis's own state is not readable
    fs::write(r.home.join("chain.key"), [7u8; 32]).unwrap();
    let key = run(
        "key",
        &format!(
            "cat '{}' > \"$TMPDIR/k\"",
            r.home.join("chain.key").display()
        ),
        20,
    );
    assert_eq!(key.result(), "failed", "the chain key was readable");
    // no network: a local listener sees nothing
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    l.set_nonblocking(true).unwrap();
    let port = l.local_addr().unwrap().port();
    let net = run(
        "net",
        &format!("curl -s -m 3 http://127.0.0.1:{port}/ || nc -z -w 2 127.0.0.1 {port}"),
        20,
    );
    assert_ne!(net.result(), "passed", "the check reached the network");
    assert!(l.accept().is_err(), "a connection reached the listener");
    // a check that never ends is an error, not a pass, and its process is gone
    let t0 = std::time::Instant::now();
    let slow = run("slow", "sleep 60", 1);
    assert_eq!(slow.result(), "error");
    assert!(slow.problem.unwrap_or_default().contains("timed out"));
    assert!(t0.elapsed() < Duration::from_secs(15));
    // stopping verification stops the check
    let flag = AtomicBool::new(true);
    let stopped = runner.run(
        &src,
        &scratch,
        &check("st", "sleep 60"),
        Duration::from_secs(60),
        &flag,
    );
    assert_eq!(stopped.result(), "error");
    // a command that cannot even start is an error
    let none = run("none", "/no/such/program", 20);
    assert_ne!(none.result(), "passed");
}

#[test]
fn without_a_confining_sandbox_the_runner_refuses() {
    use synthesis_sandbox::NullSandbox;
    let r = SandboxRunner {
        home: std::env::temp_dir(),
        backend: Box::new(NullSandbox),
        state: child_state,
        max_file_mb: 512,
    };
    let e = r.fingerprint().unwrap_err();
    assert!(e.to_string().contains("enforcing sandbox"), "{e}");
    // and a check run result that has no exit code is never `passed`
    let c = CheckRun {
        id: "x".into(),
        command_hash: String::new(),
        exit_code: None,
        problem: Some("timed out".into()),
        output: vec![],
    };
    assert_eq!(c.result(), "error");
    let c2 = CheckRun {
        exit_code: Some(0),
        problem: Some("killed".into()),
        ..c.clone()
    };
    assert_eq!(
        c2.result(),
        "error",
        "an exit code with a problem is not a pass"
    );
    assert_eq!(
        CheckRun {
            exit_code: Some(0),
            problem: None,
            ..c.clone()
        }
        .result(),
        "passed"
    );
    assert_eq!(
        CheckRun {
            exit_code: Some(1),
            problem: None,
            ..c
        }
        .result(),
        "failed"
    );
}

// ---- review fixes: the checkout does not trust the repository's config, refs or attributes ----

#[test]
fn a_filter_in_the_repos_config_does_not_run_at_checkout_and_replace_refs_are_ignored() {
    need_git!();
    let r = rig();
    let repo = &r.repo;
    fs::write(repo.join(".gitattributes"), "*.txt filter=evil\n").unwrap();
    fs::write(repo.join("a.txt"), "real\n").unwrap();
    let commit = commit_all(repo, "work");
    // the filter driver is defined AFTER the commit, in the repository's own config
    let marker = r.base.join("filter-ran");
    g(
        repo,
        &[
            "config",
            "filter.evil.smudge",
            &format!("touch '{}'; cat", marker.display()),
        ],
    );
    // positive control: plain git in the user's repository DOES run it
    let ctl = r.base.join("control/");
    fs::create_dir_all(&ctl).unwrap();
    g(
        repo,
        &[
            "checkout-index",
            "-f",
            "-a",
            &format!("--prefix={}", ctl.display()),
        ],
    );
    assert!(marker.exists(), "the control shows the filter would run");
    fs::remove_file(&marker).unwrap();
    // a replace ref that swaps the blob: plain git shows the replacement
    let blob = g(repo, &["rev-parse", &format!("{commit}:a.txt")]);
    fs::write(r.base.join("fake.txt"), "FAKE\n").unwrap();
    let fake = g(
        repo,
        &[
            "hash-object",
            "-w",
            r.base.join("fake.txt").to_str().unwrap(),
        ],
    );
    g(repo, &["replace", &blob, &fake]);
    let dest = r.base.join("verify-src");
    fresh_checkout(repo, &commit, &dest).unwrap();
    assert!(
        !marker.exists(),
        "the repository's filter ran at verification checkout"
    );
    assert_eq!(
        fs::read_to_string(dest.join("a.txt")).unwrap(),
        "real\n",
        "a replace ref changed the tree"
    );
}

#[test]
fn an_entry_inside_a_git_directory_is_refused() {
    need_git!();
    let r = rig();
    let repo = &r.repo;
    fs::write(repo.join("a.txt"), "x\n").unwrap();
    let blob = g(repo, &["hash-object", "-w", "a.txt"]);
    // `.git` as a tree entry cannot be made with `git add`: build it with plumbing
    let sub = Command::new("git")
        .current_dir(repo)
        .args(["mktree"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    use std::io::Write;
    let mut sub = sub;
    sub.stdin
        .take()
        .unwrap()
        .write_all(format!("100644 blob {blob}\tconfig\n").as_bytes())
        .unwrap();
    let subtree = String::from_utf8(sub.wait_with_output().unwrap().stdout)
        .unwrap()
        .trim()
        .to_string();
    if subtree.is_empty() {
        println!("SKIP: this git cannot build the tree");
        return;
    }
    let root = Command::new("git")
        .current_dir(repo)
        .args(["mktree", "--missing"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let mut root = root;
    root.stdin
        .take()
        .unwrap()
        .write_all(format!("040000 tree {subtree}\t.git\n").as_bytes())
        .unwrap();
    let tree = String::from_utf8(root.wait_with_output().unwrap().stdout)
        .unwrap()
        .trim()
        .to_string();
    if tree.is_empty() {
        println!("SKIP: this git refuses to build a .git entry");
        return;
    }
    let commit = g(
        repo,
        &[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "commit-tree",
            &tree,
            "-m",
            "evil",
        ],
    );
    let dest = r.base.join("evil-src");
    let e = fresh_checkout(repo, &commit, &dest).unwrap_err();
    assert!(e.to_string().contains(".git"), "{e}");
    assert!(!dest.join(".git").exists());
}

// ---- review fixes: the weakening scan reads the diff, not what a file's lines claim ----

#[test]
fn a_forged_diff_header_line_cannot_hide_a_removed_assertion() {
    need_git!();
    let r = rig();
    let repo = &r.repo;
    fs::create_dir_all(repo.join("tests")).unwrap();
    // the second line, once removed, reads in a patch like the header of another file
    fs::write(
        repo.join("tests/calc.test.js"),
        "-- a/src/x.js\nassert.equal(add(1,2),3);\nassert.equal(add(2,2),4);\n",
    )
    .unwrap();
    let base = commit_all(repo, "base");
    fs::write(
        repo.join("tests/calc.test.js"),
        "assert.equal(add(2,2),4);\n",
    )
    .unwrap();
    let weak = commit_all(repo, "weaken");
    // the user's own config must not change how the diff is read either
    g(repo, &["config", "diff.noprefix", "true"]);
    g(
        repo,
        &["config", concat!("diff.mne", "monicPrefix"), "true"],
    );
    assert_eq!(
        change_flags(repo, &base, &weak).unwrap(),
        ["tests_weakened"]
    );
    let edits = file_edits(repo, &base, &weak).unwrap();
    assert_eq!(edits.len(), 1);
    assert_eq!(edits[0].path, "tests/calc.test.js");
    assert!(
        edits[0].removed.iter().any(|l| l.contains("add(1,2)")),
        "{edits:?}"
    );
    // a path with a space is read from the raw listing, not from a header
    fs::write(
        repo.join("tests/my test.test.js"),
        "assert(1);\nassert(2);\n",
    )
    .unwrap();
    let b2 = commit_all(repo, "spaced");
    fs::write(repo.join("tests/my test.test.js"), "assert(2);\n").unwrap();
    let w2 = commit_all(repo, "spaced weak");
    assert_eq!(change_flags(repo, &b2, &w2).unwrap(), ["tests_weakened"]);
}

#[test]
fn removed_inline_tests_in_source_files_are_flagged() {
    need_git!();
    let r = rig();
    let repo = &r.repo;
    fs::create_dir_all(repo.join("src")).unwrap();
    fs::write(
        repo.join("src/lib.rs"),
        "pub fn f() -> i32 { 1 }\n#[cfg(test)]\nmod t {\n    #[test]\n    fn one() { assert_eq!(super::f(), 1); }\n}\n",
    )
    .unwrap();
    let base = commit_all(repo, "base");
    // the test and its assertion are deleted from a non-test path
    fs::write(repo.join("src/lib.rs"), "pub fn f() -> i32 { 2 }\n").unwrap();
    let c = commit_all(repo, "gone");
    assert_eq!(change_flags(repo, &base, &c).unwrap(), ["tests_weakened"]);
    // an unrelated source edit is not flagged
    fs::write(repo.join("src/lib.rs"), "pub fn f() -> i32 { 3 }\n").unwrap();
    let c2 = commit_all(repo, "plain");
    assert!(change_flags(repo, &c, &c2).unwrap().is_empty());
}

#[test]
fn build_scripts_toolchain_pins_and_file_modes_are_flagged() {
    need_git!();
    let r = rig();
    let repo = &r.repo;
    fs::write(repo.join("run.sh"), "echo\n").unwrap();
    let base = commit_all(repo, "base");
    for f in ["build.rs", "conftest.py", "rust-toolchain.toml"] {
        fs::write(repo.join(f), "x\n").unwrap();
        let c = commit_all(repo, f);
        assert!(
            change_flags(repo, &base, &c)
                .unwrap()
                .contains(&"definitions_changed"),
            "{f}"
        );
    }
    fs::create_dir_all(repo.join(".cargo")).unwrap();
    fs::write(repo.join(".cargo/config.toml"), "x\n").unwrap();
    let c = commit_all(repo, "cargo");
    assert!(
        change_flags(repo, &base, &c)
            .unwrap()
            .contains(&"definitions_changed")
    );
    // an exec-bit flip is a mode change
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let before = g(repo, &["rev-parse", "HEAD"]);
        fs::set_permissions(repo.join("run.sh"), fs::Permissions::from_mode(0o755)).unwrap();
        g(repo, &["update-index", "--chmod=+x", "run.sh"]);
        let m = commit_all(repo, "chmod");
        assert_eq!(change_flags(repo, &before, &m).unwrap(), ["mode_changed"]);
    }
}

// ---- review fixes: process handling ----

#[cfg(unix)]
fn always_gone(_: u32) -> ChildState {
    ChildState::Gone
}

#[cfg(unix)]
fn pid_from(file: &Path) -> Option<u32> {
    for _ in 0..200 {
        if let Some(p) = fs::read_to_string(file)
            .ok()
            .and_then(|t| t.trim().parse().ok())
        {
            return Some(p);
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    None
}

#[cfg(unix)]
fn alive(pid: u32) -> bool {
    Command::new("kill")
        .args(["-0", &pid.to_string()])
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

#[cfg(unix)]
#[test]
fn a_check_reaped_behind_our_back_is_never_signalled_and_a_normal_exit_kills_its_group() {
    need_git!();
    let r = rig();
    let probe = SandboxRunner::detect(r.home.clone());
    if probe.fingerprint().is_err() {
        println!("SKIP: no confining sandbox");
        return;
    }
    fs::write(r.repo.join("a.txt"), "x\n").unwrap();
    let commit = commit_all(&r.repo, "c");
    let scratch = r.base.join("proc");
    let src = scratch.join("src");
    fresh_checkout(&r.repo, &commit, &src).unwrap();
    let stop = AtomicBool::new(false);

    // the group of a check that exits is killed while the leader is still ours: a background
    // child it leaves behind does not survive the check
    let real = SandboxRunner::detect(r.home.clone());
    let bg = real.run(
        &src,
        &scratch,
        &check("bg", "sleep 60 & echo $! > \"$TMPDIR/bg.pid\"; exit 0"),
        Duration::from_secs(20),
        &stop,
    );
    assert_eq!(bg.result(), "passed", "{:?}", bg.problem);
    let bgpid = pid_from(&scratch.join("tmp-bg/bg.pid")).expect("pid file");
    let t0 = std::time::Instant::now();
    while alive(bgpid) && t0.elapsed() < Duration::from_secs(5) {
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(
        !alive(bgpid),
        "the background child of an exited check survived"
    );

    // injected state: the kernel "reaped" the child, so nothing may be sent to that pid, even on a timeout
    let gone = SandboxRunner {
        state: always_gone,
        ..SandboxRunner::detect(r.home.clone())
    };
    let run = gone.run(
        &src,
        &scratch,
        &check("gone", "echo $$ > \"$TMPDIR/gone.pid\"; sleep 60"),
        Duration::from_millis(100),
        &stop,
    );
    assert_eq!(run.result(), "error");
    let pid = pid_from(&scratch.join("tmp-gone/gone.pid")).expect("pid file");
    assert!(alive(pid), "a pid that was reported gone was signalled");
    let _ = Command::new("kill").args(["-9", &pid.to_string()]).status();
}

// ---- review fixes: only an approved contract is verified ----

#[test]
fn a_verifier_is_only_made_for_the_approved_current_contract() {
    need_git!();
    use std::sync::Arc;
    use synthesis_core::WorkspaceContext;
    use synthesis_core::outcome::OutcomeState;
    use synthesis_session::outcome::{Effects, NewOutcome};
    let r = rig();
    let runner: Arc<dyn CheckRunner> = Arc::new(SandboxRunner::detect(r.home.clone()));
    // approved: fine
    let (w, _) = r.fresh();
    assert!(
        IndependentVerifier::for_outcome(
            &w,
            r.repo.clone(),
            r.home.clone(),
            runner.clone(),
            Duration::from_secs(5)
        )
        .is_ok()
    );
    // a new contract version is not approved until a person approves it
    let ws = WorkspaceContext::open("w", &r.repo, &r.repo, &[], r.base.join("cart"), "pg").unwrap();
    let mut w = r
        .store
        .create_outcome(NewOutcome {
            workspace: &ws,
            base_commit: r.head.clone(),
            contract: &contract(),
            policy: &policy(),
        })
        .unwrap();
    let rev = w.get_outcome(w.id()).unwrap().revision;
    w.transition_outcome(
        rev,
        OutcomeState::ContractPending,
        None,
        &Effects::default(),
    )
    .unwrap();
    let err = IndependentVerifier::for_outcome(
        &w,
        r.repo.clone(),
        r.home.clone(),
        runner,
        Duration::from_secs(5),
    )
    .err()
    .expect("unapproved");
    assert!(err.to_string().contains("not approved"), "{err}");
}

#[test]
fn a_nul_byte_cannot_turn_a_weakening_diff_into_a_binary_file() {
    need_git!();
    let r = rig();
    let repo = &r.repo;
    fs::create_dir_all(repo.join("tests")).unwrap();
    fs::write(repo.join("tests/calc.test.js"), "assert(1);\nassert(2);\n").unwrap();
    let base = commit_all(repo, "base");
    // an assertion removed AND a NUL appended: plain `git diff` says "Binary files differ"
    fs::write(repo.join("tests/calc.test.js"), b"assert(2);\n\0").unwrap();
    let weak = commit_all(repo, "nul");
    assert_eq!(
        change_flags(repo, &base, &weak).unwrap(),
        ["tests_weakened"]
    );
}

#[cfg(unix)]
#[test]
fn a_path_that_is_not_utf8_is_an_error_not_a_skipped_scan() {
    need_git!();
    use std::os::unix::ffi::OsStrExt;
    let r = rig();
    let repo = &r.repo;
    let base = g(repo, &["rev-parse", "HEAD"]);
    // macOS refuses a name that is not UTF-8; Linux CI exercises this
    let odd = repo.join(std::ffi::OsStr::from_bytes(b"bad\xff.txt"));
    if fs::write(odd, "x\n").is_err() {
        println!("SKIP: this file system refuses non-UTF-8 names");
        return;
    }
    let c = commit_all(repo, "odd name");
    let e = change_flags(repo, &base, &c).unwrap_err();
    assert!(e.to_string().contains("UTF-8"), "{e}");
}

#[test]
fn the_scratch_repository_has_no_template_files() {
    need_git!();
    let r = rig();
    let stop = AtomicBool::new(false);
    let dir = r.base.join("scratch-check");
    let s = Scratch::new(&r.repo, dir.clone(), &stop).unwrap();
    assert!(!dir.join("info/attributes").exists());
    assert!(
        !dir.join("hooks").exists(),
        "an empty template: no hook samples"
    );
    assert!(dir.join("objects/info/alternates").is_file());
    drop(s);
    assert!(!dir.exists(), "removed when dropped");
}
