#![cfg(unix)]
//! The attempt of a read-only goal (S1 section 2.1): in place, no worktree, no git, the report as
//! the subject, and nothing here can verify it (P2a). Real store, a plain folder that is no git
//! repository, a worker that is `sh -c 'exit 0'`.

#[macro_use]
mod common;
use common::*;

use std::ffi::OsString;
use std::io;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use sha2::{Digest, Sha256};
use synthesis_core::WorkspaceContext;
use synthesis_core::outcome::*;
use synthesis_orchestrator::*;
use synthesis_session::outcome::*;
use synthesis_session::{NewMessage, OsProbe};

/// A worker that exits 0 at once and remembers the arguments it was given.
#[derive(Default)]
struct Sh(Mutex<Vec<Vec<OsString>>>);
impl Spawn for Sh {
    fn spawn(&self, p: &Profile) -> io::Result<Child> {
        self.0.lock().unwrap().push(p.argv());
        Command::new("sh")
            .args(["-c", "exit 0"])
            .stdin(Stdio::null())
            .process_group(0)
            .spawn()
    }
}

struct Idle;
impl CheckRunner for Idle {
    fn fingerprint(&self) -> Result<String, VerifyError> {
        Ok("fp".into())
    }
    fn run(
        &self,
        _: &Path,
        _: &Path,
        _: &ContractCheck,
        _: Duration,
        _: &std::sync::atomic::AtomicBool,
    ) -> CheckRun {
        unreachable!("a read-only goal runs no check")
    }
}

fn contract_ro() -> Contract {
    Contract::read_only(
        "review the module".into(),
        &["names the risks".into()],
        Budgets::new(5.0),
    )
}

/// A plain folder (no `.git`) with a file in it.
fn folder(r: &Rig) -> PathBuf {
    let d = r.base.join("plain");
    std::fs::create_dir_all(&d).unwrap();
    std::fs::write(d.join("a.txt"), "one\n").unwrap();
    d
}

/// A running read-only outcome whose `research` item is leased.
fn ro_outcome(r: &Rig, root: &Path, c: &Contract) -> (OutcomeWriter, WorkItemId) {
    let ws = WorkspaceContext::open("w", root, root, &[], r.base.join("cart"), "pg").unwrap();
    let mut w = r
        .store
        .create_outcome(NewOutcome {
            workspace: &ws,
            base_commit: String::new(),
            contract: c,
            policy: &policy(),
        })
        .unwrap();
    let none = Effects::default();
    let go = |w: &mut OutcomeWriter, to| {
        let rev = w.get_outcome(w.id()).unwrap().revision;
        w.transition_outcome(rev, to, None, &none).unwrap();
    };
    go(&mut w, OutcomeState::ContractPending);
    let h = w.get_contract(w.id(), 1).unwrap().1;
    w.approve_contract(1, &h, "user").unwrap();
    let (plan, _) = make_plan(&StubPlanner, c, 1).unwrap();
    let items = w.insert_plan(&plan).unwrap();
    go(&mut w, OutcomeState::Ready);
    go(&mut w, OutcomeState::Running);
    let it = items[0].1.clone();
    for to in [WorkItemState::Ready, WorkItemState::Leased] {
        let rev = w.get_work_item(&it).unwrap().revision;
        w.transition_work_item(&it, rev, to, &none).unwrap();
    }
    (w, it)
}

fn spec<'a>(it: &'a WorkItemId, root: &'a Path) -> AllocSpec<'a> {
    AllocSpec {
        item: it,
        ordinal: 1,
        source: root,
        base_commit: "",
        worker_kind: "local-engine",
    }
}

fn profile(env: &AttemptEnv, c: &Contract) -> Profile {
    Profile::build(
        PathBuf::from("synt"),
        env,
        c,
        &policy(),
        0.0,
        "review".into(),
    )
    .unwrap()
}

fn tree(d: &Path) -> Vec<(String, Vec<u8>)> {
    let mut v: Vec<_> = std::fs::read_dir(d)
        .unwrap()
        .map(|e| {
            let p = e.unwrap().path();
            (
                p.file_name().unwrap().to_string_lossy().into_owned(),
                std::fs::read(&p).unwrap(),
            )
        })
        .collect();
    v.sort();
    v
}

#[test]
fn ro_stub_plan_is_one_research_item_and_the_write_plan_is_unchanged() {
    let (plan, valid) = make_plan(&StubPlanner, &contract_ro(), 1).unwrap();
    assert_eq!(plan.items.len(), 1);
    assert_eq!(plan.items[0].key, "research");
    assert_eq!(plan.items[0].kind, WorkItemKind::Agent);
    assert_eq!(plan.items[0].capabilities, ["repo.read"]);
    assert_eq!(valid.order, ["research"]);
    let (write, _) = make_plan(&StubPlanner, &contract(), 1).unwrap();
    let keys: Vec<_> = write.items.iter().map(|i| i.key.as_str()).collect();
    assert_eq!(keys, ["inspect", "implement", "test"]);
}

#[test]
fn ro_attempt_is_allocated_in_place_in_a_folder_that_is_not_a_git_repository() {
    let r = rig();
    let root = folder(&r);
    assert!(!root.join(".git").exists());
    let c = contract_ro();
    let (mut w, it) = ro_outcome(&r, &root, &c);
    let before = tree(&root);
    let env = r
        .allocator()
        .allocate_in_place(&mut w, &spec(&it, &root), now())
        .unwrap();
    assert!(env.git.is_none(), "no worktree, no git info");
    assert_eq!(env.context.cwd, root);
    assert_eq!(env.context.root, root);
    assert!(
        !r.home.join("worktrees").exists()
            || r.home
                .join("worktrees")
                .read_dir()
                .unwrap()
                .next()
                .is_none(),
        "no worktree was made"
    );
    assert_eq!(tree(&root), before, "the folder was not touched");
    assert!(!root.join(".git").exists());
    // a replay finds the same attempt and session
    let again = r
        .allocator()
        .allocate_in_place(&mut w, &spec(&it, &root), now())
        .unwrap();
    assert_eq!(again.session_id, env.session_id);
    assert!(again.git.is_none());
    // the kinds never mix: a write allocation of that attempt, and an in-place one of a worktree
    // attempt, are refused whatever the rows say
    assert!(
        r.allocator()
            .allocate(&mut w, &spec(&it, &root), now())
            .is_err()
    );
}

#[test]
fn ro_attempt_on_native_windows_is_refused_like_a_write_attempt() {
    let r = rig();
    let root = folder(&r);
    let (mut w, it) = ro_outcome(&r, &root, &contract_ro());
    let a = Allocator {
        platform: Platform {
            native_windows: true,
        },
        ..r.allocator()
    };
    let e = a
        .allocate_in_place(&mut w, &spec(&it, &root), now())
        .unwrap_err();
    assert!(matches!(e, AllocError::NativeWindows), "{e}");
}

#[test]
fn ro_profile_from_stored_contract_has_four_read_tools_no_write_and_no_network() {
    let r = rig();
    let root = folder(&r);
    let c = contract_ro();
    let (mut w, it) = ro_outcome(&r, &root, &c);
    let env = r
        .allocator()
        .allocate_in_place(&mut w, &spec(&it, &root), now())
        .unwrap();
    // the kind is the stored contract's: read it back as the owner does
    let stored = w.get_contract(w.id(), 1).unwrap().0;
    assert_eq!(stored.kind, ContractKind::ReadOnly);
    let p = profile(&env, &stored);
    assert!(p.is_read_only());
    assert_eq!(p.permission_mode(), "dontAsk");
    let argv: Vec<String> = p
        .argv()
        .iter()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    let has = |a: &str| argv.iter().any(|x| x == a);
    for flag in [
        "--read-only-session",
        "--attempt-session",
        "--bare",
        "--unattended",
    ] {
        assert!(has(flag), "{flag} in {argv:?}");
    }
    let after = |flag: &str| argv[argv.iter().position(|x| x == flag).unwrap() + 1].clone();
    assert_eq!(after("--sandbox"), "enforce");
    assert_eq!(after("--network"), "deny");
    assert_eq!(after("--permission-mode"), "dontAsk");
    assert_eq!(after("--max-turns"), "80");
    // nothing that could write or widen: no allow rule, no worktree, no extra directory, no trust
    for flag in [
        "--allow",
        "--worktree",
        "--add-dir",
        "--trust-project",
        "--approve-mcp",
        "--allow-host",
        "--no-worktree",
    ] {
        assert!(!has(flag), "{flag} must not be there: {argv:?}");
    }
    let denied: Vec<&String> = argv
        .iter()
        .enumerate()
        .filter(|(i, _)| *i > 0 && argv[i - 1] == "--deny")
        .map(|(_, a)| a)
        .collect();
    for rule in ["Bash", "Edit", "WebFetch", "Task", "Read(**/.env)"] {
        assert!(
            denied.iter().any(|d| d.as_str() == rule),
            "{rule} denied: {denied:?}"
        );
    }
    // a caller's `--max-turns` replaces the default instead of repeating it
    let p2 = profile(&env, &stored)
        .with_extra(&["--max-turns", "5"])
        .unwrap();
    let n = p2.argv().iter().filter(|a| *a == "--max-turns").count();
    assert_eq!(n, 1);
    // no grant can be added to it
    assert!(
        profile(&env, &stored)
            .with_granted_rules(&["Edit(/a.txt)".into()])
            .is_err()
    );
    // the write profile of the same builder is unchanged
    let wr = Profile::build(
        PathBuf::from("synt"),
        &env,
        &contract(),
        &policy(),
        0.0,
        "x".into(),
    )
    .unwrap();
    assert!(!wr.is_read_only());
    assert!(!wr.argv().iter().any(|a| a == "--read-only-session"));
    assert_eq!(wr.permission_mode(), "default");
}

fn write_report(r: &Rig, w: &OutcomeWriter, sid: &str, text: &str) {
    let mut s = r.store.open_attempt_session(w, sid).unwrap();
    s.append_message(NewMessage::assistant(
        serde_json::json!([{"type": "text", "text": text}]),
    ))
    .unwrap();
}

#[test]
fn ro_report_is_the_subject_and_nothing_here_verifies_it() {
    let r = rig();
    let root = folder(&r);
    let c = contract_ro();
    let (mut w, it) = ro_outcome(&r, &root, &c);
    let env = r
        .allocator()
        .allocate_in_place(&mut w, &spec(&it, &root), now())
        .unwrap();
    write_report(&r, &w, &env.session_id, "the report: risk one");
    let spawn = Sh::default();
    let runner = Runner::new(
        &r.store,
        &r.wm,
        &spawn,
        &OsProbe,
        Duration::from_millis(40),
        Duration::from_millis(400),
    );
    let s = runner
        .run_attempt(&mut w, &env, &profile(&env, &c), &CancelFlag::default())
        .unwrap();
    assert!(matches!(s.end, WorkerEnd::Completed));
    let sha: String = Sha256::digest(b"the report: risk one")
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    assert_eq!(
        s.subject_commit.as_deref(),
        Some(format!("report:sha256:{sha}").as_str())
    );
    assert_eq!(
        s.item,
        WorkItemState::Verifying,
        "waits for the person's review"
    );
    let a = w.list_attempts(&it).unwrap().remove(0);
    assert_eq!(a.state, AttemptState::Finished);
    assert_eq!(a.subject_commit, s.subject_commit);
    assert_eq!(
        attempt_report(&r.store, &env.session_id).unwrap().subject(),
        a.subject_commit.unwrap()
    );
    // the worker was started with the read-only profile
    assert!(
        spawn.0.lock().unwrap()[0]
            .iter()
            .any(|a| a == "--read-only-session")
    );
    // the independent verifier cannot judge a report: it waits, writes nothing, verifies nothing
    let mut v = IndependentVerifier::for_outcome(
        &w,
        root.clone(),
        r.home.clone(),
        Arc::new(Idle),
        Duration::from_secs(5),
    )
    .unwrap();
    let item = w.get_work_item(&it).unwrap();
    for _ in 0..3 {
        assert_eq!(v.verify(&mut w, &item).unwrap(), Verdict::Pending);
    }
    assert!(w.authentic_evidence(w.id()).unwrap().is_empty());
    assert_ne!(w.get_outcome(w.id()).unwrap().state, OutcomeState::Verified);
    assert_eq!(tree(&root), vec![("a.txt".to_string(), b"one\n".to_vec())]);
}

#[test]
fn ro_attempt_without_a_report_fails_and_never_blocks_on_a_decision() {
    let r = rig();
    let root = folder(&r);
    let c = contract_ro();
    let (mut w, it) = ro_outcome(&r, &root, &c);
    let env = r
        .allocator()
        .allocate_in_place(&mut w, &spec(&it, &root), now())
        .unwrap();
    let spawn = Sh::default();
    let runner = Runner::new(
        &r.store,
        &r.wm,
        &spawn,
        &OsProbe,
        Duration::from_millis(40),
        Duration::from_millis(400),
    );
    // exit 0 and no final message: nothing to review
    let s = runner
        .run_attempt(&mut w, &env, &profile(&env, &c), &CancelFlag::default())
        .unwrap();
    assert_eq!(s.end, WorkerEnd::Failed);
    assert_eq!(s.subject_commit, None);
    assert!(matches!(
        s.item,
        WorkItemState::RetryableFailure | WorkItemState::TerminalFailure
    ));
    assert!(w.list_decisions(w.id()).unwrap().is_empty());
}
