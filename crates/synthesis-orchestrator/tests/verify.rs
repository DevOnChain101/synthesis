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
#[cfg(unix)]
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
