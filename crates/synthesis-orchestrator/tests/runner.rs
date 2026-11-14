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
    Runner {
        store: &r.store,
        worktrees: &r.wm,
        spawn: s,
        probe: &OsProbe,
        poll: Duration::from_millis(40),
        grace: Duration::from_millis(400),
    }
}

fn profile(r: &Rig, env: &AttemptEnv, p: &OutcomePolicy) -> Profile {
    let c = r.contract_for_profile();
    Profile::build(PathBuf::from("sh"), env, &c, p, "do the work".into()).unwrap()
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
    let prof = Profile::build(PathBuf::from("synt"), &env, &c, &p, "goal".into()).unwrap();
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
        "g".into(),
    )
    .unwrap();
    assert_eq!(plain.max_cost_usd(), 5.0);
    assert_eq!(plain.max_wall_secs(), DEFAULT_WALL_SECS);
    // a contract that does not allow writing gets the default mode, never more
    let mut ro = r.contract_for_profile();
    ro.envelope.capabilities.remove(&Capability::RepoWrite);
    assert_eq!(
        Profile::build(PathBuf::from("synt"), &env, &ro, &policy(), "g".into())
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
