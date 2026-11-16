//! `commit_diff`: the read-only diff of a candidate commit (ADR-021 notes, threat model T-37).
//! The repository is hostile; the output is bounded, printable and withholds secret-shaped files.

mod common;
use common::*;

use std::fs;
use std::path::Path;
use std::process::Command;
use std::sync::atomic::AtomicBool;

use synthesis_orchestrator::*;

fn commit_all(repo: &Path, msg: &str) -> String {
    g(repo, &["add", "-A"]);
    g(repo, &["commit", "-qm", msg]);
    g(repo, &["rev-parse", "HEAD"])
}

fn diff(r: &Rig, base: &str, commit: &str) -> DiffReport {
    commit_diff(
        &r.repo,
        r.base.join("scratch"),
        base,
        commit,
        &AtomicBool::new(false),
    )
    .unwrap()
}

#[test]
fn the_diff_has_counts_a_patch_and_withholds_secrets_and_binaries() {
    need_git!();
    let r = rig();
    let repo = &r.repo;
    fs::write(repo.join(".env"), "TOKEN=base-secret\n").unwrap();
    fs::write(repo.join("logo.bin"), b"\0old").unwrap();
    let base = commit_all(repo, "base");
    fs::write(repo.join("a.txt"), "two\u{202e}\u{e0041}\u{2028}\u{600}\u{6dd}\u{70f}\u{34f}\u{115f}\u{3164}\u{ffa0}\u{fe0f}\u{e0100}x\n").unwrap();
    fs::write(repo.join(".env"), "TOKEN=hunter2-secret\n").unwrap();
    fs::write(repo.join("logo.bin"), b"\0new").unwrap();
    #[cfg(unix)]
    fs::write(repo.join("bad\u{1b}[31mname.txt"), "x\n").unwrap();
    let commit = commit_all(repo, "work");
    let d = diff(&r, &base, &commit);
    assert_eq!(d.total_files, if cfg!(unix) { 4 } else { 3 });
    let f = |p: &str| d.files.iter().find(|f| f.path.contains(p)).unwrap();
    let a = f("a.txt");
    assert_eq!((a.added, a.removed, a.binary), (Some(1), Some(1), false));
    let patch = a.patch.as_ref().unwrap();
    assert!(patch.contains("-one") && patch.contains("+two"), "{patch}");
    for c in "\u{202e}\u{e0041}\u{2028}\u{600}\u{6dd}\u{70f}\u{34f}\u{115f}\u{3164}\u{ffa0}\u{fe0f}\u{e0100}".chars() {
        assert!(!patch.contains(c), "{:x} reached the patch", c as u32);
    }
    let bin = f("logo.bin");
    assert!(bin.binary && bin.patch.is_none() && bin.added.is_none());
    let env = f(".env");
    assert!(env.secret && env.patch.is_none() && env.added.is_none() && env.removed.is_none());
    let all = format!("{d:?}");
    assert!(
        !all.contains("hunter2") && !all.contains("base-secret"),
        "{all}"
    );
    assert!(d.files.iter().all(|f| !f.path.contains('\u{1b}')));
    #[cfg(unix)]
    assert!(d.files.iter().any(|f| f.path.contains("name.txt")));
}

#[test]
fn the_caps_hold_for_files_and_patch_bytes() {
    need_git!();
    let r = rig();
    let repo = &r.repo;
    let base = g(repo, &["rev-parse", "HEAD"]);
    for i in 0..(DIFF_MAX_FILES + 5) {
        fs::write(repo.join(format!("f{i:04}.txt")), "x\n").unwrap();
    }
    fs::write(repo.join("big.txt"), "y".repeat(DIFF_MAX_FILE_PATCH * 4)).unwrap();
    let commit = commit_all(repo, "many");
    let d = diff(&r, &base, &commit);
    assert_eq!(d.total_files, DIFF_MAX_FILES + 6);
    assert_eq!(d.files.len(), DIFF_MAX_FILES);
    let total: usize = d
        .files
        .iter()
        .filter_map(|f| f.patch.as_ref())
        .map(String::len)
        .sum();
    assert!(total <= DIFF_MAX_PATCH, "{total}");
    // big.txt sorts first: its patch is cut at the per-file cap
    let big = d.files.iter().find(|f| f.path == "big.txt").unwrap();
    assert!(big.patch_truncated && big.patch.as_ref().unwrap().len() <= DIFF_MAX_FILE_PATCH);
}

#[cfg(unix)]
#[test]
fn a_hostile_repository_runs_nothing_when_its_commit_is_diffed() {
    need_git!();
    let r = rig();
    let repo = &r.repo;
    let marker = r.base.join("ran");
    let touch = format!("touch '{}'; cat", marker.display());
    fs::write(
        repo.join(".gitattributes"),
        "*.txt diff=evil\n*.rs filter=evil\n",
    )
    .unwrap();
    fs::write(repo.join("a.txt"), "one\n").unwrap();
    let base = commit_all(repo, "base");
    fs::write(repo.join("a.txt"), "two\n").unwrap();
    fs::write(repo.join("b.rs"), "fn main() {}\n").unwrap();
    let commit = commit_all(repo, "work");
    // everything a repository can say, set AFTER the commit, in its own config, hooks and info
    g(repo, &["config", "diff.evil.textconv", &touch]);
    g(
        repo,
        &[
            "config",
            "diff.evil.command",
            &format!("touch '{}'", marker.display()),
        ],
    );
    g(
        repo,
        &[
            "config",
            "diff.external",
            &format!("touch '{}'", marker.display()),
        ],
    );
    g(
        repo,
        &[
            "config",
            "core.fsmonitor",
            &format!("touch '{}'", marker.display()),
        ],
    );
    g(
        repo,
        &[
            "config",
            "core.pager",
            &format!("touch '{}'; cat", marker.display()),
        ],
    );
    g(repo, &["config", "filter.evil.clean", &touch]);
    fs::write(repo.join(".git/info/attributes"), "*.txt diff=evil\n").unwrap();
    let hook = repo.join(".git/hooks/post-index-change");
    fs::write(&hook, format!("#!/bin/sh\ntouch '{}'\n", marker.display())).unwrap();
    #[cfg(unix)]
    std::os::unix::fs::PermissionsExt::set_mode(
        &mut fs::metadata(&hook).unwrap().permissions(),
        0o755,
    );
    // positive control: plain git in the user's repository does run the textconv
    let _ = Command::new("git")
        .current_dir(repo)
        .args(["diff", "--textconv", &base, &commit])
        .output();
    assert!(marker.exists(), "the control shows the config would run");
    fs::remove_file(&marker).unwrap();
    let d = diff(&r, &base, &commit);
    assert!(
        !marker.exists(),
        "the repository's config ran during commit_diff"
    );
    assert_eq!(d.total_files, 2);
    assert!(
        d.files
            .iter()
            .any(|f| f.patch.as_deref().is_some_and(|p| p.contains("+two")))
    );
}
