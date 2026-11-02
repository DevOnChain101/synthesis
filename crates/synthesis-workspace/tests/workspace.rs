use std::fs;
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use synthesis_config::Env;
use synthesis_core::{Budget, PermissionMode, WorkspaceContext};
use synthesis_session::{NewSession, Store, StoreOptions};
use synthesis_workspace::*;

macro_rules! need_git {
    () => {
        if Command::new("git").arg("--version").output().is_err() {
            println!("SKIP: git not available");
            return;
        }
    };
}

fn env() -> Env {
    Env::from_process()
}

/// Plain git for fixture setup: clean env, no hostile config applied yet.
fn g(dir: &Path, args: &[&str]) -> String {
    let o = Command::new("git")
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap())
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .current_dir(dir)
        .args([
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "-c",
            "init.defaultBranch=main",
        ])
        .args(args)
        .output()
        .unwrap();
    assert!(
        o.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&o.stderr)
    );
    String::from_utf8_lossy(&o.stdout).trim().to_string()
}

struct Fx {
    _tmp: tempfile::TempDir,
    base: PathBuf,
    repo: PathBuf,
    mgr: WorktreeManager,
}

fn fx_named(repo_name: &str) -> Fx {
    let tmp = tempfile::tempdir().unwrap();
    let base = synthesis_core::canonicalize(tmp.path()).unwrap();
    let repo = base.join(repo_name);
    fs::create_dir_all(&repo).unwrap();
    g(&repo, &["init", "-q"]);
    fs::write(repo.join("a.txt"), "one\n").unwrap();
    fs::write(repo.join("b.txt"), "bee\n").unwrap();
    g(&repo, &["add", "."]);
    g(&repo, &["commit", "-qm", "init"]);
    let mgr = WorktreeManager::new(&env(), WorktreeOptions::new(base.join("state"))).unwrap();
    Fx {
        _tmp: tmp,
        base,
        repo,
        mgr,
    }
}

fn fx() -> Fx {
    fx_named("repo")
}

fn alloc(f: &Fx, id: &str, inc: bool) -> (Allocation, GitInfo) {
    let a = f
        .mgr
        .create(&f.repo, id, f.base.join("art").join(id), inc)
        .unwrap();
    let info = a.git.clone().unwrap();
    (a, info)
}

#[test]
fn create_diff_cleanup_leaves_user_tree_untouched() {
    need_git!();
    let f = fx();
    // User's dirty tree: modified tracked + untracked file.
    fs::write(f.repo.join("a.txt"), "user edit\n").unwrap();
    fs::write(f.repo.join("scratch.txt"), "mine\n").unwrap();
    let status_before = g(&f.repo, &["status", "--porcelain"]);

    let (a, info) = alloc(&f, "s1", false);
    assert!(!a.not_a_git_repo);
    let wt = &info.worktree;
    assert_eq!(a.context.root, *wt);
    assert_eq!(a.context.cwd, *wt);
    assert_eq!(a.context.git_worktree.as_ref(), Some(wt));
    assert_eq!(*wt, synthesis_core::canonicalize(wt).unwrap());
    assert_eq!(
        wt,
        &synthesis_core::canonicalize(f.base.join(".synthesis-worktrees/s1")).unwrap()
    );
    assert_eq!(info.branch, "synthesis/s1");
    // Worktree starts from HEAD: no user edits visible there.
    assert_eq!(fs::read_to_string(wt.join("a.txt")).unwrap(), "one\n");
    assert!(!wt.join("scratch.txt").exists());

    fs::write(wt.join("a.txt"), "agent edit\n").unwrap();
    fs::write(wt.join("new.txt"), "fresh\n").unwrap();
    let ch = f.mgr.changes(&info).unwrap();
    let mut agent: Vec<_> = ch
        .agent
        .iter()
        .map(|c| (c.path.as_str(), c.status))
        .collect();
    agent.sort();
    assert_eq!(agent, [("a.txt", 'M'), ("new.txt", '?')]);
    assert!(
        ch.outside.is_empty(),
        "pre-existing user dirt is not 'unexpected': {:?}",
        ch.outside
    );
    let d = f.mgr.diff(&info).unwrap();
    assert!(d.contains("+agent edit") && d.contains("+fresh"), "{d}");

    // User keeps editing the original tree: reported separately, with overlap.
    fs::write(f.repo.join("a.txt"), "user edit 2\n").unwrap();
    fs::write(f.repo.join("c.txt"), "c\n").unwrap();
    let ch = f.mgr.changes(&info).unwrap();
    let mut out: Vec<_> = ch.outside.iter().map(|c| c.path.as_str()).collect();
    out.sort();
    assert_eq!(out, ["a.txt", "c.txt"]);
    assert_eq!(ch.overlap, ["a.txt"]);
    assert!(!ch.agent.iter().any(|c| c.path == "c.txt"));

    // Cleanup refuses unmerged / uncommitted work, then succeeds when forced.
    assert!(matches!(
        f.mgr.cleanup(&info, false),
        Err(Error::Refused(_))
    ));
    assert!(wt.exists());
    f.mgr.commit_all(&info, "agent work").unwrap().unwrap();
    assert!(
        matches!(f.mgr.cleanup(&info, false), Err(Error::Refused(m)) if m.contains("unmerged"))
    );
    f.mgr.cleanup(&info, true).unwrap();
    assert!(!wt.exists());
    assert!(g(&f.repo, &["branch", "--list", "synthesis/s1"]).is_empty());

    // The user's tree: exactly what they wrote.
    assert_eq!(
        fs::read_to_string(f.repo.join("a.txt")).unwrap(),
        "user edit 2\n"
    );
    assert_eq!(
        fs::read_to_string(f.repo.join("scratch.txt")).unwrap(),
        "mine\n"
    );
    assert_ne!(status_before, "");
    assert_eq!(g(&f.repo, &["rev-parse", "HEAD"]), info.main_head);
}

#[test]
fn source_subdirectory_maps_to_the_same_subdirectory_of_the_worktree() {
    need_git!();
    let f = fx();
    fs::create_dir_all(f.repo.join("pkg")).unwrap();
    fs::write(f.repo.join("pkg/b.txt"), "b\n").unwrap();
    g(&f.repo, &["add", "."]);
    g(&f.repo, &["commit", "-q", "-m", "pkg"]);
    let a = f
        .mgr
        .create(&f.repo.join("pkg"), "sub", f.base.join("art/sub"), false)
        .unwrap();
    let wt = a.git.unwrap().worktree;
    assert_eq!(a.context.root, wt);
    assert_eq!(a.context.cwd, wt.join("pkg"));
}

#[test]
fn clean_cleanup_without_force() {
    need_git!();
    let f = fx();
    let (_, info) = alloc(&f, "s1", false);
    f.mgr.cleanup(&info, false).unwrap();
    assert!(!info.worktree.exists());
}

#[test]
fn cleanup_without_force_keeps_a_worktree_that_holds_gitignored_files() {
    need_git!();
    let f = fx();
    std::fs::write(f.repo.join(".gitignore"), "*.secret\ntarget/\n").unwrap();
    g(&f.repo, &["add", ".gitignore"]);
    g(&f.repo, &["commit", "-qm", "ignore"]);
    let (_, info) = alloc(&f, "s-ignored", false);
    std::fs::write(info.worktree.join("a.secret"), "token").unwrap();
    assert!(
        matches!(f.mgr.cleanup(&info, false), Err(Error::Refused(m)) if m.contains("gitignored")),
        "ignored content must stop a non-forced removal"
    );
    assert_eq!(
        std::fs::read_to_string(info.worktree.join("a.secret")).unwrap(),
        "token"
    );
    // build output in an ignored directory too
    std::fs::remove_file(info.worktree.join("a.secret")).unwrap();
    std::fs::create_dir_all(info.worktree.join("target")).unwrap();
    std::fs::write(info.worktree.join("target/out"), "x").unwrap();
    assert!(f.mgr.cleanup(&info, false).is_err());
    assert!(info.worktree.join("target/out").exists());
    // nothing ignored left: removable again
    std::fs::remove_dir_all(info.worktree.join("target")).unwrap();
    f.mgr.cleanup(&info, false).unwrap();
}

#[test]
fn include_uncommitted_snapshot_is_baseline_not_agent_work() {
    need_git!();
    let f = fx();
    fs::write(f.repo.join("a.txt"), "user edit\n").unwrap();
    fs::write(f.repo.join("scratch.txt"), "mine\n").unwrap();
    let (_, info) = alloc(&f, "s1", true);
    let wt = &info.worktree;
    assert_eq!(fs::read_to_string(wt.join("a.txt")).unwrap(), "user edit\n");
    assert_eq!(
        fs::read_to_string(wt.join("scratch.txt")).unwrap(),
        "mine\n"
    );
    assert_ne!(info.baseline, info.main_head);
    assert!(f.mgr.changes(&info).unwrap().agent.is_empty());
    // Original tree untouched.
    assert_eq!(
        fs::read_to_string(f.repo.join("a.txt")).unwrap(),
        "user edit\n"
    );
    assert!(g(&f.repo, &["status", "--porcelain"]).contains("scratch.txt"));
    fs::write(wt.join("b.txt"), "agent\n").unwrap();
    assert_eq!(f.mgr.changes(&info).unwrap().agent.len(), 1);
    // Snapshot baseline cannot be merged wholesale.
    f.mgr.commit_all(&info, "w").unwrap();
    let plan = f.mgr.plan_merge(&info).unwrap();
    assert!(matches!(
        f.mgr.execute_merge(&info, &plan),
        Err(Error::Refused(_))
    ));
    // Clean worktree (nothing new) is removable without force; unmerged commit is not.
    assert!(f.mgr.cleanup(&info, false).is_err());
}

// sh scripts + exec bits: unix only (Windows unverified).
#[cfg(unix)]
#[test]
fn hostile_repo_runs_nothing() {
    need_git!();
    let f = fx();
    let r = &f.repo;
    let canary = f.base.join("canary");
    fs::create_dir_all(&canary).unwrap();
    let c = |n: &str| canary.join(n).to_string_lossy().into_owned();
    // Committed attributes route *.txt through evil drivers.
    fs::write(
        r.join(".gitattributes"),
        "*.txt diff=evil filter=evil merge=evil\n",
    )
    .unwrap();
    g(r, &["add", "."]);
    g(r, &["commit", "-qm", "attrs"]);
    for hook in [
        "post-checkout",
        "pre-commit",
        "post-commit",
        "post-merge",
        "reference-transaction",
        "pre-merge-commit",
        "commit-msg",
        "fsmonitor-watchman",
        "pre-auto-gc",
    ] {
        let p = r.join(".git/hooks").join(hook);
        fs::write(
            &p,
            format!("#!/bin/sh\ntouch '{}'\n", c(&format!("hook-{hook}"))),
        )
        .unwrap();
        fs::set_permissions(&p, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    }
    let cfg = |k: &str, v: String| g(r, &["config", k, &v]);
    cfg("core.fsmonitor", format!("touch '{}' #", c("fsmonitor")));
    cfg("core.sshCommand", format!("touch '{}' #", c("ssh")));
    cfg(
        "diff.external",
        format!("sh -c 'touch {}' #", c("diff-external")),
    );
    cfg("core.pager", format!("touch '{}' #", c("pager")));
    cfg("core.editor", format!("touch '{}' #", c("editor")));
    cfg(
        "diff.evil.textconv",
        format!("touch '{}'; cat", c("textconv")),
    );
    cfg(
        "diff.evil.command",
        format!("touch '{}' #", c("diff-driver")),
    );
    cfg("filter.evil.clean", format!("touch '{}'; cat", c("clean")));
    cfg(
        "filter.evil.smudge",
        format!("touch '{}'; cat", c("smudge")),
    );
    cfg("filter.evil.process", format!("touch '{}' #", c("process")));
    cfg(
        "merge.evil.driver",
        format!("touch '{}'; exit 1 #", c("merge-driver")),
    );
    cfg("credential.helper", format!("!touch '{}' #", c("cred")));

    // Control: the same repo under un-hardened git DOES fire at least one trap.
    fs::write(r.join("a.txt"), "dirty\n").unwrap();
    let _ = Command::new("git")
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap())
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .current_dir(r)
        .args(["diff", "HEAD"])
        .output();
    let fired = fs::read_dir(&canary).unwrap().count();
    assert!(
        fired > 0,
        "control: hostile fixture did not fire under plain git"
    );
    for e in fs::read_dir(&canary).unwrap() {
        fs::remove_file(e.unwrap().path()).unwrap();
    }
    fs::write(r.join("a.txt"), "one\n").unwrap();

    // Hardened: every code path of the manager.
    for inc in [false, true] {
        let id = format!("h{inc}");
        fs::write(r.join("b.txt"), "user dirt\n").unwrap();
        let (_, info) = alloc(&f, &id, inc);
        fs::write(info.worktree.join("a.txt"), "agent\n").unwrap();
        fs::write(info.worktree.join("n.txt"), "new\n").unwrap();
        f.mgr.changes(&info).unwrap();
        f.mgr.diff(&info).unwrap();
        f.mgr.commit_all(&info, "x").unwrap();
        let plan = f.mgr.plan_merge(&info).unwrap();
        assert!(plan.overlap.is_empty());
        f.mgr.cleanup(&info, true).unwrap();
        fs::write(r.join("b.txt"), "bee\n").unwrap();
    }
    // Merge path too (merge driver, hooks).
    let (_, info) = alloc(&f, "hm", false);
    fs::write(info.worktree.join("a.txt"), "agent\n").unwrap();
    f.mgr.commit_all(&info, "x").unwrap();
    let plan = f.mgr.plan_merge(&info).unwrap();
    if plan.clean {
        f.mgr.execute_merge(&info, &plan).unwrap();
    }
    let left: Vec<_> = fs::read_dir(&canary)
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    assert!(left.is_empty(), "canary fired: {left:?}");
}

#[test]
fn concurrent_sessions_get_distinct_worktrees_and_ports() {
    need_git!();
    let f = fx();
    let rm = ResourceManager::new(49152..65000);
    let results: Vec<_> = std::thread::scope(|s| {
        let hs: Vec<_> = (0..4)
            .map(|i| {
                let (f, rm) = (&f, &rm);
                s.spawn(move || {
                    let id = format!("c{i}");
                    let a = f
                        .mgr
                        .create(&f.repo, &id, f.base.join("art").join(&id), false)
                        .unwrap();
                    rm.open_session(&id, f.base.join("art").join(&id));
                    let p = (rm.reserve_port(&id).unwrap(), rm.reserve_port(&id).unwrap());
                    (a, p)
                })
            })
            .collect();
        hs.into_iter().map(|h| h.join().unwrap()).collect()
    });
    let mut roots: Vec<_> = results
        .iter()
        .map(|(a, _)| a.context.root.clone())
        .collect();
    roots.sort();
    roots.dedup();
    assert_eq!(roots.len(), 4);
    let mut ports: Vec<u16> = results
        .iter()
        .flat_map(|(_, p)| [p.0.port, p.1.port])
        .collect();
    ports.sort();
    ports.dedup();
    assert_eq!(ports.len(), 8);
}

/// Adding and removing worktrees of one repository from many threads at once: git alone races on
/// `.git/worktrees/<id>` ("failed to read .../commondir"); the repository lock serialises them.
#[test]
fn many_threads_adding_and_removing_worktrees_do_not_race() {
    need_git!();
    let f = fx();
    std::thread::scope(|s| {
        let hs: Vec<_> = (0..8)
            .map(|i| {
                let f = &f;
                s.spawn(move || {
                    for round in 0..3 {
                        let id = format!("r{i}-{round}");
                        let a = f
                            .mgr
                            .create(&f.repo, &id, f.base.join("art").join(&id), false)
                            .unwrap_or_else(|e| panic!("{id}: {e}"));
                        let info = a.git.expect("a git workspace");
                        f.mgr
                            .cleanup(&info, false)
                            .unwrap_or_else(|e| panic!("{id}: {e}"));
                    }
                })
            })
            .collect();
        for h in hs {
            h.join().unwrap();
        }
    });
}

#[test]
fn paths_with_spaces_and_unicode() {
    need_git!();
    let f = fx_named("my répo ünï 日本");
    let (a, info) = alloc(&f, "s_1.x", false);
    assert!(info.repo_root.to_string_lossy().contains("répo"));
    fs::write(a.context.root.join("ünï file.txt"), "x\n").unwrap();
    let ch = f.mgr.changes(&info).unwrap();
    assert_eq!(ch.agent[0].path, "ünï file.txt");
    assert!(f.mgr.diff(&info).unwrap().contains("ünï file.txt"));
    f.mgr.cleanup(&info, true).unwrap();
}

#[test]
fn rejects_bad_session_ids() {
    need_git!();
    let f = fx();
    for bad in ["", "-x", "../x", "a/b", "a b", ".hid", "x.lock", "--help"] {
        assert!(
            f.mgr
                .create(&f.repo, bad, f.base.join("art"), false)
                .is_err(),
            "{bad:?}"
        );
    }
}

#[test]
fn merge_plan_clean_and_conflict() {
    need_git!();
    let f = fx();
    // Clean merge.
    let (_, i1) = alloc(&f, "m1", false);
    fs::write(i1.worktree.join("b.txt"), "agent bee\n").unwrap();
    f.mgr.commit_all(&i1, "m1").unwrap();
    let p = f.mgr.plan_merge(&i1).unwrap();
    assert!(
        p.clean && p.conflicts.is_empty() && p.commits.len() == 1,
        "{p:?}"
    );
    // Uncommitted worktree changes block a clean plan.
    fs::write(i1.worktree.join("b.txt"), "more\n").unwrap();
    assert!(f.mgr.plan_merge(&i1).unwrap().uncommitted_in_worktree);
    fs::write(i1.worktree.join("b.txt"), "agent bee\n").unwrap();
    let p = f.mgr.plan_merge(&i1).unwrap();
    // User dirt on an unrelated file does not block; git merge keeps it.
    fs::write(f.repo.join("a.txt"), "user dirt\n").unwrap();
    let p = f.mgr.plan_merge(&i1).unwrap().clean.then_some(p).unwrap();
    let new_head = f.mgr.execute_merge(&i1, &p).unwrap();
    assert_eq!(g(&f.repo, &["rev-parse", "HEAD"]), new_head);
    assert_eq!(
        fs::read_to_string(f.repo.join("a.txt")).unwrap(),
        "user dirt\n"
    );
    assert_eq!(
        fs::read_to_string(f.repo.join("b.txt")).unwrap(),
        "agent bee\n"
    );
    assert!(
        matches!(f.mgr.execute_merge(&i1, &p), Err(Error::Refused(_))),
        "stale plan"
    );
    g(&f.repo, &["checkout", "-q", "a.txt"]);
    f.mgr.cleanup(&i1, false).unwrap(); // merged -> removable without force

    // Conflict: both sides commit different content to a.txt.
    let (_, i2) = alloc(&f, "m2", false);
    fs::write(i2.worktree.join("a.txt"), "agent side\n").unwrap();
    f.mgr.commit_all(&i2, "agent").unwrap();
    fs::write(f.repo.join("a.txt"), "user side\n").unwrap();
    g(&f.repo, &["commit", "-qam", "user"]);
    let head = g(&f.repo, &["rev-parse", "HEAD"]);
    let p = f.mgr.plan_merge(&i2).unwrap();
    assert_eq!(p.conflicts, ["a.txt"]);
    assert!(!p.clean);
    assert!(matches!(
        f.mgr.execute_merge(&i2, &p),
        Err(Error::Refused(_))
    ));
    assert_eq!(g(&f.repo, &["rev-parse", "HEAD"]), head);
    assert_eq!(f.mgr.changes(&i2).unwrap().main_head_moved.unwrap().1, head);
    assert!(g(&f.repo, &["status", "--porcelain"]).is_empty());
}

#[test]
fn non_git_directory_is_plain_workspace() {
    need_git!();
    let tmp = tempfile::tempdir().unwrap();
    let dir = synthesis_core::canonicalize(tmp.path())
        .unwrap()
        .join("plain dir");
    fs::create_dir_all(&dir).unwrap();
    let mgr = WorktreeManager::new(
        &env(),
        WorktreeOptions::new(dir.parent().unwrap().join("state")),
    )
    .unwrap();
    let a = mgr
        .create(&dir, "p1", dir.parent().unwrap().join("art"), false)
        .unwrap();
    assert!(a.not_a_git_repo && a.git.is_none());
    assert_eq!(a.context.root, dir);
    assert!(a.context.git_worktree.is_none());
}

#[test]
fn repo_without_commits_is_refused() {
    need_git!();
    let tmp = tempfile::tempdir().unwrap();
    let dir = synthesis_core::canonicalize(tmp.path())
        .unwrap()
        .join("empty");
    fs::create_dir_all(&dir).unwrap();
    g(&dir, &["init", "-q"]);
    let mgr = WorktreeManager::new(
        &env(),
        WorktreeOptions::new(dir.parent().unwrap().join("state")),
    )
    .unwrap();
    assert!(matches!(
        mgr.create(&dir, "e1", dir.join("art"), false),
        Err(Error::Refused(_))
    ));
}

// ---- ResourceManager ----

struct Fake(AtomicBool);
impl SessionResources for Fake {
    fn live(&self, _: &str) -> Vec<String> {
        if self.0.load(Ordering::SeqCst) {
            vec!["pg:dev-server".into()]
        } else {
            vec![]
        }
    }
    fn shutdown(&self, _: &str) {}
}

#[test]
fn ports_are_exclusive_released_on_drop_and_close() {
    let tmp = tempfile::tempdir().unwrap();
    let art = tmp.path().join("art");
    // Wide range: unrelated sockets may hold individual ephemeral ports.
    let rm = ResourceManager::new(50300..50400);
    rm.open_session("a", art.clone());
    rm.open_session("b", art.clone());
    let leases: Vec<_> = (0..20)
        .map(|i| rm.reserve_port(if i % 2 == 0 { "a" } else { "b" }).unwrap())
        .collect();
    let mut ports: Vec<u16> = leases.iter().map(|l| l.port).collect();
    ports.sort();
    ports.dedup();
    assert_eq!(ports.len(), 20, "no port handed out twice while held");
    // Exhaust a one-port range: held -> refused; dropped -> reusable.
    let one = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let p = one.local_addr().unwrap().port();
    let tiny = ResourceManager::new(p..p + 1);
    tiny.open_session("a", art.clone());
    assert!(tiny.reserve_port("a").is_err(), "OS-held port is skipped");
    drop(one);
    // `p` came from the OS ephemeral pool, so any outgoing connection or bind-probe in another
    // process can briefly own it after we release it: wait (bounded) for it to be free again.
    let reserve = || {
        let start = std::time::Instant::now();
        loop {
            match tiny.reserve_port("a") {
                Ok(l) => return l,
                Err(e) => assert!(
                    start.elapsed() < std::time::Duration::from_secs(10),
                    "port {p} never became free: {e}"
                ),
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    };
    let l = reserve();
    assert!(
        tiny.reserve_port("a").is_err(),
        "manager-held port is skipped"
    );
    drop(l);
    drop(reserve());
    // close_session releases everything the session still holds.
    let rep = rm.close_session("a").unwrap();
    assert_eq!(rep.released_ports.len(), 10);
    assert!(rm.close_session("a").is_err());
    drop(leases);
}

#[test]
fn close_session_reports_leaks_and_removes_temp() {
    let tmp = tempfile::tempdir().unwrap();
    let art = tmp.path().join("art");
    let rm = ResourceManager::new(50200..50300);
    rm.open_session("s", art.clone());
    let leaky = Arc::new(Fake(AtomicBool::new(true)));
    rm.register("s", leaky).unwrap();
    let t = rm.temp_dir("s", "build").unwrap();
    assert!(t.starts_with(&art) && t.is_dir());
    assert!(rm.temp_dir("s", "../x").is_err());
    let rep = rm.close_session("s").unwrap();
    assert_eq!(rep.leaked, ["pg:dev-server"]);
    assert!(!t.exists());

    rm.open_session("t", art);
    rm.register("t", Arc::new(Fake(AtomicBool::new(false))))
        .unwrap();
    assert!(rm.close_session("t").unwrap().leaked.is_empty());
}

// ---- checkpoint restore ----

#[test]
fn restore_files_rewinds_agent_edits_and_refuses_user_edits() {
    let tmp = tempfile::tempdir().unwrap();
    let base = synthesis_core::canonicalize(tmp.path()).unwrap();
    let wt = base.join("wt");
    fs::create_dir_all(&wt).unwrap();
    fs::write(wt.join("a.txt"), "v1").unwrap();
    fs::write(wt.join("b.txt"), "b1").unwrap();
    let ws = WorkspaceContext::open("w", &wt, &wt, &[], base.join("art"), "pg").unwrap();
    let home = base.join("home");
    let store = Store::open(&home, StoreOptions::default()).unwrap();
    let mut w = store
        .create_session(NewSession {
            id: Some("s1".into()),
            workspace: &ws,
            model_config_id: "m".into(),
            permission_mode: PermissionMode::Default,
            permission_policy_id: "p".into(),
            budget: Budget::default(),
            parent: None,
            title: None,
        })
        .unwrap();
    let cp = w
        .create_checkpoint(
            "cp",
            &[
                PathBuf::from("a.txt"),
                PathBuf::from("b.txt"),
                PathBuf::from("new.txt"),
            ],
        )
        .unwrap();
    let reader = store.reader().unwrap();
    let fetch = |id: &str| reader.artifact_bytes("s1", id);
    let key = |n: &str| wt.join(n).to_string_lossy().into_owned();

    // Agent edits a, deletes b, creates new.
    fs::write(wt.join("a.txt"), "agent").unwrap();
    fs::remove_file(wt.join("b.txt")).unwrap();
    fs::write(wt.join("new.txt"), "n").unwrap();
    let ledger: Ledger = [
        (key("a.txt"), Some(hash_bytes(b"agent"))),
        (key("b.txt"), None),
        (key("new.txt"), Some(hash_bytes(b"n"))),
    ]
    .into();

    // Dry run changes nothing.
    let o = restore_files(&ws, &cp, &ledger, fetch, true, false).unwrap();
    assert!(!o.applied);
    assert_eq!(fs::read_to_string(wt.join("a.txt")).unwrap(), "agent");
    assert_eq!(o.plan.rewind.message_count, cp.message_count);

    // User edits a.txt after the agent: everything is refused, nothing written.
    fs::write(wt.join("a.txt"), "user touched").unwrap();
    let o = restore_files(&ws, &cp, &ledger, fetch, false, false).unwrap();
    assert!(!o.applied);
    assert!(matches!(
        &o.plan.refusals()[..],
        [(_, FileAction::Refuse(Refusal::UserModified))]
    ));
    assert_eq!(
        fs::read_to_string(wt.join("a.txt")).unwrap(),
        "user touched"
    );
    assert!(!wt.join("b.txt").exists() && wt.join("new.txt").exists());

    // Back to the agent's state: real restore.
    fs::write(wt.join("a.txt"), "agent").unwrap();
    let o = restore_files(&ws, &cp, &ledger, fetch, false, false).unwrap();
    assert!(o.applied);
    assert_eq!(fs::read_to_string(wt.join("a.txt")).unwrap(), "v1");
    assert_eq!(fs::read_to_string(wt.join("b.txt")).unwrap(), "b1");
    assert!(!wt.join("new.txt").exists());

    // Unknown agent state is refused too; idempotent afterwards.
    let o = restore_files(&ws, &cp, &Ledger::new(), fetch, false, false).unwrap();
    assert!(!o.applied);
    let ledger: Ledger = [
        (key("a.txt"), Some(hash_bytes(b"v1"))),
        (key("b.txt"), Some(hash_bytes(b"b1"))),
        (key("new.txt"), None),
    ]
    .into();
    let o = restore_files(&ws, &cp, &ledger, fetch, false, false).unwrap();
    assert!(
        o.applied
            && o.plan
                .files
                .iter()
                .all(|(_, a)| *a == FileAction::Unchanged)
    );
}

#[test]
fn restore_refuses_lossy_snapshots_unless_skipped() {
    let tmp = tempfile::tempdir().unwrap();
    let base = synthesis_core::canonicalize(tmp.path()).unwrap();
    let wt = base.join("wt");
    fs::create_dir_all(&wt).unwrap();
    fs::write(wt.join(".env"), "TOKEN=real-secret-value\n").unwrap();
    fs::write(wt.join("a.txt"), "v1").unwrap();
    let ws = WorkspaceContext::open("w", &wt, &wt, &[], base.join("art"), "pg").unwrap();
    let store = Store::open(&base.join("home"), StoreOptions::default()).unwrap();
    let mut w = store
        .create_session(NewSession {
            id: Some("s1".into()),
            workspace: &ws,
            model_config_id: "m".into(),
            permission_mode: PermissionMode::Default,
            permission_policy_id: "p".into(),
            budget: Budget::default(),
            parent: None,
            title: None,
        })
        .unwrap();
    let cp = w
        .create_checkpoint("cp", &[PathBuf::from(".env"), PathBuf::from("a.txt")])
        .unwrap();
    assert!(cp.files.iter().any(|f| f.lossy_sha256.is_some()));
    let reader = store.reader().unwrap();
    let fetch = |id: &str| reader.artifact_bytes("s1", id);
    let key = |n: &str| wt.join(n).to_string_lossy().into_owned();
    fs::write(wt.join(".env"), "TOKEN=changed\n").unwrap();
    fs::write(wt.join("a.txt"), "agent").unwrap();
    let ledger: Ledger = [
        (key(".env"), Some(hash_bytes(b"TOKEN=changed\n"))),
        (key("a.txt"), Some(hash_bytes(b"agent"))),
    ]
    .into();
    let lossy = |o: &RestoreOutcome| {
        o.plan.files.iter().any(
            |(_, a)| matches!(a, FileAction::RefusedLossySnapshot { path, .. } if path.ends_with(".env")),
        )
    };

    // Default: nothing is written, the plan names the .env.
    let o = restore_files(&ws, &cp, &ledger, fetch, false, false).unwrap();
    assert!(!o.applied && lossy(&o));
    assert_eq!(fs::read_to_string(wt.join("a.txt")).unwrap(), "agent");
    assert_eq!(fs::read(wt.join(".env")).unwrap(), b"TOKEN=changed\n");

    // skip_lossy: the normal file is restored, the .env stays byte-identical.
    let o = restore_files(&ws, &cp, &ledger, fetch, false, true).unwrap();
    assert!(o.applied && lossy(&o));
    assert_eq!(fs::read_to_string(wt.join("a.txt")).unwrap(), "v1");
    assert_eq!(fs::read(wt.join(".env")).unwrap(), b"TOKEN=changed\n");
}

#[test]
fn rewritten_dot_git_pointer_is_refused_never_the_decoy() {
    need_git!();
    let f = fx();
    let (_, info) = alloc(&f, "gp", false);
    fs::write(info.worktree.join("real.txt"), "agent\n").unwrap();
    // Decoy repo with a different history, laid out like the worktree.
    let decoy = f.base.join("decoy");
    fs::create_dir_all(&decoy).unwrap();
    g(&decoy, &["init", "-q"]);
    fs::write(decoy.join("fake.txt"), "decoy\n").unwrap();
    g(&decoy, &["add", "."]);
    g(&decoy, &["commit", "-qm", "decoy"]);
    let decoy_head = g(&decoy, &["rev-parse", "HEAD"]);
    // The sandboxed agent rewrites the pointer file.
    let dot_git = info.worktree.join(".git");
    let real_ptr = fs::read_to_string(&dot_git).unwrap();
    fs::write(
        &dot_git,
        format!("gitdir: {}\n", decoy.join(".git").display()),
    )
    .unwrap();

    let refused = |r: Result<()>| assert!(matches!(r, Err(Error::Refused(_))), "{r:?}");
    refused(f.mgr.changes(&info).map(drop));
    refused(f.mgr.diff(&info).map(drop));
    refused(f.mgr.commit_all(&info, "x").map(drop));
    refused(f.mgr.plan_merge(&info).map(drop));
    refused(f.mgr.cleanup(&info, false));
    assert_eq!(
        g(&decoy, &["rev-parse", "HEAD"]),
        decoy_head,
        "decoy untouched"
    );

    // Replacing the file by a real directory (own repo) is refused too.
    fs::remove_file(&dot_git).unwrap();
    g(&info.worktree, &["init", "-q"]);
    refused(f.mgr.diff(&info).map(drop));
    fs::remove_dir_all(&dot_git).unwrap();
    fs::write(&dot_git, real_ptr).unwrap();
    let ch = f.mgr.changes(&info).unwrap();
    assert!(ch.agent.iter().any(|c| c.path == "real.txt"));
    assert!(!ch.agent.iter().any(|c| c.path == "fake.txt"));
}

#[test]
fn tampered_admin_files_and_pointer_forms_are_refused_without_running_git() {
    need_git!();
    let f = fx();
    let (_, info) = alloc(&f, "adm", false);
    assert!(f.mgr.changes(&info).is_ok());
    let refused = |r: Result<()>| assert!(matches!(r, Err(Error::Refused(_))), "{r:?}");
    let swap = |p: &Path, bad: &str, check: &dyn Fn()| {
        let good = fs::read_to_string(p).unwrap();
        fs::write(p, bad).unwrap();
        check();
        fs::write(p, good).unwrap();
    };
    // admin `commondir` / `gitdir` rewritten
    swap(
        &info.git_dir.join("commondir"),
        "../../../elsewhere\n",
        &|| refused(f.mgr.diff(&info).map(drop)),
    );
    swap(
        &info.git_dir.join("gitdir"),
        &format!("{}/x/.git\n", f.base.display()),
        &|| refused(f.mgr.diff(&info).map(drop)),
    );
    // pointer: relative traversal, extra garbage first line, missing
    let dot_git = info.worktree.join(".git");
    swap(&dot_git, "gitdir: ../../repo/.git\n", &|| {
        refused(f.mgr.diff(&info).map(drop))
    });
    swap(&dot_git, "garbage\ngitdir: x\n", &|| {
        refused(f.mgr.diff(&info).map(drop))
    });
    // pointer replaced by a symlink to a copy of the right content is still refused
    #[cfg(unix)]
    {
        let good = fs::read_to_string(&dot_git).unwrap();
        let copy = f.base.join("ptr-copy");
        fs::write(&copy, good).unwrap();
        fs::remove_file(&dot_git).unwrap();
        std::os::unix::fs::symlink(&copy, &dot_git).unwrap();
        refused(f.mgr.diff(&info).map(drop));
        fs::remove_file(&dot_git).unwrap();
        fs::write(&dot_git, fs::read_to_string(&copy).unwrap()).unwrap();
    }
    assert!(f.mgr.changes(&info).is_ok(), "restored state works again");
}
