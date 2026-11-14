//! The attempt runner against scripted workers (a shell script stands in for `synt run`), real
//! git worktrees and a real v5 store. Unix only: the worker is a process tree.
#![cfg(unix)]

#[macro_use]
mod common;
use common::*;

use std::fs;
use std::io;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use synthesis_core::outcome::*;
use synthesis_orchestrator::*;
use synthesis_session::outcome::*;
use synthesis_session::{OsProbe, ProcessProbe};
use time::OffsetDateTime;

struct Script(String);

impl Spawn for Script {
    fn spawn(&self, p: &Profile) -> io::Result<Child> {
        use std::os::unix::process::CommandExt;
        Command::new("sh")
            .arg("-c")
            .arg(&self.0)
            .current_dir(p.cwd())
            .stdin(Stdio::null())
            .process_group(0)
            .spawn()
    }
}

fn runner<'a>(r: &'a Rig, s: &'a Script) -> Runner<'a> {
    Runner::new(
        &r.store,
        &r.wm,
        s,
        &OsProbe,
        Duration::from_millis(40),
        Duration::from_millis(400),
    )
}

fn profile(r: &Rig, env: &AttemptEnv, p: &OutcomePolicy) -> Profile {
    let c = r.contract_for_profile();
    Profile::build(PathBuf::from("sh"), env, &c, p, 0.0, "do the work".into()).unwrap()
}

fn alive(pid: u32) -> bool {
    Command::new("kill")
        .args(["-0", &pid.to_string()])
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

fn item_state(w: &OutcomeWriter, it: &WorkItemId) -> WorkItemState {
    w.list_work_items(w.id())
        .unwrap()
        .into_iter()
        .find(|i| i.id == *it)
        .unwrap()
        .state
}

fn attempt(w: &OutcomeWriter, it: &WorkItemId, ordinal: u32) -> AttemptRecord {
    w.list_attempts(it)
        .unwrap()
        .into_iter()
        .find(|a| a.ordinal == ordinal)
        .unwrap()
}

fn run_script(
    r: &Rig,
    w: &mut OutcomeWriter,
    it: &WorkItemId,
    ordinal: u32,
    script: &str,
) -> (AttemptEnv, Settled) {
    let env = r
        .allocator()
        .allocate(w, &r.spec(it, ordinal), OffsetDateTime::now_utc())
        .unwrap();
    let s = Script(script.into());
    let settled = runner(r, &s)
        .run_attempt(
            w,
            &env,
            &profile(r, &env, &policy()),
            &CancelFlag::default(),
        )
        .unwrap();
    (env, settled)
}

#[test]
fn a_normal_run_commits_the_tree_and_hands_over_to_verification() {
    need_git!();
    let r = rig();
    let (mut w, one, _) = r.outcome();
    let (env, s) = run_script(&r, &mut w, &one, 1, "echo made > out.txt; exit 0");
    assert_eq!(
        (s.end, s.item),
        (WorkerEnd::Completed, WorkItemState::Verifying)
    );
    let commit = s.subject_commit.unwrap();
    assert_eq!(common::g(&env.git.worktree, &["rev-parse", "HEAD"]), commit);
    assert_eq!(
        common::g(
            &env.git.worktree,
            &["show", "--name-only", "--format=", "HEAD"]
        ),
        "out.txt"
    );
    let a = attempt(&w, &one, 1);
    assert_eq!(
        (a.state, a.task_status.as_deref()),
        (AttemptState::Finished, Some("completed"))
    );
    assert_eq!(a.subject_commit.as_deref(), Some(commit.as_str()));
    assert!(a.worker_pid.is_some() && a.heartbeat_at.is_some() && a.ended_at.is_some());
    // the worker was marked as started before it ran: the worktree is never reset again
    assert!(r.wm.is_started(&env.git));
    assert!(
        r.wm.allocate_unstarted(&r.repo, &env.session_id, r.base.join("art"), &r.head)
            .is_err()
    );
}

#[test]
fn failures_retry_within_the_contracts_budget_and_then_fail_for_good() {
    need_git!();
    let r = rig();
    let (mut w, one, _) = r.outcome();
    // the default budget is 3 attempts: two retries
    let (_, s1) = run_script(&r, &mut w, &one, 1, "exit 1");
    assert_eq!(
        (s1.end, s1.item),
        (WorkerEnd::Failed, WorkItemState::RetryableFailure)
    );
    for (to, _) in [(WorkItemState::Ready, ()), (WorkItemState::Leased, ())] {
        let rev = w
            .list_work_items(w.id())
            .unwrap()
            .into_iter()
            .find(|i| i.id == one)
            .unwrap()
            .revision;
        w.transition_work_item(&one, rev, to, &Effects::default())
            .unwrap();
    }
    let (_, s2) = run_script(&r, &mut w, &one, 2, "exit 1");
    assert_eq!(
        (s2.end, s2.item),
        (WorkerEnd::Failed, WorkItemState::RetryableFailure)
    );
    for to in [WorkItemState::Ready, WorkItemState::Leased] {
        let rev = w
            .list_work_items(w.id())
            .unwrap()
            .into_iter()
            .find(|i| i.id == one)
            .unwrap()
            .revision;
        w.transition_work_item(&one, rev, to, &Effects::default())
            .unwrap();
    }
    let (_, s3) = run_script(&r, &mut w, &one, 3, "exit 70");
    assert_eq!(
        (s3.end, s3.item),
        (WorkerEnd::Failed, WorkItemState::TerminalFailure)
    );
    // a fourth attempt is not possible
    assert_eq!(item_state(&w, &one), WorkItemState::TerminalFailure);
    let statuses: Vec<_> = w
        .list_attempts(&one)
        .unwrap()
        .iter()
        .map(|a| a.task_status.clone().unwrap())
        .collect();
    assert_eq!(statuses, ["failed", "failed", "failed"]);
}

#[test]
fn blocked_waits_for_a_decision_and_exit_codes_map_as_documented() {
    need_git!();
    let r = rig();
    let (mut w, one, _) = r.outcome();
    let (_, s) = run_script(&r, &mut w, &one, 1, "exit 4");
    assert_eq!(
        (s.end, s.item),
        (WorkerEnd::Blocked, WorkItemState::WaitingDecision)
    );
    for (code, cancelled, wall, want) in [
        (Some(0), false, false, WorkerEnd::Completed),
        (Some(1), false, false, WorkerEnd::Failed),
        (Some(2), false, false, WorkerEnd::Failed),
        (Some(3), false, false, WorkerEnd::Unverified),
        (Some(4), false, false, WorkerEnd::Blocked),
        (Some(70), false, false, WorkerEnd::Failed),
        (Some(130), false, false, WorkerEnd::Cancelled),
        (None, false, false, WorkerEnd::Failed),
        (None, true, false, WorkerEnd::Cancelled),
        (Some(0), false, true, WorkerEnd::Failed),
    ] {
        assert_eq!(
            map_exit(code, cancelled, wall),
            want,
            "{code:?} {cancelled} {wall}"
        );
    }
}

#[test]
fn cancel_stops_the_worker_and_kills_everything_it_started() {
    need_git!();
    let r = rig();
    let (mut w, one, _) = r.outcome();
    let env = r
        .allocator()
        .allocate(&mut w, &r.spec(&one, 1), OffsetDateTime::now_utc())
        .unwrap();
    // the worker starts a background subtree, as a test command would
    let s =
        Script("sleep 300 & echo $! > child.pid; sleep 300 & echo $! > child2.pid; wait".into());
    let flag = CancelFlag::default();
    let f2 = flag.clone();
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(600));
        f2.cancel();
    });
    let began = Instant::now();
    let settled = runner(&r, &s)
        .run_attempt(&mut w, &env, &profile(&r, &env, &policy()), &flag)
        .unwrap();
    assert!(
        began.elapsed() < Duration::from_secs(10),
        "{:?}",
        began.elapsed()
    );
    assert_eq!(
        (settled.end, settled.item, settled.subject_commit),
        (WorkerEnd::Cancelled, WorkItemState::Cancelled, None)
    );
    for f in ["child.pid", "child2.pid"] {
        let pid: u32 = fs::read_to_string(env.git.worktree.join(f))
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        // allow the kernel a moment to reap the orphans
        let until = Instant::now() + Duration::from_secs(3);
        while alive(pid) && Instant::now() < until {
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(!alive(pid), "{f}: the subtree survived the cancel");
    }
    let a = attempt(&w, &one, 1);
    assert_eq!(a.state, AttemptState::Cancelled);
    assert!(a.retained_reason.unwrap().contains("kept"));
    assert!(
        env.git.worktree.join("child.pid").exists(),
        "the tree is kept as it was"
    );
    // the cooperative request was made through the store (the worker polls it)
    let stop: Option<String> = rusqlite::Connection::open(r.home.join("synt.db"))
        .unwrap()
        .query_row(
            "SELECT stop_requested_at FROM sessions WHERE id=?1",
            [&env.session_id],
            |row| row.get(0),
        )
        .unwrap();
    assert!(stop.is_some());
}

#[test]
fn a_worker_over_its_wall_cap_is_killed_and_fails() {
    need_git!();
    let r = rig();
    let (mut w, one, _) = r.outcome();
    let env = r
        .allocator()
        .allocate(&mut w, &r.spec(&one, 1), OffsetDateTime::now_utc())
        .unwrap();
    let mut p = policy();
    p.attempt_max_wall_secs = Some(1);
    let prof = profile(&r, &env, &p);
    assert_eq!(prof.max_wall_secs(), 1);
    let s = Script("sleep 60".into());
    let began = Instant::now();
    let settled = runner(&r, &s)
        .run_attempt(&mut w, &env, &prof, &CancelFlag::default())
        .unwrap();
    assert!(began.elapsed() < Duration::from_secs(10));
    assert_eq!(settled.end, WorkerEnd::Failed);
}

// ------------------------------------------------------------------- fencing

#[test]
fn a_stale_worker_cannot_write_through_a_wrong_fence_or_after_it_was_superseded() {
    need_git!();
    let r = rig();
    let (mut w, one, _) = r.outcome();
    let env = r
        .allocator()
        .allocate(&mut w, &r.spec(&one, 1), OffsetDateTime::now_utc())
        .unwrap();
    let id = env.attempt.id.clone();
    // wrong fence
    assert!(matches!(
        w.record_worker(&id, 99, 1, None),
        Err(synthesis_session::Error::Stale(_))
    ));
    assert!(matches!(
        w.heartbeat(&id, 99),
        Err(synthesis_session::Error::Stale(_))
    ));
    w.record_worker(&id, 1, 4242, Some("tok")).unwrap();
    assert!(
        w.record_worker(&id, 1, 4343, None).is_err(),
        "a worker is recorded once"
    );
    w.heartbeat(&id, 1).unwrap();
    // the owner gives up on it: lost; a new attempt takes over
    let rev = attempt(&w, &one, 1).revision;
    w.transition_attempt(
        &id,
        rev,
        AttemptState::Lost,
        &AttemptEnd::default(),
        &Effects::default(),
    )
    .unwrap();
    for to in [
        WorkItemState::Recovering,
        WorkItemState::Ready,
        WorkItemState::Leased,
    ] {
        let rev = w
            .list_work_items(w.id())
            .unwrap()
            .into_iter()
            .find(|i| i.id == one)
            .unwrap()
            .revision;
        w.transition_work_item(&one, rev, to, &Effects::default())
            .unwrap();
    }
    let env2 = r
        .allocator()
        .allocate(&mut w, &r.spec(&one, 2), OffsetDateTime::now_utc())
        .unwrap();
    assert_eq!(env2.attempt.fence, 2);
    // the zombie worker of attempt 1 reports in: nothing is accepted
    assert!(matches!(
        w.heartbeat(&id, 1),
        Err(synthesis_session::Error::Stale(_))
    ));
    assert!(
        w.transition_attempt(
            &id,
            attempt(&w, &one, 1).revision,
            AttemptState::Finished,
            &AttemptEnd::default(),
            &Effects::default()
        )
        .is_err()
    );
    // and its late evidence is refused by the fence
    let late = NewEvidence {
        work_item_id: one.clone(),
        attempt_id: Some(id.clone()),
        trust_class: TrustClass::IndependentCheck,
        producer: "independent_runner".into(),
        subject_commit: "abc".into(),
        check_id: "unit".into(),
        command_hash: None,
        criteria_ids: vec![],
        result: "passed".into(),
        exit_code: Some(0),
        env_fingerprint: "fp".into(),
        artifact: None,
        fence: 1,
    };
    assert!(
        w.record_evidence(&late)
            .unwrap_err()
            .to_string()
            .contains("stale")
    );
}

// ------------------------------------------------------------------ recovery

fn running_with_worker(
    r: &Rig,
    w: &mut OutcomeWriter,
    it: &WorkItemId,
    ordinal: u32,
    pid: u32,
    start: Option<&str>,
) -> AttemptEnv {
    let env = r
        .allocator()
        .allocate(w, &r.spec(it, ordinal), OffsetDateTime::now_utc())
        .unwrap();
    r.wm.mark_started(&env.git).unwrap();
    let rev = attempt(w, it, ordinal).revision;
    w.transition_attempt(
        &env.attempt.id,
        rev,
        AttemptState::Running,
        &AttemptEnd::default(),
        &Effects::default(),
    )
    .unwrap();
    let rev = w
        .list_work_items(w.id())
        .unwrap()
        .into_iter()
        .find(|i| i.id == *it)
        .unwrap()
        .revision;
    w.transition_work_item(it, rev, WorkItemState::Running, &Effects::default())
        .unwrap();
    w.record_worker(&env.attempt.id, env.attempt.fence, pid, start)
        .unwrap();
    env
}

fn recover(
    r: &Rig,
    w: &mut OutcomeWriter,
    now: OffsetDateTime,
    lease: Duration,
    max: u32,
) -> RecoveryReport {
    recover_attempts(
        &r.store,
        &r.wm,
        &OsProbe,
        w,
        now,
        lease,
        max,
        Platform {
            native_windows: false,
        },
    )
    .unwrap()
}

#[test]
fn a_dead_worker_is_lost_and_its_item_freed_without_rerunning_anything() {
    need_git!();
    let r = rig();
    let (mut w, one, two) = r.outcome();
    let mut gone = Command::new("true").spawn().unwrap();
    let pid = gone.id();
    gone.wait().unwrap();
    let env = running_with_worker(&r, &mut w, &one, 1, pid, Some("a-start-token"));
    let id = w.id().clone();
    drop(w);
    // the orchestrator restarts
    let mut w = r.store.open_outcome(&id, &policy()).unwrap();
    let rep = recover(
        &r,
        &mut w,
        OffsetDateTime::now_utc(),
        Duration::from_secs(120),
        3,
    );
    assert_eq!(
        (rep.lost.clone(), rep.alive.len(), rep.settled.len()),
        (vec![env.attempt.id.clone()], 0, 0)
    );
    let a = attempt(&w, &one, 1);
    assert_eq!(a.state, AttemptState::Lost);
    assert!(a.retained_reason.unwrap().contains("worktree is kept"));
    assert_eq!(
        item_state(&w, &one),
        WorkItemState::Ready,
        "free for a NEW attempt"
    );
    assert_eq!(
        item_state(&w, &two),
        WorkItemState::Leased,
        "other items untouched"
    );
    assert!(env.git.worktree.exists());
    // a second restart finds nothing left to do
    assert_eq!(
        recover(
            &r,
            &mut w,
            OffsetDateTime::now_utc(),
            Duration::from_secs(120),
            3
        ),
        RecoveryReport::default()
    );
    // the re-run is a new attempt with a new session and worktree
    let rev = w
        .list_work_items(w.id())
        .unwrap()
        .into_iter()
        .find(|i| i.id == one)
        .unwrap()
        .revision;
    w.transition_work_item(&one, rev, WorkItemState::Leased, &Effects::default())
        .unwrap();
    let env2 = r
        .allocator()
        .allocate(&mut w, &r.spec(&one, 2), OffsetDateTime::now_utc())
        .unwrap();
    assert_ne!(env2.session_id, env.session_id);
    assert_ne!(env2.git.worktree, env.git.worktree);
}

#[test]
fn an_exhausted_budget_makes_recovery_fail_the_item_instead_of_freeing_it() {
    need_git!();
    let r = rig();
    let (mut w, one, _) = r.outcome();
    let mut gone = Command::new("true").spawn().unwrap();
    let pid = gone.id();
    gone.wait().unwrap();
    running_with_worker(&r, &mut w, &one, 1, pid, None);
    // a budget of one attempt: nothing left to retry with
    recover(
        &r,
        &mut w,
        OffsetDateTime::now_utc(),
        Duration::from_secs(120),
        1,
    );
    assert_eq!(item_state(&w, &one), WorkItemState::TerminalFailure);
}

#[test]
fn a_live_worker_with_a_fresh_lease_is_left_alone_and_a_silent_one_is_killed() {
    need_git!();
    use std::os::unix::process::CommandExt;
    let r = rig();
    let (mut w, one, two) = r.outcome();
    let mut live = Command::new("sleep")
        .arg("60")
        .process_group(0)
        .spawn()
        .unwrap();
    let start = OsProbe.start_time(live.id());
    assert!(start.is_some());
    running_with_worker(&r, &mut w, &one, 1, live.id(), start.as_deref());
    let rep = recover(
        &r,
        &mut w,
        OffsetDateTime::now_utc(),
        Duration::from_secs(120),
        3,
    );
    assert_eq!(rep.alive.len(), 1);
    assert!(rep.lost.is_empty() && rep.expired.is_empty());
    assert_eq!(attempt(&w, &one, 1).state, AttemptState::Running);
    assert!(alive(live.id()));
    // later, with its lease run out
    let later = OffsetDateTime::now_utc() + time::Duration::hours(1);
    let rep = recover(&r, &mut w, later, Duration::from_secs(120), 3);
    assert_eq!((rep.expired.len(), rep.lost.len()), (1, 1));
    live.wait().unwrap(); // reaped: it was killed
    assert!(!alive(live.id()));
    assert_eq!(attempt(&w, &one, 1).state, AttemptState::Lost);
    // a pid reused by another program (same pid, different start token) is not "alive"
    let mut other = Command::new("sleep")
        .arg("60")
        .process_group(0)
        .spawn()
        .unwrap();
    running_with_worker(&r, &mut w, &two, 1, other.id(), Some("not-the-start-token"));
    let rep = recover(
        &r,
        &mut w,
        OffsetDateTime::now_utc(),
        Duration::from_secs(120),
        3,
    );
    assert_eq!(rep.lost.len(), 1);
    assert!(alive(other.id()), "an unrelated process is never killed");
    other.kill().unwrap();
    other.wait().unwrap();
}

#[test]
fn a_worker_that_finished_before_the_owner_died_is_settled_not_lost() {
    need_git!();
    let r = rig();
    let (mut w, one, _) = r.outcome();
    let mut gone = Command::new("true").spawn().unwrap();
    let pid = gone.id();
    gone.wait().unwrap();
    let env = running_with_worker(&r, &mut w, &one, 1, pid, Some("tok"));
    fs::write(env.git.worktree.join("done.txt"), "finished work").unwrap();
    // the worker's session ended `completed` in the store
    {
        let mut sw = r.store.open_attempt_session(&w, &env.session_id).unwrap();
        for to in [
            synthesis_core::SessionStatus::Running,
            synthesis_core::SessionStatus::Verifying,
            synthesis_core::SessionStatus::Completed,
        ] {
            sw.update_status(to, None).unwrap();
        }
    }
    let rep = recover(
        &r,
        &mut w,
        OffsetDateTime::now_utc(),
        Duration::from_secs(120),
        3,
    );
    assert_eq!((rep.settled.len(), rep.lost.len()), (1, 0));
    let a = attempt(&w, &one, 1);
    assert_eq!(
        (a.state, a.task_status.as_deref()),
        (AttemptState::Finished, Some("completed"))
    );
    assert_eq!(
        common::g(
            &env.git.worktree,
            &["show", "--name-only", "--format=", "HEAD"]
        ),
        "done.txt"
    );
    assert_eq!(item_state(&w, &one), WorkItemState::Verifying);
}

// ------------------------------------------------------------------ profile

#[test]
fn the_profile_is_unattended_capped_and_cannot_be_loosened() {
    need_git!();
    let r = rig();
    let (mut w, one, _) = r.outcome();
    let env = r
        .allocator()
        .allocate(&mut w, &r.spec(&one, 1), OffsetDateTime::now_utc())
        .unwrap();
    let mut c = r.contract_for_profile();
    c.envelope.budgets.attempt_max_cost_usd = Some(2.0);
    c.envelope.budgets.attempt_max_wall_secs = Some(900);
    let mut p = policy();
    p.attempt_max_cost_usd = Some(1.5);
    p.attempt_max_wall_secs = Some(600);
    let prof = Profile::build(PathBuf::from("synt"), &env, &c, &p, 0.0, "goal".into()).unwrap();
    // the smallest of contract and settings; the outcome cap bounds it too
    assert_eq!((prof.max_cost_usd(), prof.max_wall_secs()), (1.5, 600));
    assert_eq!(prof.permission_mode(), "acceptEdits");
    let argv: Vec<String> = prof
        .argv()
        .iter()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    for want in [
        "run",
        "--unattended",
        "--attempt-session",
        "--resume",
        env.session_id.as_str(),
        "--cwd",
    ] {
        assert!(argv.iter().any(|a| a == want), "{want} in {argv:?}");
    }
    assert!(argv.windows(2).any(|w| w
        == [
            "--cwd".to_string(),
            env.context.cwd.to_string_lossy().into_owned()
        ]));
    // nothing that would loosen the unattended defaults, and the permission mode is a safe one
    for never in [
        "--sandbox",
        "--network",
        "--allow-host",
        "--trust-project",
        "--approve-mcp",
        "--settings",
        "--add-dir",
        "--no-worktree",
        "--no-session-store",
        "--no-cost-cap",
        "--peers",
        "--allow",
        "--allowedTools",
        "bypassPermissions",
        "auto",
        "--dangerously-skip-permissions",
    ] {
        assert!(!argv.iter().any(|a| a == never), "{never} in {argv:?}");
    }
    // with no per-attempt caps the outcome cap is the cap: a worker always has one
    let plain = Profile::build(
        PathBuf::from("synt"),
        &env,
        &r.contract_for_profile(),
        &policy(),
        0.0,
        "g".into(),
    )
    .unwrap();
    assert_eq!(plain.max_cost_usd(), 5.0);
    assert_eq!(plain.max_wall_secs(), DEFAULT_WALL_SECS);
    // a contract that does not allow writing gets the default mode, never more
    let mut ro = r.contract_for_profile();
    ro.envelope.capabilities.remove(&Capability::RepoWrite);
    assert_eq!(
        Profile::build(PathBuf::from("synt"), &env, &ro, &policy(), 0.0, "g".into())
            .unwrap()
            .permission_mode(),
        "default"
    );
}

#[test]
fn only_the_model_and_turn_bounds_can_be_added_to_the_profile() {
    need_git!();
    let r = rig();
    let (mut w, one, _) = r.outcome();
    let env = r
        .allocator()
        .allocate(&mut w, &r.spec(&one, 1), OffsetDateTime::now_utc())
        .unwrap();
    let base = || {
        Profile::build(
            PathBuf::from("synt"),
            &env,
            &r.contract_for_profile(),
            &policy(),
            0.0,
            "g".into(),
        )
        .unwrap()
    };
    assert!(
        base()
            .with_extra(&["--provider", "fake:x", "--model", "m", "--max-turns", "5"])
            .is_ok()
    );
    for bad in [
        vec!["--sandbox", "off"],
        vec!["--sandbox"],
        vec!["--network", "allow"],
        vec!["--trust-project", "x"],
        vec!["--approve-mcp", "x"],
        vec!["--allow-host", "x"],
        vec!["--permission-mode", "bypassPermissions"],
        vec!["--no-worktree", "x"],
        vec!["--settings", "/tmp/x"],
        vec!["--cwd", "/"],
        vec!["--model", "--sandbox"],
        vec!["--max-turns"],
    ] {
        assert!(
            matches!(base().with_extra(&bad), Err(ProfileError::Forbidden(_))),
            "{bad:?}"
        );
    }
}

#[test]
fn unverified_hands_over_to_verification_like_completed() {
    need_git!();
    let r = rig();
    let (mut w, one, _) = r.outcome();
    let (_, s) = run_script(&r, &mut w, &one, 1, "echo x > f; exit 3");
    assert_eq!(
        (s.end, s.item),
        (WorkerEnd::Unverified, WorkItemState::Verifying)
    );
    assert!(
        s.subject_commit.is_some(),
        "its tree is committed as the subject for the independent runner"
    );
    assert_eq!(
        attempt(&w, &one, 1).task_status.as_deref(),
        Some("unverified")
    );
}

// ------------------------------------------------------- review fixes (PR-05)

fn busy() -> synthesis_session::Error {
    synthesis_session::Error::Sqlite(rusqlite::Error::SqliteFailure(
        rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_BUSY),
        None,
    ))
}

fn pid_of(env: &AttemptEnv, file: &str) -> u32 {
    fs::read_to_string(env.git.worktree.join(file))
        .unwrap()
        .trim()
        .parse()
        .unwrap()
}

fn gone(pid: u32) -> bool {
    let until = Instant::now() + Duration::from_secs(3);
    while alive(pid) && Instant::now() < until {
        std::thread::sleep(Duration::from_millis(50));
    }
    !alive(pid)
}

#[test]
fn a_busy_database_never_kills_a_healthy_worker() {
    need_git!();
    let r = rig();
    let (mut w, one, _) = r.outcome();
    let env = r
        .allocator()
        .allocate(&mut w, &r.spec(&one, 1), OffsetDateTime::now_utc())
        .unwrap();
    let s = Script("sleep 1; exit 0".into());
    // five misses in a row, then the database answers: under the limit of six
    let n = std::cell::Cell::new(0u32);
    let hook = |w: &mut OutcomeWriter, id: &AttemptId, fence: i64| {
        n.set(n.get() + 1);
        if n.get() <= 5 {
            Err(busy())
        } else {
            w.heartbeat(id, fence)
        }
    };
    let rn = runner(&r, &s).with_beat(&hook);
    let settled = rn
        .run_attempt(
            &mut w,
            &env,
            &profile(&r, &env, &policy()),
            &CancelFlag::default(),
        )
        .unwrap();
    assert_eq!(
        settled.end,
        WorkerEnd::Completed,
        "a missed beat is not a verdict on the worker"
    );
    assert!(n.get() > 5);
    // an I/O error and a missing row are transient from the worker's point of view too
    let (mut w2, one2, _) = r.outcome();
    let env2 = r
        .allocator()
        .allocate(&mut w2, &r.spec(&one2, 1), OffsetDateTime::now_utc())
        .unwrap();
    let k = std::cell::Cell::new(0u32);
    let hook2 = |w: &mut OutcomeWriter, id: &AttemptId, fence: i64| {
        k.set(k.get() + 1);
        match k.get() {
            1 => Err(synthesis_session::Error::NotFound("attempt".into())),
            2 => Err(synthesis_session::Error::Io(io::Error::other("disk full"))),
            _ => w.heartbeat(id, fence),
        }
    };
    let rn = runner(&r, &s).with_beat(&hook2);
    let settled = rn
        .run_attempt(
            &mut w2,
            &env2,
            &profile(&r, &env2, &policy()),
            &CancelFlag::default(),
        )
        .unwrap();
    assert_eq!(settled.end, WorkerEnd::Completed);
}

#[test]
fn only_a_stale_heartbeat_kills_the_worker_and_a_dead_database_gives_the_attempt_up() {
    need_git!();
    let r = rig();
    let (mut w, one, two) = r.outcome();
    // Stale: the worker is superseded; its whole tree goes, the error says why
    let env = r
        .allocator()
        .allocate(&mut w, &r.spec(&one, 1), OffsetDateTime::now_utc())
        .unwrap();
    let s = Script("echo $$ > w.pid; sleep 300 & echo $! > sub.pid; wait".into());
    let hook = |_: &mut OutcomeWriter, _: &AttemptId, _: i64| {
        Err(synthesis_session::Error::Stale("superseded".into()))
    };
    let rn = runner(&r, &s).with_beat(&hook);
    let began = Instant::now();
    let err = rn
        .run_attempt(
            &mut w,
            &env,
            &profile(&r, &env, &policy()),
            &CancelFlag::default(),
        )
        .unwrap_err();
    assert!(
        matches!(err, RunError::Store(synthesis_session::Error::Stale(_))),
        "{err}"
    );
    assert!(began.elapsed() < Duration::from_secs(10));
    assert!(
        gone(pid_of(&env, "w.pid")) && gone(pid_of(&env, "sub.pid")),
        "no worker outlives a failed runner"
    );
    // a database that keeps failing: after six misses the worker is stopped and the attempt is
    // marked lost with the reason, not retried blindly
    let env2 = r
        .allocator()
        .allocate(&mut w, &r.spec(&two, 1), OffsetDateTime::now_utc())
        .unwrap();
    let hook = |_: &mut OutcomeWriter, _: &AttemptId, _: i64| Err(busy());
    let rn = runner(&r, &s).with_beat(&hook);
    let err = rn
        .run_attempt(
            &mut w,
            &env2,
            &profile(&r, &env2, &policy()),
            &CancelFlag::default(),
        )
        .unwrap_err();
    assert!(
        err.to_string().contains("heartbeat failed 6 times"),
        "{err}"
    );
    assert!(gone(pid_of(&env2, "w.pid")) && gone(pid_of(&env2, "sub.pid")));
    let a = attempt(&w, &two, 1);
    assert_eq!(a.state, AttemptState::Lost);
    assert!(a.retained_reason.unwrap().contains("heartbeat failed"));
}

struct Broken;
impl Spawn for Broken {
    fn spawn(&self, _: &Profile) -> io::Result<Child> {
        Err(io::Error::other("no such program"))
    }
}

#[test]
fn a_worker_that_cannot_start_fails_the_attempt_instead_of_leaving_it_running() {
    need_git!();
    let r = rig();
    let (mut w, one, _) = r.outcome();
    let env = r
        .allocator()
        .allocate(&mut w, &r.spec(&one, 1), OffsetDateTime::now_utc())
        .unwrap();
    let rn = Runner::new(
        &r.store,
        &r.wm,
        &Broken,
        &OsProbe,
        Duration::from_millis(40),
        Duration::from_millis(100),
    );
    let err = rn
        .run_attempt(
            &mut w,
            &env,
            &profile(&r, &env, &policy()),
            &CancelFlag::default(),
        )
        .unwrap_err();
    assert!(matches!(err, RunError::Io(_)), "{err}");
    let a = attempt(&w, &one, 1);
    assert_eq!(
        (a.state, a.task_status.as_deref()),
        (AttemptState::Finished, Some("failed"))
    );
    assert_eq!(item_state(&w, &one), WorkItemState::RetryableFailure);
}

#[test]
fn kill_verified_refuses_what_it_cannot_prove_is_the_worker() {
    use synthesis_orchestrator::kill_verified;
    let probe = OsProbe;
    // reserved pids and ourselves
    assert!(kill_verified(&probe, 0, Some("x")).is_err());
    assert!(kill_verified(&probe, 1, Some("x")).is_err());
    assert!(
        kill_verified(
            &probe,
            std::process::id(),
            probe.start_time(std::process::id()).as_deref()
        )
        .is_err()
    );
    // a live process whose start token is not the recorded one (pid reuse), or none recorded
    let mut other = Command::new("sleep").arg("60").spawn().unwrap();
    assert!(kill_verified(&probe, other.id(), Some("not the token")).is_err());
    assert!(kill_verified(&probe, other.id(), None).is_err());
    assert!(alive(other.id()), "refused means untouched");
    other.kill().unwrap();
    other.wait().unwrap();
}

#[test]
fn a_running_attempt_with_no_recorded_worker_is_left_alone_while_its_session_is_open() {
    need_git!();
    let r = rig();
    let (mut w, one, _) = r.outcome();
    // the owner died between spawn and record_worker: Running, no pid
    let env = r
        .allocator()
        .allocate(&mut w, &r.spec(&one, 1), OffsetDateTime::now_utc())
        .unwrap();
    r.wm.mark_started(&env.git).unwrap();
    let rev = attempt(&w, &one, 1).revision;
    w.transition_attempt(
        &env.attempt.id,
        rev,
        AttemptState::Running,
        &AttemptEnd::default(),
        &Effects::default(),
    )
    .unwrap();
    // the worker, still running, holds its session's lock
    let held = r.store.open_attempt_session(&w, &env.session_id).unwrap();
    let rep = recover(
        &r,
        &mut w,
        OffsetDateTime::now_utc() + time::Duration::hours(2),
        Duration::from_secs(1),
        3,
    );
    assert_eq!(
        (rep.unknown.len(), rep.lost.len(), rep.expired.len()),
        (1, 0, 0),
        "{rep:?}"
    );
    assert_eq!(attempt(&w, &one, 1).state, AttemptState::Running);
    drop(held);
    // the lock is free now: nobody is writing, the attempt is lost
    let rep = recover(
        &r,
        &mut w,
        OffsetDateTime::now_utc(),
        Duration::from_secs(120),
        3,
    );
    assert_eq!(rep.lost.len(), 1);
}

#[test]
fn an_unreadable_or_future_heartbeat_counts_as_fresh_never_as_silence() {
    need_git!();
    use std::os::unix::process::CommandExt;
    for bad in ["garbage", "2999-01-01T00:00:00.000Z", ""] {
        let r = rig();
        let (mut w, one, _) = r.outcome();
        let mut live = Command::new("sleep")
            .arg("60")
            .process_group(0)
            .spawn()
            .unwrap();
        let start = OsProbe.start_time(live.id());
        let env = running_with_worker(&r, &mut w, &one, 1, live.id(), start.as_deref());
        rusqlite::Connection::open(r.home.join("synt.db"))
            .unwrap()
            .execute(
                "UPDATE attempts SET heartbeat_at=?1 WHERE id=?2",
                rusqlite::params![bad, env.attempt.id.as_str()],
            )
            .unwrap();
        let rep = recover(
            &r,
            &mut w,
            OffsetDateTime::now_utc(),
            Duration::from_secs(1),
            3,
        );
        assert_eq!(
            (rep.alive.len(), rep.expired.len(), rep.lost.len()),
            (1, 0, 0),
            "{bad:?}: {rep:?}"
        );
        assert!(!rep.notes.is_empty(), "{bad:?}: it is logged");
        assert!(alive(live.id()), "{bad:?}: a live worker was killed");
        live.kill().unwrap();
        live.wait().unwrap();
    }
}

#[test]
fn later_attempts_get_what_is_left_of_the_outcome_cap() {
    need_git!();
    let r = rig();
    let (mut w, one, _) = r.outcome();
    let env = r
        .allocator()
        .allocate(&mut w, &r.spec(&one, 1), OffsetDateTime::now_utc())
        .unwrap();
    let c = r.contract_for_profile(); // outcome cap 5.0
    let build = |spent| {
        Profile::build(
            PathBuf::from("synt"),
            &env,
            &c,
            &policy(),
            spent,
            "g".into(),
        )
    };
    assert_eq!(build(0.0).unwrap().max_cost_usd(), 5.0);
    assert_eq!(build(3.5).unwrap().max_cost_usd(), 1.5);
    assert!(matches!(
        build(5.0),
        Err(ProfileError::BudgetExhausted { .. })
    ));
    assert!(matches!(
        build(7.0),
        Err(ProfileError::BudgetExhausted { .. })
    ));
    // a hostile value cannot raise the cap: a spend that is not a number is a used-up budget
    assert!(matches!(
        build(f64::NAN),
        Err(ProfileError::BudgetExhausted { .. })
    ));
    assert!(matches!(
        build(f64::INFINITY),
        Err(ProfileError::BudgetExhausted { .. })
    ));
    assert_eq!(build(-100.0).unwrap().max_cost_usd(), 5.0);
    // the settings' ceiling bounds it as well
    let mut p = policy();
    p.max_cost_usd = Some(4.0);
    assert_eq!(
        Profile::build(PathBuf::from("synt"), &env, &c, &p, 1.0, "g".into())
            .unwrap()
            .max_cost_usd(),
        3.0
    );
    // the spend is read from what the attempts' sessions recorded
    let id = w.id().clone();
    assert_eq!(w.outcome_spend(&id).unwrap(), 0.0);
    rusqlite::Connection::open(r.home.join("synt.db"))
        .unwrap()
        .execute(
            "UPDATE sessions SET usage='{\"cost_usd\":2.25}' WHERE id=?1",
            [&env.session_id],
        )
        .unwrap();
    assert_eq!(w.outcome_spend(&id).unwrap(), 2.25);
    let left = Profile::build(
        PathBuf::from("synt"),
        &env,
        &c,
        &policy(),
        w.outcome_spend(&id).unwrap(),
        "g".into(),
    )
    .unwrap();
    assert_eq!(left.max_cost_usd(), 2.75);
}

#[test]
fn a_prompt_that_starts_with_a_dash_is_a_value() {
    need_git!();
    let r = rig();
    let (mut w, one, _) = r.outcome();
    let env = r
        .allocator()
        .allocate(&mut w, &r.spec(&one, 1), OffsetDateTime::now_utc())
        .unwrap();
    let p = Profile::build(
        PathBuf::from("synt"),
        &env,
        &r.contract_for_profile(),
        &policy(),
        0.0,
        "--sandbox off".into(),
    )
    .unwrap();
    let argv: Vec<String> = p
        .argv()
        .iter()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    assert!(
        argv.contains(&"--prompt=--sandbox off".to_string()),
        "{argv:?}"
    );
    assert!(!argv.iter().any(|a| a == "--sandbox"));
}

fn usage_of(r: &Rig, env: &AttemptEnv, status: &str, usage: &str) {
    rusqlite::Connection::open(r.home.join("synt.db"))
        .unwrap()
        .execute(
            "UPDATE sessions SET status=?2, usage=?3 WHERE id=?1",
            [&env.session_id, status, usage],
        )
        .unwrap();
}

#[test]
fn a_killed_attempt_keeps_its_reservation_and_a_final_usage_lowers_it_but_never_below_what_is_recorded()
 {
    need_git!();
    let r = rig();
    let (mut w, one, _) = r.outcome();
    let env = r
        .allocator()
        .allocate(&mut w, &r.spec(&one, 1), OffsetDateTime::now_utc())
        .unwrap();
    let id = w.id().clone();
    assert_eq!(w.outcome_spend(&id).unwrap(), 0.0, "nothing started yet");
    let s = Script("sleep 30".into());
    let rn = runner(&r, &s);
    let mut run = rn
        .start(&mut w, &env, &profile(&r, &env, &policy()))
        .unwrap();
    assert_eq!(attempt(&w, &one, 1).reserved_usd, Some(5.0));
    assert_eq!(
        w.outcome_spend(&id).unwrap(),
        5.0,
        "a running worker counts its cap"
    );
    // killed: no usage was ever recorded, the reservation stays
    rn.abort(&mut w, &mut run, "test");
    assert_eq!(attempt(&w, &one, 1).state, AttemptState::Lost);
    assert_eq!(
        w.outcome_spend(&id).unwrap(),
        5.0,
        "a killed attempt is not free"
    );
    // the default (all zero) usage of a session that did not run is not a final usage
    usage_of(
        &r,
        &env,
        "failed",
        r#"{"turns":0,"tool_calls":0,"cost_usd":0.0,"wall_ms":0,"verify_attempts":0}"#,
    );
    assert_eq!(w.outcome_spend(&id).unwrap(), 5.0);
    // a final usage replaces the reservation
    usage_of(
        &r,
        &env,
        "failed",
        r#"{"turns":3,"tool_calls":2,"cost_usd":0.4,"wall_ms":900,"verify_attempts":0}"#,
    );
    assert_eq!(w.outcome_spend(&id).unwrap(), 0.4);
    // usage written while the session is not over never lowers it, and never goes below itself
    usage_of(
        &r,
        &env,
        "running",
        r#"{"turns":3,"tool_calls":2,"cost_usd":0.4,"wall_ms":900,"verify_attempts":0}"#,
    );
    assert_eq!(w.outcome_spend(&id).unwrap(), 5.0);
    usage_of(
        &r,
        &env,
        "running",
        r#"{"turns":9,"tool_calls":2,"cost_usd":7.5,"wall_ms":900,"verify_attempts":0}"#,
    );
    assert_eq!(w.outcome_spend(&id).unwrap(), 7.5);
    // the reservation is set once, can only be lowered, and only by the attempt's own fence
    let db = rusqlite::Connection::open(r.home.join("synt.db")).unwrap();
    assert!(
        db.execute(
            "UPDATE attempts SET reserved_usd=99 WHERE id=?1",
            [env.attempt.id.as_str()]
        )
        .is_err()
    );
    assert!(w.reserve_budget(&env.attempt.id, 99, 1.0).is_err());
    assert!(w.reserve_budget(&env.attempt.id, 1, f64::NAN).is_err());
    assert!(w.reserve_budget(&env.attempt.id, 1, -1.0).is_err());
}

#[test]
fn a_worker_that_could_not_be_spawned_gives_its_reservation_back() {
    need_git!();
    let r = rig();
    let (mut w, one, _) = r.outcome();
    let env = r
        .allocator()
        .allocate(&mut w, &r.spec(&one, 1), OffsetDateTime::now_utc())
        .unwrap();
    let rn = Runner::new(
        &r.store,
        &r.wm,
        &Broken,
        &OsProbe,
        Duration::from_millis(40),
        Duration::from_millis(100),
    );
    assert!(
        rn.start(&mut w, &env, &profile(&r, &env, &policy()))
            .is_err()
    );
    let id = w.id().clone();
    assert_eq!(w.outcome_spend(&id).unwrap(), 0.0, "nothing ever ran");
}

#[test]
fn a_dropped_handle_kills_and_reaps_its_whole_tree() {
    need_git!();
    let r = rig();
    let (mut w, one, _) = r.outcome();
    let env = r
        .allocator()
        .allocate(&mut w, &r.spec(&one, 1), OffsetDateTime::now_utc())
        .unwrap();
    let pidfile = r.base.join("child.pid");
    let s = Script(format!("sleep 60 & echo $! > {}; wait", pidfile.display()));
    let rn = runner(&r, &s);
    let run = rn
        .start(&mut w, &env, &profile(&r, &env, &policy()))
        .unwrap();
    let pid = run.pid();
    let until = Instant::now() + Duration::from_secs(5);
    while !pidfile.exists() && Instant::now() < until {
        std::thread::sleep(Duration::from_millis(20));
    }
    let child: u32 = fs::read_to_string(&pidfile)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    assert!(alive(pid) && alive(child));
    drop(run);
    assert!(gone(pid), "the worker is reaped");
    assert!(gone(child), "and so is what it started");
}

#[test]
fn a_settle_error_marks_the_attempt_lost_instead_of_leaving_the_slot_stuck() {
    need_git!();
    let r = rig();
    let (mut w, one, _) = r.outcome();
    let env = r
        .allocator()
        .allocate(&mut w, &r.spec(&one, 1), OffsetDateTime::now_utc())
        .unwrap();
    let s = Script("sleep 1".into());
    let rn = runner(&r, &s);
    let mut run = rn
        .start(&mut w, &env, &profile(&r, &env, &policy()))
        .unwrap();
    // the worktree is damaged: committing the attempt's tree fails
    let _ = fs::remove_file(env.context.cwd.join(".git"));
    let cancel = CancelFlag::default();
    let err = loop {
        match rn.poll(&mut w, &mut run, &cancel) {
            Ok(None) => std::thread::sleep(Duration::from_millis(30)),
            Ok(Some(s)) => panic!("settled: {s:?}"),
            Err(e) => break e,
        }
    };
    assert!(!err.to_string().is_empty());
    let a = attempt(&w, &one, 1);
    assert_eq!(
        a.state,
        AttemptState::Lost,
        "no live attempt holds the slot"
    );
    assert!(a.retained_reason.unwrap_or_default().contains("lost"));
}
