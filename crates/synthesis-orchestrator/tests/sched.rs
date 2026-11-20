//! The scheduler against real git worktrees, a real v5 store and scripted workers (unix).
#![cfg(unix)]

#[macro_use]
mod common;
use common::*;

use std::cell::{Cell, RefCell};
use std::collections::BTreeSet;
use std::fs;
use std::io;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

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

/// The scripted workers record no usage, so each attempt keeps its whole reservation: keep it small.
fn small_attempts(r: &Rig) -> Contract {
    let mut c = r.contract_for_profile();
    c.envelope.budgets.attempt_max_cost_usd = Some(0.01);
    c
}

fn host<'a>(r: &'a Rig, s: &'a dyn Spawn) -> ProcessHost<'a> {
    ProcessHost {
        allocator: r.allocator(),
        runner: Runner::new(
            &r.store,
            &r.wm,
            s,
            &OsProbe,
            Duration::from_millis(30),
            Duration::from_millis(300),
        ),
        contract: small_attempts(r),
        policy: policy(),
        program: PathBuf::from("sh"),
        extra: vec![],
        source: r.repo.clone(),
        base_commit: r.head.clone(),
        worker_kind: "local-inproc",
    }
}

/// What PR-09's independent runner will do: record independent evidence for the attempt's commit
/// (or, for the verify item, for the candidate commit), then pass. `fail_once` keys fail once.
struct EvidenceVerifier {
    fail_once: RefCell<BTreeSet<String>>,
    fail_always: BTreeSet<String>,
    verified: RefCell<Vec<String>>,
}

impl EvidenceVerifier {
    fn new(fail_once: &[&str]) -> Self {
        Self {
            fail_once: RefCell::new(fail_once.iter().map(|s| (*s).to_string()).collect()),
            fail_always: BTreeSet::new(),
            verified: RefCell::new(vec![]),
        }
    }
}

impl Verifier for EvidenceVerifier {
    fn verify(
        &mut self,
        w: &mut OutcomeWriter,
        item: &WorkItemRecord,
    ) -> Result<Verdict, SchedError> {
        let ev = |item: &WorkItemRecord,
                  attempt: Option<&AttemptRecord>,
                  commit: &str,
                  crit: &[&str],
                  check: String| NewEvidence {
            work_item_id: item.id.clone(),
            attempt_id: attempt.map(|a| a.id.clone()),
            trust_class: TrustClass::IndependentCheck,
            producer: "independent_runner".into(),
            subject_commit: commit.into(),
            check_id: check,
            command_hash: None,
            criteria_ids: crit.iter().map(|s| (*s).into()).collect(),
            result: "passed".into(),
            exit_code: Some(0),
            env_fingerprint: "fp".into(),
            artifact: None,
            fence: attempt.map_or(0, |a| a.fence),
            flags: vec![],
        };
        let attempts = w.list_attempts(&item.id)?;
        if let Some(last) = attempts.last() {
            if last.state != AttemptState::Finished {
                return Ok(Verdict::Pending);
            }
            if self.fail_once.borrow_mut().remove(&item.key) || self.fail_always.contains(&item.key)
            {
                return Ok(Verdict::Failed("planted failure".into()));
            }
            let commit = last.subject_commit.clone().unwrap();
            w.record_evidence(&ev(
                item,
                Some(last),
                &commit,
                &[],
                format!("unit-{}", last.ordinal),
            ))?;
        } else {
            // the verify item: the commit of the attempt that ended last
            let mut best: Option<AttemptRecord> = None;
            for i in w.list_work_items(w.id())? {
                for a in w.list_attempts(&i.id)? {
                    if a.state == AttemptState::Finished
                        && best.as_ref().is_none_or(|b| a.ended_at > b.ended_at)
                    {
                        best = Some(a);
                    }
                }
            }
            let commit = best.and_then(|a| a.subject_commit).unwrap();
            w.record_evidence(&ev(item, None, &commit, &["c1"], "final".into()))?;
        }
        self.verified.borrow_mut().push(item.key.clone());
        Ok(Verdict::Passed)
    }
}

fn clock() -> OffsetDateTime {
    OffsetDateTime::now_utc()
}

fn states(w: &OutcomeWriter) -> Vec<(String, WorkItemState)> {
    let mut v: Vec<_> = w
        .list_work_items(w.id())
        .unwrap()
        .into_iter()
        .map(|i| (i.key, i.state))
        .collect();
    v.sort_by(|a, b| a.0.cmp(&b.0));
    v
}

fn cfg() -> SchedConfig {
    let mut c = SchedConfig::new(2, 3);
    c.backoff_base = Duration::from_secs(0);
    c
}

#[test]
fn the_dag_runs_in_order_two_at_a_time_and_the_outcome_verifies() {
    need_git!();
    let r = rig();
    let (mut w, _) = r.fresh();
    let log = r.base.join("order.log");
    let s = Script(format!(
        // each worker waits (bounded, 30s) until both have started: no wall-clock overlap window
        "echo start >> {l}; i=0; while [ $(grep -c start {l}) -lt 2 ] && [ $i -lt 300 ]; do sleep 0.1; i=$((i+1)); done; echo end >> {l}; echo made > out.txt",
        l = log.display()
    ));
    let mut sched = Scheduler::new(host(&r, &s), EvidenceVerifier::new(&[]), cfg());
    let (stop, reports) = sched
        .run(&mut w, &clock, Duration::from_millis(30))
        .unwrap();
    assert_eq!(stop, Stop::Succeeded, "{reports:?}");
    // `one` and `two` had no dependencies and ran together (cap 2); `test` waited for both
    let order: Vec<String> = fs::read_to_string(&log)
        .unwrap()
        .lines()
        .map(String::from)
        .collect();
    assert_eq!(
        order,
        ["start", "start", "end", "end"],
        "two workers overlapped"
    );
    let leased: Vec<_> = reports.iter().flat_map(|r| r.leased.clone()).collect();
    assert_eq!(
        leased,
        ["one", "two", "test"],
        "sorted, once each, the verify item last"
    );
    assert!(
        states(&w)
            .iter()
            .all(|(_, s)| *s == WorkItemState::Succeeded),
        "{:?}",
        states(&w)
    );
    // the outcome's own verdict, computed in the store from the evidence rows, agrees
    for to in [OutcomeState::Verifying, OutcomeState::Verified] {
        let rev = w.get_outcome(w.id()).unwrap().revision;
        w.transition_outcome(rev, to, None, &Effects::default())
            .unwrap();
    }
    // no duplicate assignment: one attempt per agent item
    for (_, id) in w
        .list_work_items(w.id())
        .unwrap()
        .iter()
        .filter(|i| i.kind == WorkItemKind::Agent)
        .map(|i| (i.key.clone(), i.id.clone()))
    {
        assert_eq!(w.list_attempts(&id).unwrap().len(), 1);
    }
}

#[test]
fn never_more_workers_than_the_cap_at_any_instant() {
    need_git!();
    for cap in [1u32, 2] {
        let r = rig();
        let (mut w, _) = r.fresh();
        let dir = r.base.join(format!("conc{cap}"));
        fs::create_dir_all(&dir).unwrap();
        // each worker registers itself, counts the live ones, records the maximum
        let s = Script(format!(
            "d={d}; touch $d/live.$$; n=$(ls $d | grep -c '^live\\.'); echo $n >> $d/seen; i=0; while [ $(ls $d | grep -c '^live\\.') -lt {cap} ] && [ $i -lt 300 ]; do sleep 0.1; i=$((i+1)); done; sleep 0.5; rm $d/live.$$",
            d = dir.display()
        ));
        let mut c = cfg();
        c.max_parallel = cap;
        let mut sched = Scheduler::new(host(&r, &s), EvidenceVerifier::new(&[]), c);
        let (stop, _) = sched
            .run(&mut w, &clock, Duration::from_millis(20))
            .unwrap();
        assert_eq!(stop, Stop::Succeeded);
        let max = fs::read_to_string(dir.join("seen"))
            .unwrap()
            .lines()
            .map(|l| l.trim().parse::<u32>().unwrap())
            .max()
            .unwrap();
        assert!(max <= cap, "cap {cap}: saw {max} workers at once");
        if cap == 2 {
            assert_eq!(max, 2, "the cap is used");
        }
    }
}

#[test]
fn a_failed_attempt_retries_after_its_backoff_and_a_verification_failure_retries_too() {
    need_git!();
    let r = rig();
    let (mut w, _) = r.fresh();
    // the first attempt of every item fails, the second works (state is kept outside the worktree)
    let marks = r.base.join("marks");
    fs::create_dir_all(&marks).unwrap();
    let s = Script(format!(
        "if [ -f {m}/first ]; then exit 0; else touch {m}/first; exit 1; fi",
        m = marks.display()
    ));
    let mut c = cfg();
    c.max_parallel = 1;
    c.backoff_base = Duration::from_secs(60);
    let mut sched = Scheduler::new(host(&r, &s), EvidenceVerifier::new(&["two"]), c);
    let t0 = OffsetDateTime::now_utc();
    // tick 1: `one` is leased and (soon) fails
    let rep = sched.tick(&mut w, t0).unwrap();
    assert_eq!(rep.leased, ["one"]);
    while sched.running_count() > 0 {
        std::thread::sleep(Duration::from_millis(50));
        sched.tick(&mut w, t0).unwrap();
    }
    let st = states(&w);
    assert_eq!(
        st.iter().find(|(k, _)| k == "one").unwrap().1,
        WorkItemState::RetryableFailure,
        "{st:?}"
    );
    // the retry waits for its backoff: nothing is leased at the same instant
    let rep = sched.tick(&mut w, t0).unwrap();
    assert!(
        rep.leased.is_empty() && rep.stop == Some(Stop::Backoff),
        "{rep:?}"
    );
    // 61 seconds later it is ready again and runs as attempt 2 (a new worktree and session)
    let t1 = t0 + time::Duration::seconds(61);
    let rep = sched.tick(&mut w, t1).unwrap();
    assert_eq!(rep.leased, ["one"]);
    let one = w
        .list_work_items(w.id())
        .unwrap()
        .into_iter()
        .find(|i| i.key == "one")
        .unwrap();
    let a = w.list_attempts(&one.id).unwrap();
    assert_eq!(a.iter().map(|x| x.ordinal).collect::<Vec<_>>(), [1, 2]);
    assert_ne!(a[0].session_id, a[1].session_id);
    assert_eq!(one.retries_used, 1);
    // finish the whole walk; `two`'s first verification fails and it retries within budget
    let mut now = t1;
    let (mut stop, mut n) = (None, 0);
    while stop.is_none() && n < 400 {
        n += 1;
        now += time::Duration::seconds(30);
        std::thread::sleep(Duration::from_millis(40));
        stop = sched
            .tick(&mut w, now)
            .unwrap()
            .stop
            .filter(|s| !matches!(s, Stop::Backoff));
    }
    assert_eq!(stop, Some(Stop::Succeeded), "{:?}", states(&w));
    let two = w
        .list_work_items(w.id())
        .unwrap()
        .into_iter()
        .find(|i| i.key == "two")
        .unwrap();
    assert!(
        two.retries_used >= 1,
        "the failed verification consumed a retry"
    );
}

#[test]
fn a_verification_that_keeps_failing_ends_the_item_and_blocks_the_outcome() {
    need_git!();
    let r = rig();
    let (mut w, _) = r.fresh();
    let s = Script("exit 0".into());
    // `one` fails verification on every attempt
    let mut v = EvidenceVerifier::new(&[]);
    v.fail_always.insert("one".into());
    let mut sched = Scheduler::new(host(&r, &s), v, cfg());
    let (stop, _) = sched
        .run(&mut w, &clock, Duration::from_millis(30))
        .unwrap();
    let Stop::Blocked(why) = stop else {
        panic!("{stop:?}")
    };
    assert!(why.contains("one (terminal_failure)"), "{why}");
    let one = w
        .list_work_items(w.id())
        .unwrap()
        .into_iter()
        .find(|i| i.key == "one")
        .unwrap();
    assert_eq!(
        w.list_attempts(&one.id).unwrap().len(),
        3,
        "exactly the budget, no fourth attempt"
    );
    assert_eq!(w.get_outcome(w.id()).unwrap().state, OutcomeState::Blocked);
}

#[test]
fn the_unverified_handoff_waits_and_never_marks_anything_succeeded() {
    need_git!();
    let r = rig();
    let (mut w, _) = r.fresh();
    let s = Script("exit 0".into());
    let mut sched = Scheduler::new(host(&r, &s), HandOff, cfg());
    let (stop, _) = sched
        .run(&mut w, &clock, Duration::from_millis(30))
        .unwrap();
    assert_eq!(stop, Stop::WaitingVerification);
    let st = states(&w);
    assert_eq!(
        st.iter()
            .filter(|(_, s)| *s == WorkItemState::Verifying)
            .count(),
        2,
        "{st:?}"
    );
    assert!(st.iter().all(|(_, s)| *s != WorkItemState::Succeeded));
}

#[test]
fn pause_stops_new_leases_and_cancel_ends_the_outcome() {
    need_git!();
    let r = rig();
    let (mut w, _) = r.fresh();
    let s = Script("sleep 300 & echo $! > sub.pid; wait".into());
    let mut sched = Scheduler::new(host(&r, &s), EvidenceVerifier::new(&[]), cfg());
    sched.control.pause();
    let rep = sched.tick(&mut w, clock()).unwrap();
    assert!(
        rep.leased.is_empty() && rep.stop == Some(Stop::Paused),
        "{rep:?}"
    );
    sched.control.resume();
    let rep = sched.tick(&mut w, clock()).unwrap();
    assert_eq!(rep.leased, ["one", "two"]);
    // a pause now keeps the two running and leases nothing more
    sched.control.pause();
    assert_eq!(sched.running_count(), 2);
    sched.control.resume();
    // cancel the whole outcome: the workers' trees die, every open item is cancelled
    sched.control.cancel.cancel();
    let began = std::time::Instant::now();
    let mut stop = None;
    while stop.is_none() && began.elapsed() < Duration::from_secs(15) {
        std::thread::sleep(Duration::from_millis(50));
        stop = sched.tick(&mut w, clock()).unwrap().stop;
    }
    assert_eq!(stop, Some(Stop::Cancelled));
    assert_eq!(
        w.get_outcome(w.id()).unwrap().state,
        OutcomeState::Cancelled
    );
    assert!(
        states(&w)
            .iter()
            .all(|(_, s)| *s == WorkItemState::Cancelled),
        "{:?}",
        states(&w)
    );
    for it in w
        .list_work_items(w.id())
        .unwrap()
        .iter()
        .filter(|i| i.kind == WorkItemKind::Agent)
    {
        let a = w.list_attempts(&it.id).unwrap();
        assert_eq!(a[0].state, AttemptState::Cancelled);
    }
}

#[test]
fn a_restart_frees_orphaned_leases_and_continues_without_duplicate_attempts() {
    need_git!();
    let r = rig();
    let (mut w, items) = r.fresh();
    let id = w.id().clone();
    let it = |k: &str| items.iter().find(|(n, _)| n == k).unwrap().1.clone();
    let go = |w: &mut OutcomeWriter, i: &WorkItemId, to| {
        let rev = w
            .list_work_items(w.id())
            .unwrap()
            .into_iter()
            .find(|x| x.id == *i)
            .unwrap()
            .revision;
        w.transition_work_item(i, rev, to, &Effects::default())
            .unwrap();
    };
    // the owner died right after leasing `one` (no attempt yet), and `two` has a worker that died
    for to in [WorkItemState::Ready, WorkItemState::Leased] {
        go(&mut w, &it("one"), to);
    }
    for to in [WorkItemState::Ready, WorkItemState::Leased] {
        go(&mut w, &it("two"), to);
    }
    let env = r
        .allocator()
        .allocate(&mut w, &r.spec(&it("two"), 1), OffsetDateTime::now_utc())
        .unwrap();
    r.wm.mark_started(env.git.as_ref().unwrap()).unwrap();
    let rev = w.list_attempts(&it("two")).unwrap()[0].revision;
    w.transition_attempt(
        &env.attempt.id,
        rev,
        AttemptState::Running,
        &AttemptEnd::default(),
        &Effects::default(),
    )
    .unwrap();
    go(&mut w, &it("two"), WorkItemState::Running);
    let mut gone = Command::new("true").spawn().unwrap();
    let pid = gone.id();
    gone.wait().unwrap();
    w.record_worker(&env.attempt.id, 1, pid, Some("tok"))
        .unwrap();
    drop(w);
    // restart: recover the attempts (PR-05), then walk
    let mut w = r.store.open_outcome(&id, &policy()).unwrap();
    let rep = recover_attempts(
        &r.store,
        &r.wm,
        &OsProbe,
        &mut w,
        clock(),
        Duration::from_secs(120),
        3,
        Platform {
            native_windows: false,
        },
    )
    .unwrap();
    assert_eq!(rep.lost.len(), 1);
    let s = Script("exit 0".into());
    let mut sched = Scheduler::new(host(&r, &s), EvidenceVerifier::new(&[]), cfg());
    let (stop, _) = sched
        .run(&mut w, &clock, Duration::from_millis(30))
        .unwrap();
    assert_eq!(stop, Stop::Succeeded, "{:?}", states(&w));
    for key in ["one", "two"] {
        let a = w.list_attempts(&it(key)).unwrap();
        let ords: Vec<u32> = a.iter().map(|x| x.ordinal).collect();
        let want: Vec<u32> = if key == "one" { vec![1] } else { vec![1, 2] };
        assert_eq!(ords, want, "{key}: one attempt per ordinal, no duplicates");
        let sessions: BTreeSet<_> = a.iter().map(|x| x.session_id.clone()).collect();
        assert_eq!(sessions.len(), a.len());
    }
}

#[test]
fn a_spent_outcome_budget_stops_new_workers_and_a_start_that_cannot_work_blocks_the_item() {
    need_git!();
    let r = rig();
    let (mut w, _) = r.fresh();
    let s = Script("exit 0".into());
    let mut c = cfg();
    c.max_cost_usd = Some(0.0);
    let mut sched = Scheduler::new(host(&r, &s), EvidenceVerifier::new(&[]), c);
    let rep = sched.tick(&mut w, clock()).unwrap();
    assert!(rep.leased.is_empty(), "{rep:?}");
    assert!(
        matches!(&rep.stop, Some(Stop::Blocked(m)) if m.contains("cost cap")),
        "{rep:?}"
    );
    assert_eq!(w.get_outcome(w.id()).unwrap().state, OutcomeState::Blocked);
    // a start that can never work (a folder that is not the outcome's repository): the item is
    // blocked with the reason, not retried in a loop
    let (mut w, _) = r.fresh();
    let mut h = host(&r, &s);
    h.source = r.base.clone();
    let mut sched = Scheduler::new(h, EvidenceVerifier::new(&[]), cfg());
    let rep = sched.tick(&mut w, clock()).unwrap();
    assert!(
        rep.errors
            .iter()
            .any(|e| e.contains("outcome's repository")),
        "{rep:?}"
    );
    assert!(
        states(&w)
            .iter()
            .any(|(k, s)| k == "one" && *s == WorkItemState::Blocked)
    );
}

#[test]
fn a_thousand_ready_names_are_picked_sorted_and_only_up_to_the_free_slots() {
    let names: Vec<String> = (0..1000)
        .rev()
        .map(|i| format!("w{i:04}"))
        .chain((0..1000).map(|i| format!("w{i:04}")))
        .collect();
    let picked = pick_in_order(&names, 2);
    assert_eq!(picked, ["w0000", "w0001"]);
    assert_eq!(pick_in_order(&names, 0), Vec::<String>::new());
    let all = pick_in_order(&names, 5000);
    assert_eq!(all.len(), 1000, "duplicates are one item");
    let mut sorted = all.clone();
    sorted.sort();
    assert_eq!(all, sorted);
}

/// A host that never runs anything: it counts the workers "alive" at once.
struct Counting {
    live: std::rc::Rc<std::cell::Cell<usize>>,
    max: std::rc::Rc<std::cell::Cell<usize>>,
    started: std::rc::Rc<RefCell<Vec<String>>>,
}

impl WorkerHost for Counting {
    type Handle = u32;
    fn start(
        &mut self,
        w: &mut OutcomeWriter,
        item: &WorkItemRecord,
        _: u32,
        _: f64,
        _: u32,
    ) -> Result<u32, SchedError> {
        self.live.set(self.live.get() + 1);
        self.max.set(self.max.get().max(self.live.get()));
        self.started.borrow_mut().push(item.key.clone());
        let rev = w
            .list_work_items(w.id())?
            .into_iter()
            .find(|i| i.id == item.id)
            .unwrap()
            .revision;
        w.transition_work_item(&item.id, rev, WorkItemState::Running, &Effects::default())?;
        Ok(2) // finishes after two polls
    }
    fn poll(
        &mut self,
        w: &mut OutcomeWriter,
        h: &mut u32,
        _: &CancelFlag,
    ) -> Result<Option<Settled>, SchedError> {
        if *h > 0 {
            *h -= 1;
            return Ok(None);
        }
        self.live.set(self.live.get() - 1);
        // hand every item over to verification (the keys are found through the running item)
        for it in w.list_work_items(w.id())? {
            if it.state == WorkItemState::Running && it.kind == WorkItemKind::Agent {
                let rev = it.revision;
                w.transition_work_item(&it.id, rev, WorkItemState::Verifying, &Effects::default())?;
                break;
            }
        }
        Ok(Some(Settled {
            end: WorkerEnd::Completed,
            subject_commit: None,
            item: WorkItemState::Verifying,
        }))
    }
}

fn big_outcome(r: &Rig) -> OutcomeWriter {
    // the plan limit is 256 items; one verify item covers the criterion
    let ws = synthesis_core::WorkspaceContext::open(
        "w",
        &r.repo,
        &r.repo,
        &[],
        r.base.join("cart"),
        "pg",
    )
    .unwrap();
    let mut w = r
        .store
        .create_outcome(NewOutcome {
            workspace: &ws,
            base_commit: r.head.clone(),
            contract: &r.contract_for_profile(),
            policy: &policy(),
        })
        .unwrap();
    let go = |w: &mut OutcomeWriter, to| {
        let rev = w.get_outcome(w.id()).unwrap().revision;
        w.transition_outcome(rev, to, None, &Effects::default())
            .unwrap();
    };
    go(&mut w, OutcomeState::ContractPending);
    let h = w.get_contract(w.id(), 1).unwrap().1;
    w.approve_contract(1, &h, "user").unwrap();
    let mut items: Vec<PlanItem> = (0..250)
        .map(|i| PlanItem {
            key: format!("i{i:03}"),
            kind: WorkItemKind::Agent,
            requires: vec![],
            capabilities: vec![],
            covers: vec![],
            title: String::new(),
            instructions: String::new(),
        })
        .collect();
    items.push(PlanItem {
        key: "z-check".into(),
        kind: WorkItemKind::Verify,
        requires: vec!["i000".into()],
        capabilities: vec![],
        covers: vec!["c1".into()],
        title: String::new(),
        instructions: String::new(),
    });
    w.insert_plan(&Plan {
        version: 1,
        contract_version: 1,
        items,
    })
    .unwrap();
    go(&mut w, OutcomeState::Ready);
    go(&mut w, OutcomeState::Running);
    w
}

#[test]
fn two_hundred_and_fifty_ready_items_never_run_more_than_the_cap_at_once() {
    need_git!();
    for cap in [1u32, 2, 5] {
        let r = rig();
        let mut w = big_outcome(&r);
        let (live, max, started) = (
            std::rc::Rc::new(std::cell::Cell::new(0)),
            std::rc::Rc::new(std::cell::Cell::new(0)),
            std::rc::Rc::new(RefCell::new(vec![])),
        );
        let mut c = cfg();
        c.max_parallel = cap;
        let mut sched = Scheduler::new(
            Counting {
                live: live.clone(),
                max: max.clone(),
                started: started.clone(),
            },
            HandOff,
            c,
        );
        let mut ticks = 0;
        loop {
            let rep = sched.tick(&mut w, clock()).unwrap();
            ticks += 1;
            assert!(
                sched.running_count() <= cap as usize,
                "cap {cap}: {} running",
                sched.running_count()
            );
            assert!(live.get() <= cap as usize);
            if matches!(rep.stop, Some(Stop::WaitingVerification)) || ticks > 3000 {
                break;
            }
        }
        assert!(ticks <= 3000, "cap {cap}: the walk did not finish");
        assert!(
            max.get() <= cap as usize,
            "cap {cap}: {} at once",
            max.get()
        );
        assert_eq!(max.get(), cap as usize, "the cap is used");
        let order = started.borrow().clone();
        let mut sorted = order.clone();
        sorted.sort();
        assert_eq!(order, sorted, "leased in sorted key order");
        assert_eq!(
            order.iter().collect::<BTreeSet<_>>().len(),
            order.len(),
            "no item started twice"
        );
        assert_eq!(order.len(), 250);
        // every agent item reached the hand-over, none was run twice
        assert_eq!(
            w.list_work_items(w.id())
                .unwrap()
                .iter()
                .filter(|i| i.state == WorkItemState::Verifying)
                .count(),
            250
        );
    }
}

/// Picks the command by work item (the prompt names it): `slow` sleeps, `fail` cannot be spawned,
/// every other item finishes at once.
struct ByItem {
    slow: &'static str,
    fail: Option<&'static str>,
}

impl Spawn for ByItem {
    fn spawn(&self, p: &Profile) -> io::Result<Child> {
        let named = |k: &str| {
            p.argv()
                .iter()
                .any(|a| a.to_string_lossy().ends_with(&format!("Work item: {k}")))
        };
        if self.fail.is_some_and(named) {
            return Err(io::Error::other("no such program"));
        }
        let script = if named(self.slow) {
            "sleep 30"
        } else {
            "echo made > out.txt"
        };
        Script(script.into()).spawn(p)
    }
}

fn ready(w: &mut OutcomeWriter, it: &WorkItemId) {
    let cur = w.get_work_item(it).unwrap();
    if cur.state == WorkItemState::Ready {
        return;
    }
    w.transition_work_item(it, cur.revision, WorkItemState::Ready, &Effects::default())
        .unwrap();
}

fn alive(pid: u32) -> bool {
    Command::new("kill")
        .args(["-0", &pid.to_string()])
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

fn pids(w: &OutcomeWriter) -> Vec<u32> {
    let mut v = vec![];
    for i in w.list_work_items(w.id()).unwrap() {
        v.extend(
            w.list_attempts(&i.id)
                .unwrap()
                .iter()
                .filter_map(|a| a.worker_pid),
        );
    }
    v
}

fn lost_attempts(w: &OutcomeWriter) -> usize {
    let mut n = 0;
    for i in w.list_work_items(w.id()).unwrap() {
        n += w
            .list_attempts(&i.id)
            .unwrap()
            .iter()
            .filter(|a| a.state == AttemptState::Lost)
            .count();
    }
    n
}

#[test]
fn parallel_starts_split_what_is_left_and_cannot_over_commit_the_outcome_cap() {
    need_git!();
    let r = rig();
    let (mut w, _) = r.fresh();
    let s = Script("sleep 30".into());
    let mut c = cfg();
    c.max_cost_usd = Some(5.0);
    let mut h = host(&r, &s);
    h.contract = r.contract_for_profile(); // outcome cap 5.0, no per-attempt cap
    let mut sched = Scheduler::new(h, EvidenceVerifier::new(&[]), c);
    let rep = sched.tick(&mut w, clock()).unwrap();
    assert_eq!(rep.leased, ["one", "two"], "{rep:?}");
    let id = w.id().clone();
    let mut reserved = vec![];
    for i in w.list_work_items(&id).unwrap() {
        reserved.extend(
            w.list_attempts(&i.id)
                .unwrap()
                .iter()
                .map(|a| a.reserved_usd),
        );
    }
    assert_eq!(
        reserved,
        [Some(2.5), Some(2.5)],
        "each gets its share, not the whole cap"
    );
    assert_eq!(w.outcome_spend(&id).unwrap(), 5.0);
    // the cap is reserved: nothing more starts, and that is no failure while workers run
    let rep = sched.tick(&mut w, clock()).unwrap();
    assert!(rep.leased.is_empty() && rep.stop.is_none(), "{rep:?}");
    // killed workers keep their reservations
    let pids = pids(&w);
    sched.shutdown(&mut w, "test");
    assert_eq!(w.outcome_spend(&id).unwrap(), 5.0);
    assert_eq!(lost_attempts(&w), 2);
    assert!(pids.iter().all(|p| !alive(*p)), "no worker is left");
    // a lone ready item gets the whole remainder
    let (mut w, _) = r.fresh();
    let mut h = host(&r, &s);
    h.contract = r.contract_for_profile();
    let mut c = cfg();
    c.max_parallel = 1;
    c.max_cost_usd = Some(5.0);
    let mut sched = Scheduler::new(h, EvidenceVerifier::new(&[]), c);
    sched.tick(&mut w, clock()).unwrap();
    let it = w.list_work_items(w.id()).unwrap();
    let leased = it
        .iter()
        .find(|i| i.state == WorkItemState::Running)
        .unwrap();
    assert_eq!(
        w.list_attempts(&leased.id).unwrap()[0].reserved_usd,
        Some(5.0)
    );
}

#[test]
fn a_crash_keeps_the_reservations_of_attempts_that_never_recorded_a_usage() {
    need_git!();
    let r = rig();
    let (mut w, items) = r.fresh();
    let id = w.id().clone();
    let one = items.iter().find(|(n, _)| n == "one").unwrap().1.clone();
    ready(&mut w, &one);
    let env = r
        .allocator()
        .allocate(&mut w, &r.spec(&one, 1), OffsetDateTime::now_utc())
        .unwrap();
    r.wm.mark_started(env.git.as_ref().unwrap()).unwrap();
    let rev = w.list_attempts(&one).unwrap()[0].revision;
    w.transition_attempt(
        &env.attempt.id,
        rev,
        AttemptState::Running,
        &AttemptEnd::default(),
        &Effects::default(),
    )
    .unwrap();
    w.reserve_budget(&env.attempt.id, 1, 3.0).unwrap();
    let mut gone = Command::new("true").spawn().unwrap();
    let pid = gone.id();
    gone.wait().unwrap();
    w.record_worker(&env.attempt.id, 1, pid, Some("tok"))
        .unwrap();
    drop(w);
    let mut w = r.store.open_outcome(&id, &policy()).unwrap();
    let rep = recover_attempts(
        &r.store,
        &r.wm,
        &OsProbe,
        &mut w,
        clock(),
        Duration::from_secs(120),
        3,
        Platform {
            native_windows: false,
        },
    )
    .unwrap();
    assert_eq!(rep.lost.len(), 1);
    assert_eq!(w.list_attempts(&one).unwrap()[0].state, AttemptState::Lost);
    assert_eq!(
        w.outcome_spend(&id).unwrap(),
        3.0,
        "the crashed attempt still counts"
    );
}

#[test]
fn a_spend_that_is_not_a_number_is_a_used_up_budget_even_without_a_cap() {
    need_git!();
    let r = rig();
    let (mut w, items) = r.fresh();
    let one = items.iter().find(|(n, _)| n == "one").unwrap().1.clone();
    ready(&mut w, &one);
    r.allocator()
        .allocate(&mut w, &r.spec(&one, 1), OffsetDateTime::now_utc())
        .unwrap();
    r.raw("UPDATE attempts SET reserved_usd=9e999");
    let s = Script("exit 0".into());
    let mut sched = Scheduler::new(host(&r, &s), EvidenceVerifier::new(&[]), cfg()); // no cap
    let rep = sched.tick(&mut w, clock()).unwrap();
    assert!(rep.leased.is_empty(), "{rep:?}");
    assert!(
        matches!(&rep.stop, Some(Stop::Blocked(m)) if m.contains("cost cap")),
        "{rep:?}"
    );
}

#[test]
fn a_worker_that_cannot_start_does_not_abort_the_tick_or_stop_its_sibling() {
    need_git!();
    let r = rig();
    let (mut w, _) = r.fresh();
    let s = ByItem {
        slow: "one",
        fail: Some("two"),
    };
    let mut sched = Scheduler::new(host(&r, &s), EvidenceVerifier::new(&[]), cfg());
    let rep = sched.tick(&mut w, clock()).unwrap();
    assert!(
        rep.errors
            .iter()
            .any(|e| e.starts_with("two:") && e.contains("cannot start")),
        "{rep:?}"
    );
    assert_eq!(
        sched.running_count(),
        1,
        "the sibling was started and is still held"
    );
    let st = states(&w);
    assert!(
        st.contains(&("one".into(), WorkItemState::Running)),
        "{st:?}"
    );
    assert!(
        st.contains(&("two".into(), WorkItemState::RetryableFailure)),
        "the failed start retries within its budget: {st:?}"
    );
    let sibling = pids(&w);
    // the walk goes on: `two` is retried until its attempts are used up, then the outcome is
    // blocked (the sibling is still sleeping: a deadline ends this test)
    let mut n = 0;
    while sched.running_count() > 0 && n < 40 {
        let rep = sched.tick(&mut w, clock()).unwrap();
        if matches!(rep.stop, Some(Stop::Blocked(_))) {
            break;
        }
        n += 1;
        std::thread::sleep(Duration::from_millis(20));
    }
    let st = states(&w);
    assert!(
        st.contains(&("two".into(), WorkItemState::TerminalFailure)),
        "{st:?}"
    );
    assert!(
        sibling.iter().all(|p| alive(*p)),
        "the sibling was never touched"
    );
    sched.shutdown(&mut w, "test");
    assert!(sibling.iter().all(|p| !alive(*p)));
}

#[test]
fn a_hard_error_or_the_deadline_stops_every_worker_and_marks_their_attempts_lost() {
    need_git!();
    // the outcome is cancelled behind the walk's back while a worker sleeps: the tick fails
    struct Pull;
    impl Verifier for Pull {
        fn verify(
            &mut self,
            w: &mut OutcomeWriter,
            _: &WorkItemRecord,
        ) -> Result<Verdict, SchedError> {
            let rev = w.get_outcome(w.id())?.revision;
            w.transition_outcome(
                rev,
                OutcomeState::Cancelled,
                Some("pulled"),
                &Effects::default(),
            )?;
            Ok(Verdict::Pending)
        }
    }
    let r = rig();
    let (mut w, _) = r.fresh();
    let s = ByItem {
        slow: "two",
        fail: None,
    };
    let mut sched = Scheduler::new(host(&r, &s), Pull, cfg());
    let err = sched
        .run(&mut w, &clock, Duration::from_millis(30))
        .unwrap_err();
    assert!(err.to_string().contains("not running"), "{err}");
    assert_eq!(sched.running_count(), 0);
    let p = pids(&w);
    assert_eq!(p.len(), 2);
    assert!(
        p.iter().all(|p| !alive(*p)),
        "the sleeping worker was killed and reaped"
    );
    assert!(lost_attempts(&w) >= 1);
    // the deadline
    let (mut w, _) = r.fresh();
    let s = Script("sleep 30".into());
    let mut sched = Scheduler::new(host(&r, &s), EvidenceVerifier::new(&[]), cfg());
    let t = std::time::Instant::now();
    let err = sched
        .run_until(
            &mut w,
            &clock,
            Duration::from_millis(20),
            Some(t + Duration::from_millis(300)),
        )
        .unwrap_err();
    assert!(err.to_string().contains("deadline"), "{err}");
    assert!(t.elapsed() < Duration::from_secs(10));
    let p = pids(&w);
    assert_eq!(p.len(), 2);
    assert!(p.iter().all(|p| !alive(*p)));
    assert_eq!(lost_attempts(&w), 2);
}

#[test]
fn a_dropped_scheduler_leaves_no_worker_behind() {
    need_git!();
    let r = rig();
    let (mut w, _) = r.fresh();
    let s = Script("sleep 30".into());
    let mut sched = Scheduler::new(host(&r, &s), EvidenceVerifier::new(&[]), cfg());
    sched.tick(&mut w, clock()).unwrap();
    let p = pids(&w);
    assert_eq!(p.len(), 2);
    assert!(p.iter().all(|p| alive(*p)));
    drop(sched);
    let until = std::time::Instant::now() + Duration::from_secs(5);
    while p.iter().any(|p| alive(*p)) && std::time::Instant::now() < until {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(p.iter().all(|p| !alive(*p)));
}

#[test]
fn a_verifier_that_fails_is_asked_again_and_does_not_end_the_walk() {
    need_git!();
    let r = rig();
    let (mut w, _) = r.fresh();
    struct Flaky(EvidenceVerifier, Cell<u32>);
    impl Verifier for Flaky {
        fn verify(
            &mut self,
            w: &mut OutcomeWriter,
            it: &WorkItemRecord,
        ) -> Result<Verdict, SchedError> {
            if self.1.get() < 2 {
                self.1.set(self.1.get() + 1);
                return Err(SchedError::State("verifier crashed".into()));
            }
            self.0.verify(w, it)
        }
    }
    let s = ByItem {
        slow: "none",
        fail: None,
    };
    let mut sched = Scheduler::new(
        host(&r, &s),
        Flaky(EvidenceVerifier::new(&[]), Cell::new(0)),
        cfg(),
    );
    let (stop, reports) = sched
        .run(&mut w, &clock, Duration::from_millis(30))
        .unwrap();
    assert_eq!(stop, Stop::Succeeded, "{reports:?}");
    assert!(
        reports
            .iter()
            .flat_map(|r| &r.errors)
            .any(|e| e.contains("the verifier failed")),
        "{reports:?}"
    );
}

#[test]
fn a_missing_synt_binary_ends_blocked_with_the_reason_and_no_worker() {
    need_git!();
    let r = rig();
    let (mut w, _) = r.fresh();
    let spawn = SyntSpawn {
        log: r.base.join("worker.log"),
        env: Some(vec![]),
    };
    let mut h = host(&r, &spawn);
    h.program = PathBuf::from("/no/such/dir/synt");
    let mut sched = Scheduler::new(h, EvidenceVerifier::new(&[]), cfg());
    let (stop, reports) = sched
        .run(&mut w, &clock, Duration::from_millis(10))
        .unwrap();
    assert!(
        matches!(&stop, Stop::Blocked(m) if m.contains("cannot start the worker")),
        "{stop:?}"
    );
    assert_eq!(sched.running_count(), 0);
    assert!(
        reports
            .iter()
            .flat_map(|r| &r.errors)
            .any(|e| e.contains("cannot start"))
    );
    // nothing was spent: the reservations went back
    assert_eq!(w.outcome_spend(&w.id().clone()).unwrap(), 0.0);
}

#[test]
fn an_item_left_in_recovering_by_a_crash_is_freed() {
    need_git!();
    let r = rig();
    let (mut w, items) = r.fresh();
    let one = items.iter().find(|(n, _)| n == "one").unwrap().1.clone();
    for to in [
        WorkItemState::Ready,
        WorkItemState::Leased,
        WorkItemState::Recovering,
    ] {
        let rev = w.get_work_item(&one).unwrap().revision;
        w.transition_work_item(&one, rev, to, &Effects::default())
            .unwrap();
    }
    let s = Script("exit 0".into());
    let mut sched = Scheduler::new(host(&r, &s), EvidenceVerifier::new(&[]), cfg());
    let (stop, _) = sched
        .run(&mut w, &clock, Duration::from_millis(30))
        .unwrap();
    assert_eq!(stop, Stop::Succeeded, "{:?}", states(&w));
}

#[test]
fn a_walk_that_only_reports_errors_with_nothing_running_gives_up() {
    need_git!();
    let r = rig();
    let (mut w, _) = r.fresh();
    // every start fails and nothing runs: the tick reports errors and makes no progress
    let spawn = SyntSpawn {
        log: r.base.join("worker.log"),
        env: Some(vec![]),
    };
    let mut h = host(&r, &spawn);
    h.program = PathBuf::from("/no/such/dir/synt");
    let mut c = cfg();
    c.max_stalled_ticks = 1;
    c.backoff_base = Duration::from_secs(60);
    let mut sched = Scheduler::new(h, EvidenceVerifier::new(&[]), c);
    let err = sched
        .run(&mut w, &clock, Duration::from_millis(10))
        .unwrap_err();
    assert!(err.to_string().contains("no progress"), "{err}");
    assert_eq!(sched.running_count(), 0);
}

#[test]
fn errors_every_tick_do_not_stop_a_healthy_worker() {
    need_git!();
    let r = rig();
    let (mut w, _) = r.fresh();
    struct Broken;
    impl Verifier for Broken {
        fn verify(
            &mut self,
            _: &mut OutcomeWriter,
            _: &WorkItemRecord,
        ) -> Result<Verdict, SchedError> {
            Err(SchedError::State("verifier crashed".into()))
        }
    }
    // `one` finishes at once (the verifier then fails every tick), `two` sleeps
    let s = ByItem {
        slow: "two",
        fail: None,
    };
    let mut c = cfg();
    c.max_stalled_ticks = 2;
    let mut sched = Scheduler::new(host(&r, &s), Broken, c);
    let err = sched
        .run_until(
            &mut w,
            &clock,
            Duration::from_millis(20),
            Some(std::time::Instant::now() + Duration::from_millis(700)),
        )
        .unwrap_err();
    assert!(err.to_string().contains("deadline"), "not stalled: {err}");
}

// ---- PR-09: the independent verifier on the scheduler's seam ----

use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicBool;

/// A check runner that records where it ran and answers from a table (no sandbox needed).
struct FakeRunner {
    exit: Option<i32>,
    problem: Option<String>,
    enforcing: bool,
    delay: Duration,
    output: Vec<u8>,
    /// What the same check prints at the base commit (its checkout dir is named `base`).
    base_output: Option<Vec<u8>>,
    ran_in: Mutex<Vec<(PathBuf, bool)>>,
    /// The ids of the checks run, in order.
    ids: Mutex<Vec<String>>,
    /// This check errors (no exit status) the first time it runs.
    fail_first: Option<String>,
    /// A check has begun to run (set before its delay): tests wait on it, not on a clock.
    started: std::sync::atomic::AtomicBool,
    /// While set the check does not finish: a test gates it instead of timing it.
    hold: AtomicBool,
}

impl FakeRunner {
    fn new(exit: Option<i32>) -> Arc<Self> {
        Arc::new(Self {
            exit,
            problem: None,
            enforcing: true,
            delay: Duration::ZERO,
            output: b"check output\n".to_vec(),
            base_output: None,
            ran_in: Mutex::new(vec![]),
            ids: Mutex::new(vec![]),
            fail_first: None,
            started: Default::default(),
            hold: Default::default(),
        })
    }
}

impl CheckRunner for FakeRunner {
    fn fingerprint(&self) -> Result<String, VerifyError> {
        if self.enforcing {
            Ok("fake-os sandbox=fake".into())
        } else {
            Err(VerifyError("the none sandbox cannot confine a check: independent evidence needs an enforcing sandbox".into()))
        }
    }

    fn run(
        &self,
        dir: &std::path::Path,
        _: &std::path::Path,
        check: &ContractCheck,
        _: Duration,
        stop: &AtomicBool,
    ) -> CheckRun {
        self.started
            .store(true, std::sync::atomic::Ordering::SeqCst);
        let until = std::time::Instant::now() + self.delay;
        while (std::time::Instant::now() < until
            || self.hold.load(std::sync::atomic::Ordering::SeqCst))
            && !stop.load(std::sync::atomic::Ordering::SeqCst)
        {
            std::thread::sleep(Duration::from_millis(10));
        }
        if stop.load(std::sync::atomic::Ordering::SeqCst) {
            return CheckRun {
                id: check.id.clone(),
                command_hash: String::new(),
                exit_code: None,
                problem: Some("verification was stopped".into()),
                output: vec![],
            };
        }
        // what a check would see: the committed tree, with no repository in it
        self.ran_in
            .lock()
            .unwrap()
            .push((dir.to_path_buf(), dir.join(".git").exists()));
        let n = {
            let mut ids = self.ids.lock().unwrap();
            ids.push(check.id.clone());
            ids.iter().filter(|i| **i == check.id).count()
        };
        if n == 1 && self.fail_first.as_deref() == Some(check.id.as_str()) {
            return CheckRun {
                id: check.id.clone(),
                command_hash: String::new(),
                exit_code: None,
                problem: Some("the check timed out".into()),
                output: vec![],
            };
        }
        CheckRun {
            id: check.id.clone(),
            command_hash: "unused".into(),
            exit_code: self.exit,
            problem: self.problem.clone(),
            // the output differs on every run (a timestamp, a temp path): a re-recorded row would not
            // be identical
            output: {
                let mut out = if dir.file_name().is_some_and(|n| n == "base") {
                    self.base_output
                        .clone()
                        .unwrap_or_else(|| self.output.clone())
                } else {
                    self.output.clone()
                };
                if !out.is_empty() {
                    out.extend(format!("(run {n})\n").as_bytes());
                }
                out
            },
        }
    }
}

fn verified_contract(allowed: &[&str]) -> (Contract, Plan) {
    let mut c = small_attempts_contract();
    c.envelope.allowed_paths = allowed.iter().map(|s| (*s).to_owned()).collect();
    c.checks = vec![ContractCheck {
        id: "unit".into(),
        command: "cargo test".into(),
    }];
    c.criteria[0].check_ids = vec!["unit".into()];
    (c, common::plan())
}

fn small_attempts_contract() -> Contract {
    let mut c = common::contract();
    c.envelope.budgets.attempt_max_cost_usd = Some(0.01);
    c
}

fn real_verifier(w: &OutcomeWriter, r: &Rig, runner: Arc<FakeRunner>) -> IndependentVerifier {
    IndependentVerifier::for_outcome(
        w,
        r.repo.clone(),
        r.home.clone(),
        runner,
        Duration::from_secs(30),
    )
    .expect("the contract is approved")
}

fn evidence_of(w: &OutcomeWriter) -> Vec<EvidenceRecord> {
    w.list_evidence(w.id()).unwrap()
}

fn to_verified(w: &mut OutcomeWriter) -> Result<(), synthesis_session::Error> {
    for to in [OutcomeState::Verifying, OutcomeState::Verified] {
        let rev = w.get_outcome(w.id()).unwrap().revision;
        w.transition_outcome(rev, to, None, &Effects::default())?;
    }
    Ok(())
}

const WRITE_INSIDE: &str = "mkdir -p src; echo made > src/ok.txt";

#[test]
fn ac08_an_outcome_is_verified_only_through_independent_evidence() {
    need_git!();
    let r = rig();
    let (c, p) = verified_contract(&["src/**"]);
    let (mut w, _) = r.fresh_with(&c, &p);
    let s = Script(WRITE_INSIDE.into());
    let runner = FakeRunner::new(Some(0));
    let mut sched = Scheduler::new(host(&r, &s), real_verifier(&w, &r, runner.clone()), cfg());
    let (stop, reports) = sched
        .run(&mut w, &clock, Duration::from_millis(30))
        .unwrap();
    assert_eq!(stop, Stop::Succeeded, "{reports:?}");
    // the check ran in a fresh checkout under <home>/verify, never in an attempt's worktree
    let ran = runner.ran_in.lock().unwrap().clone();
    assert_eq!(ran.len(), 1, "{ran:?}");
    assert!(
        ran[0].0.starts_with(r.home.join("verify")),
        "{:?}",
        ran[0].0
    );
    assert!(!ran[0].1, "the checkout has no .git");
    // evidence: independent runner, bound to the contract's frozen command, with its output stored
    let ev = evidence_of(&w);
    let check = ev
        .iter()
        .find(|e| e.check_id == "unit")
        .expect("check evidence");
    assert_eq!(check.trust_class, TrustClass::IndependentCheck);
    assert_eq!(check.producer, "independent_runner");
    assert_eq!(check.result, "passed");
    assert_eq!(check.criteria_ids, ["c1"]);
    assert!(check.sha256.is_some(), "output stored as an artifact");
    assert!(
        ev.iter()
            .any(|e| e.check_id == SCOPE_CHECK && e.result == "passed")
    );
    to_verified(&mut w).expect("every mandatory criterion has current independent evidence");
}

#[test]
fn a_write_outside_the_envelope_fails_verification_even_if_the_check_passes() {
    need_git!();
    let r = rig();
    let (c, p) = verified_contract(&["src/**"]);
    let (mut w, _) = r.fresh_with(&c, &p);
    // a shell redirect the permission rules never saw
    let s = Script("echo pwned > README.md".into());
    let runner = FakeRunner::new(Some(0));
    let mut sched = Scheduler::new(host(&r, &s), real_verifier(&w, &r, runner.clone()), cfg());
    let (stop, reports) = sched
        .run(&mut w, &clock, Duration::from_millis(30))
        .unwrap();
    assert!(matches!(&stop, Stop::Blocked(_)), "{stop:?} {reports:?}");
    assert!(
        reports
            .iter()
            .flat_map(|r| &r.errors)
            .any(|e| e.contains("outside its envelope") && e.contains("README.md")),
        "{reports:?}"
    );
    assert!(
        runner.ran_in.lock().unwrap().is_empty(),
        "no code of the violating attempt was run"
    );
    assert!(
        evidence_of(&w)
            .iter()
            .any(|e| e.check_id == SCOPE_CHECK && e.result == "failed")
    );
    assert!(to_verified(&mut w).is_err());
}

#[test]
fn changed_definitions_are_flagged_and_the_outcome_is_not_verified() {
    need_git!();
    let r = rig();
    // the envelope allows everything, so the scope passes: only the flag stands in the way
    let (c, p) = verified_contract(&["**"]);
    let (mut w, _) = r.fresh_with(&c, &p);
    let s = Script("mkdir -p src; echo made > src/ok.txt; echo '{\"scripts\":{\"test\":\"true\"}}' > package.json".into());
    let runner = FakeRunner::new(Some(0));
    let mut sched = Scheduler::new(host(&r, &s), real_verifier(&w, &r, runner.clone()), cfg());
    let (stop, reports) = sched
        .run(&mut w, &clock, Duration::from_millis(30))
        .unwrap();
    assert!(matches!(&stop, Stop::Blocked(_)), "{stop:?}");
    assert!(
        reports
            .iter()
            .flat_map(|r| &r.errors)
            .any(|e| e.contains("definitions_changed")),
        "{reports:?}"
    );
    let ev = evidence_of(&w);
    assert!(
        ev.iter()
            .any(|e| e.check_id == SCOPE_CHECK && e.result == "passed")
    );
    assert!(
        runner.ran_in.lock().unwrap().is_empty(),
        "the checks of a flagged change are not run"
    );
    assert!(to_verified(&mut w).is_err());
}

#[test]
fn a_failing_or_stuck_check_and_a_forged_report_never_verify() {
    need_git!();
    for (exit, problem) in [
        (Some(1), None),
        (None, Some("the check timed out".to_string())),
    ] {
        let r = rig();
        let (c, p) = verified_contract(&["src/**"]);
        let (mut w, _) = r.fresh_with(&c, &p);
        // the worker also leaves a report claiming everything passed
        let s = Script(format!(
            "{WRITE_INSIDE}; echo '{{\"status\":\"passed\",\"checks\":[{{\"id\":\"unit\",\"ok\":true}}]}}' > src/verification-report.json"
        ));
        let runner = Arc::new(FakeRunner {
            exit,
            problem: problem.clone(),
            enforcing: true,
            delay: Duration::ZERO,
            output: b"check output\n".to_vec(),
            base_output: None,
            ran_in: Mutex::new(vec![]),
            ids: Mutex::new(vec![]),
            fail_first: None,
            started: Default::default(),
            hold: Default::default(),
        });
        let mut sched = Scheduler::new(host(&r, &s), real_verifier(&w, &r, runner), cfg());
        let (stop, _) = sched
            .run(&mut w, &clock, Duration::from_millis(30))
            .unwrap();
        assert!(matches!(&stop, Stop::Blocked(_)), "{exit:?}: {stop:?}");
        let ev = evidence_of(&w);
        let unit = ev.iter().find(|e| e.check_id == "unit");
        if problem.is_some() {
            // a run with no exit status is not evidence: no row, and the same commit can be retried
            assert!(unit.is_none(), "{ev:?}");
        } else {
            assert_eq!(unit.unwrap().result, "failed");
        }
        assert!(to_verified(&mut w).is_err(), "{exit:?}");
    }
}

#[test]
fn without_an_enforcing_sandbox_no_independent_evidence_is_produced() {
    need_git!();
    let r = rig();
    let (c, p) = verified_contract(&["src/**"]);
    let (mut w, _) = r.fresh_with(&c, &p);
    let s = Script(WRITE_INSIDE.into());
    let runner = Arc::new(FakeRunner {
        exit: Some(0),
        problem: None,
        enforcing: false,
        delay: Duration::ZERO,
        output: b"check output\n".to_vec(),
        base_output: None,
        ran_in: Mutex::new(vec![]),
        ids: Mutex::new(vec![]),
        fail_first: None,
        started: Default::default(),
        hold: Default::default(),
    });
    let mut sched = Scheduler::new(host(&r, &s), real_verifier(&w, &r, runner.clone()), cfg());
    let (stop, reports) = sched
        .run(&mut w, &clock, Duration::from_millis(30))
        .unwrap();
    // verification cannot start: the items wait in `verifying` and say why
    assert_eq!(stop, Stop::WaitingVerification, "{reports:?}");
    assert!(runner.ran_in.lock().unwrap().is_empty());
    assert!(
        evidence_of(&w).is_empty(),
        "nothing was recorded: {:?}",
        evidence_of(&w)
    );
    assert!(
        reports
            .iter()
            .flat_map(|r| &r.errors)
            .any(|e| e.contains("enforcing sandbox")),
        "{reports:?}"
    );
}

#[test]
fn verification_runs_off_thread_and_the_walk_keeps_ticking() {
    need_git!();
    let r = rig();
    let (c, p) = verified_contract(&["src/**"]);
    let (mut w, _) = r.fresh_with(&c, &p);
    let s = Script(WRITE_INSIDE.into());
    let runner = Arc::new(FakeRunner {
        exit: Some(0),
        problem: None,
        enforcing: true,
        delay: Duration::ZERO,
        output: b"check output\n".to_vec(),
        base_output: None,
        ran_in: Mutex::new(vec![]),
        ids: Mutex::new(vec![]),
        fail_first: None,
        started: Default::default(),
        hold: Default::default(),
    });
    runner.hold.store(true, std::sync::atomic::Ordering::SeqCst);
    let mut sched = Scheduler::new(host(&r, &s), real_verifier(&w, &r, runner.clone()), cfg());
    // safety net only: if a tick blocks on the gated check the test fails instead of hanging
    let done = Arc::new(AtomicBool::new(false));
    let wd = std::thread::spawn({
        let (runner, done) = (runner.clone(), done.clone());
        move || {
            for _ in 0..1200 {
                if done.load(std::sync::atomic::Ordering::SeqCst) {
                    return;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            runner
                .hold
                .store(false, std::sync::atomic::Ordering::SeqCst);
        }
    });
    let mut gated_ticks = 0;
    let stop = loop {
        let rep = sched.tick(&mut w, clock()).unwrap();
        if let Some(s) = rep.stop {
            break s;
        }
        // a tick that returned while the check is started and still held did not wait for it
        if runner.started.load(std::sync::atomic::Ordering::SeqCst)
            && runner.hold.load(std::sync::atomic::Ordering::SeqCst)
        {
            gated_ticks += 1;
            if gated_ticks == 6 {
                runner
                    .hold
                    .store(false, std::sync::atomic::Ordering::SeqCst);
            }
        }
        std::thread::sleep(Duration::from_millis(5));
    };
    done.store(true, std::sync::atomic::Ordering::SeqCst);
    wd.join().unwrap();
    assert_eq!(stop, Stop::Succeeded);
    assert!(
        gated_ticks >= 6,
        "the walk kept ticking while the check was held ({gated_ticks})"
    );
}

fn cargo_summary(n: u32) -> Vec<u8> {
    format!("running {n} tests\ntest result: ok. {n} passed; 0 failed; 0 ignored; 0 measured; 0 filtered out\n")
        .into_bytes()
}

fn runner_printing(candidate: Vec<u8>, base: Option<Vec<u8>>) -> Arc<FakeRunner> {
    Arc::new(FakeRunner {
        exit: Some(0),
        problem: None,
        enforcing: true,
        delay: Duration::ZERO,
        output: candidate,
        base_output: base,
        ran_in: Mutex::new(vec![]),
        ids: Mutex::new(vec![]),
        fail_first: None,
        started: Default::default(),
        hold: Default::default(),
    })
}

#[test]
fn a_check_that_ran_zero_tests_is_not_evidence_and_fewer_tests_than_the_base_are_flagged() {
    need_git!();
    // zero tests: no row (an error is not evidence), the item fails, nothing verifies
    let r = rig();
    let (c, p) = verified_contract(&["src/**"]);
    let (mut w, _) = r.fresh_with(&c, &p);
    let s = Script(WRITE_INSIDE.into());
    let runner = runner_printing(cargo_summary(0), None);
    let mut sched = Scheduler::new(host(&r, &s), real_verifier(&w, &r, runner), cfg());
    let (stop, reports) = sched
        .run(&mut w, &clock, Duration::from_millis(30))
        .unwrap();
    assert!(matches!(&stop, Stop::Blocked(_)), "{stop:?}");
    assert!(
        reports
            .iter()
            .flat_map(|r| &r.errors)
            .any(|e| e.contains("ran no tests")),
        "{reports:?}"
    );
    assert!(evidence_of(&w).iter().all(|e| e.check_id != "unit"));
    assert!(to_verified(&mut w).is_err());

    // fewer tests than the base commit had: the evidence is flagged and the item fails
    let r = rig();
    let (c, p) = verified_contract(&["src/**"]);
    let (mut w, _) = r.fresh_with(&c, &p);
    let runner = runner_printing(cargo_summary(3), Some(cargo_summary(5)));
    let mut sched = Scheduler::new(host(&r, &s), real_verifier(&w, &r, runner), cfg());
    let (stop, reports) = sched
        .run(&mut w, &clock, Duration::from_millis(30))
        .unwrap();
    assert!(matches!(&stop, Stop::Blocked(_)), "{stop:?}");
    assert!(
        reports
            .iter()
            .flat_map(|r| &r.errors)
            .any(|e| e.contains("flagged") && e.contains("tests_weakened")),
        "{reports:?}"
    );
    assert!(
        evidence_of(&w)
            .iter()
            .any(|e| e.check_id == "unit" && e.flags == ["tests_weakened"])
    );
    assert!(to_verified(&mut w).is_err());

    // the same count as the base is fine
    let r = rig();
    let (c, p) = verified_contract(&["src/**"]);
    let (mut w, _) = r.fresh_with(&c, &p);
    let runner = runner_printing(cargo_summary(5), Some(cargo_summary(5)));
    let mut sched = Scheduler::new(host(&r, &s), real_verifier(&w, &r, runner), cfg());
    let (stop, _) = sched
        .run(&mut w, &clock, Duration::from_millis(30))
        .unwrap();
    assert_eq!(stop, Stop::Succeeded);
}

#[test]
fn a_symlink_or_a_changed_file_mode_fails_the_envelope_check() {
    need_git!();
    for (script, what) in [
        ("mkdir -p src; ln -s /etc/passwd src/link", "src/link"),
        ("mkdir -p src; echo x > src/run.sh; chmod +x src/run.sh", ""),
    ] {
        let r = rig();
        let (c, p) = verified_contract(&["src/**"]);
        let (mut w, _) = r.fresh_with(&c, &p);
        let s = Script(script.into());
        let mut sched = Scheduler::new(
            host(&r, &s),
            real_verifier(&w, &r, FakeRunner::new(Some(0))),
            cfg(),
        );
        let (stop, reports) = sched
            .run(&mut w, &clock, Duration::from_millis(30))
            .unwrap();
        if what.is_empty() {
            // a new executable file is an addition, not a mode change: it passes the scope
            assert_eq!(stop, Stop::Succeeded, "{reports:?}");
        } else {
            assert!(matches!(&stop, Stop::Blocked(_)), "{stop:?}");
            assert!(
                reports
                    .iter()
                    .flat_map(|r| &r.errors)
                    .any(|e| e.contains(what) && e.contains("links")),
                "{reports:?}"
            );
        }
    }
}

#[test]
fn cancelling_stops_a_running_verification_promptly() {
    need_git!();
    let r = rig();
    let (c, p) = verified_contract(&["src/**"]);
    let (mut w, _) = r.fresh_with(&c, &p);
    let s = Script(WRITE_INSIDE.into());
    let runner = Arc::new(FakeRunner {
        exit: Some(0),
        problem: None,
        enforcing: true,
        delay: Duration::from_secs(60),
        output: vec![],
        base_output: None,
        ran_in: Mutex::new(vec![]),
        ids: Mutex::new(vec![]),
        fail_first: None,
        started: Default::default(),
        hold: Default::default(),
    });
    let mut sched = Scheduler::new(host(&r, &s), real_verifier(&w, &r, runner.clone()), cfg());
    // wait until the check itself runs (not just the scope pass before it); under load that takes long
    let deadline = std::time::Instant::now() + Duration::from_secs(60);
    while !runner.started.load(std::sync::atomic::Ordering::SeqCst) {
        assert!(
            std::time::Instant::now() < deadline,
            "the check never started"
        );
        sched.tick(&mut w, clock()).unwrap();
        std::thread::sleep(Duration::from_millis(20));
    }
    // a tick does not wait for the check (it runs on its own thread): the owner loop that calls it
    // keeps applying pause, cancel and decisions while a check runs for minutes. A blocked tick
    // would last the whole 60 s delay, so a bound far below that still proves it.
    for _ in 0..3 {
        let t = std::time::Instant::now();
        sched.tick(&mut w, clock()).unwrap();
        assert!(sched.verifier.busy());
        assert!(t.elapsed() < Duration::from_secs(30), "{:?}", t.elapsed());
    }
    let t0 = std::time::Instant::now();
    sched.control.cancel.cancel();
    let rep = sched.tick(&mut w, clock()).unwrap();
    assert_eq!(rep.stop, Some(Stop::Cancelled), "{rep:?}");
    // dropping the scheduler joins the verification thread, which the cancel already stopped
    drop(sched);
    assert!(t0.elapsed() < Duration::from_secs(30), "{:?}", t0.elapsed());
}

#[test]
fn a_retry_after_a_partial_failure_runs_and_records_only_the_missing_checks() {
    need_git!();
    let r = rig();
    let (mut c, p) = verified_contract(&["src/**"]);
    c.checks = vec![
        ContractCheck {
            id: "alpha".into(),
            command: "cargo test".into(),
        },
        ContractCheck {
            id: "beta".into(),
            command: "cargo clippy".into(),
        },
    ];
    c.criteria[0].check_ids = vec!["alpha".into(), "beta".into()];
    let (mut w, _) = r.fresh_with(&c, &p);
    let s = Script(WRITE_INSIDE.into());
    let runner = Arc::new(FakeRunner {
        exit: Some(0),
        problem: None,
        enforcing: true,
        delay: Duration::ZERO,
        output: b"check output\n".to_vec(),
        base_output: None,
        ran_in: Mutex::new(vec![]),
        ids: Mutex::new(vec![]),
        fail_first: Some("beta".into()),
        started: Default::default(),
        hold: Default::default(),
    });
    let mut sched = Scheduler::new(host(&r, &s), real_verifier(&w, &r, runner.clone()), cfg());
    let (stop, reports) = sched
        .run(&mut w, &clock, Duration::from_millis(30))
        .unwrap();
    assert_eq!(stop, Stop::Succeeded, "{reports:?}");
    assert!(
        !reports
            .iter()
            .flat_map(|r| &r.errors)
            .any(|e| e.contains("already recorded")),
        "{reports:?}"
    );
    let ids = runner.ids.lock().unwrap().clone();
    assert_eq!(
        ids.iter().filter(|i| *i == "alpha").count(),
        1,
        "alpha ran again: {ids:?}"
    );
    assert_eq!(ids.iter().filter(|i| *i == "beta").count(), 2, "{ids:?}");
    to_verified(&mut w).unwrap();
}

#[test]
fn a_grant_the_profile_refuses_is_not_burned_by_the_failed_start() {
    need_git!();
    let r = rig();
    let (mut w, items) = r.fresh();
    let it = items.iter().find(|(k, _)| k == "one").unwrap().1.clone();
    // item `one` waits for a decision whose grant is `Bash(git push)`, which the contract denies
    // (git.push is unlisted): the grant exists, but no profile may carry it
    for to in [
        WorkItemState::Ready,
        WorkItemState::Leased,
        WorkItemState::Running,
        WorkItemState::WaitingDecision,
    ] {
        let rev = w.get_work_item(&it).unwrap().revision;
        w.transition_work_item(&it, rev, to, &Effects::default())
            .unwrap();
    }
    let o = w.get_outcome(w.id()).unwrap();
    let (c, ..) = w.get_contract(w.id(), o.contract_version).unwrap();
    let policy_hash = compile_envelope(&c.envelope, &[]).unwrap().policy_hash();
    let binding = decision_binding(
        o.contract_version,
        o.plan_version.unwrap(),
        None,
        &policy_hash,
        "",
        0,
        &it,
    );
    let fmt = time::macros::format_description!(
        "[year]-[month]-[day]T[hour]:[minute]:[second].[subsecond digits:3]Z"
    );
    let (d, _) = w
        .new_decision(&NewDecision {
            kind: "approval_needed".into(),
            dedupe_key: "g".into(),
            brief: serde_json::json!({"why": "x", "alternatives": [{"id": "grant_once", "label": "x"}], "safe_default": "HOLD"}),
            binding,
            expires_at: (OffsetDateTime::now_utc() + time::Duration::hours(2)).format(&fmt).unwrap(),
            action: Some(serde_json::json!({"grant_rules": ["Bash(git push)"]})),
        })
        .unwrap();
    w.transition_decision(
        &d.id,
        1,
        DecisionState::Pending,
        &d.binding,
        &Effects::default(),
    )
    .unwrap();
    let d = w.list_decisions(w.id()).unwrap().remove(0);
    w.resolve_decision(
        &Resolution {
            decision_id: d.id.clone(),
            decision_revision: d.revision,
            approver: "user:cli".into(),
            choice: "grant_once".into(),
            brief_sha256: d.brief_sha256.clone(),
        },
        &d.binding,
    )
    .unwrap();
    let g = w.create_grant(&d.id, &d.binding, 24).unwrap();
    let rev = w.get_work_item(&it).unwrap().revision;
    w.transition_work_item(&it, rev, WorkItemState::Ready, &Effects::default())
        .unwrap();
    let s = Script("exit 0".into());
    let mut sched = Scheduler::new(host(&r, &s), EvidenceVerifier::new(&[]), cfg());
    let rep = sched.tick(&mut w, clock()).unwrap();
    assert!(
        rep.errors
            .iter()
            .any(|e| e.contains("not allowed for an outcome attempt")),
        "the start was refused: {rep:?}"
    );
    // the grant is still there, unused: a refused start does not burn it
    let again = w.usable_grants(&it).unwrap();
    assert_eq!(again.len(), 1, "{again:?}");
    assert_eq!(again[0].id, g.id);
    assert!(again[0].consumed_at.is_none());
}

/// An attempt a dead owner left running with a live worker, as a new owner finds it: item `one`
/// `running`, attempt `running` with `reserved` usd, a real process with its recorded start token.
fn adopted_live(
    r: &Rig,
    w: &mut OutcomeWriter,
    items: &[(String, WorkItemId)],
    reserved: f64,
) -> (Child, AttemptRecord) {
    adopted_live_of(r, w, items, "one", reserved)
}

fn adopted_live_of(
    r: &Rig,
    w: &mut OutcomeWriter,
    items: &[(String, WorkItemId)],
    key: &str,
    reserved: f64,
) -> (Child, AttemptRecord) {
    use std::os::unix::process::CommandExt;
    let one = items.iter().find(|(n, _)| n == key).unwrap().1.clone();
    ready(w, &one);
    let rev = w.get_work_item(&one).unwrap().revision;
    w.transition_work_item(&one, rev, WorkItemState::Leased, &Effects::default())
        .unwrap();
    let env = r
        .allocator()
        .allocate(w, &r.spec(&one, 1), OffsetDateTime::now_utc())
        .unwrap();
    r.wm.mark_started(env.git.as_ref().unwrap()).unwrap();
    let rev = w.list_attempts(&one).unwrap()[0].revision;
    w.transition_attempt(
        &env.attempt.id,
        rev,
        AttemptState::Running,
        &AttemptEnd::default(),
        &Effects::default(),
    )
    .unwrap();
    let rev = w.get_work_item(&one).unwrap().revision;
    w.transition_work_item(&one, rev, WorkItemState::Running, &Effects::default())
        .unwrap();
    w.reserve_budget(&env.attempt.id, env.attempt.fence, reserved)
        .unwrap();
    let child = Command::new("sleep")
        .arg("60")
        .process_group(0)
        .spawn()
        .unwrap();
    let start = OsProbe.start_time(child.id());
    w.record_worker(
        &env.attempt.id,
        env.attempt.fence,
        child.id(),
        start.as_deref(),
    )
    .unwrap();
    (child, w.list_attempts(&one).unwrap().remove(0))
}

#[test]
fn an_adopted_live_attempt_keeps_its_reservation_without_blocking_and_a_lost_one_is_never_rerun() {
    need_git!();
    let r = rig();
    let s = Script("exit 0".into());
    // (1) the live attempt holds the WHOLE cap (reservation == cap): that is its own share, not an
    // overrun, so the outcome is not blocked while its worker lives; nothing else starts
    let (mut w, items) = r.fresh();
    let id = w.id().clone();
    let (mut child, a) = adopted_live(&r, &mut w, &items, 1.0);
    let mut c = cfg();
    c.max_cost_usd = Some(1.0);
    let mut sched = Scheduler::new(host(&r, &s), EvidenceVerifier::new(&[]), c);
    for _ in 0..3 {
        let rep = sched.tick(&mut w, clock()).unwrap();
        assert!(rep.stop.is_none() && rep.leased.is_empty(), "{rep:?}");
        assert_eq!(w.get_outcome(&id).unwrap().state, OutcomeState::Running);
        assert_eq!(
            w.outcome_spend(&id).unwrap(),
            1.0,
            "counted once, not twice"
        );
    }
    // (2) the worker dies without ever recording a final usage (whole-machine loss): the attempt
    // is `lost`, never rerun in place, its reservation still counts as spent, and with nothing
    // left the outcome stops Blocked with the real numbers
    child.kill().unwrap();
    child.wait().unwrap();
    let rep = sched.tick(&mut w, clock()).unwrap();
    let Some(Stop::Blocked(why)) = &rep.stop else {
        panic!("{rep:?}")
    };
    assert!(
        why.contains("cost cap") && why.contains("$1.0000 of $1.0000"),
        "{why}"
    );
    let at = w.list_attempts(&a.work_item_id).unwrap();
    assert_eq!(at.len(), 1, "not rerun in place: {at:?}");
    assert_eq!(at[0].state, AttemptState::Lost);
    assert_eq!(w.outcome_spend(&id).unwrap(), 1.0);
    // (3) with budget left a NEW attempt (new session) does the item after the loss
    let (mut w, items) = r.fresh();
    let id = w.id().clone();
    let (mut child, a) = adopted_live(&r, &mut w, &items, 1.0);
    let mut c = cfg();
    c.max_cost_usd = Some(2.0);
    let mut sched = Scheduler::new(host(&r, &s), EvidenceVerifier::new(&[]), c);
    let rep = sched.tick(&mut w, clock()).unwrap();
    assert!(rep.stop.is_none(), "{rep:?}");
    child.kill().unwrap();
    child.wait().unwrap();
    let (stop, reports) = sched
        .run(&mut w, &clock, Duration::from_millis(30))
        .unwrap();
    assert_eq!(stop, Stop::Succeeded, "{reports:?}\n{:?}", states(&w));
    let at = w.list_attempts(&a.work_item_id).unwrap();
    assert_eq!(
        at.iter().map(|x| (x.ordinal, x.state)).collect::<Vec<_>>(),
        [(1, AttemptState::Lost), (2, AttemptState::Finished)]
    );
    assert_ne!(at[0].session_id, at[1].session_id);
    assert!(w.outcome_spend(&id).unwrap() <= 2.0);
}

/// A host that never identifies or touches an adopted worker (an `unknown` attempt: its session
/// is open somewhere, nothing proves whose process it is).
struct BlindHost;

impl WorkerHost for BlindHost {
    type Handle = ();
    fn start(
        &mut self,
        _: &mut OutcomeWriter,
        _: &WorkItemRecord,
        _: u32,
        _: f64,
        _: u32,
    ) -> Result<(), SchedError> {
        unreachable!("the cap is used up")
    }
    fn poll(
        &mut self,
        _: &mut OutcomeWriter,
        _: &mut (),
        _: &CancelFlag,
    ) -> Result<Option<Settled>, SchedError> {
        Ok(None)
    }
}

#[test]
fn an_unknown_live_attempt_with_the_cap_used_up_blocks_the_outcome_after_its_wall_cap_unkilled() {
    need_git!();
    let r = rig();
    let (mut w, items) = r.fresh();
    let id = w.id().clone();
    let (mut child, a) = adopted_live(&r, &mut w, &items, 1.0);
    let mk = |after: Option<Duration>| {
        let mut c = cfg();
        c.max_cost_usd = Some(1.0);
        c.unknown_after = after;
        Scheduler::new(BlindHost, EvidenceVerifier::new(&[]), c)
    };
    let late = clock() + Duration::from_secs(7200);
    let hour = Some(Duration::from_secs(3600));
    // young: it may still be a healthy worker holding its own reservation
    let rep = mk(hour).tick(&mut w, clock()).unwrap();
    assert!(rep.stop.is_none(), "{rep:?}");
    // no limit given: never decided (the old behaviour)
    assert!(mk(None).tick(&mut w, late).unwrap().stop.is_none());
    // a pause is a person's word and wins; so does a cancel
    let mut sc = mk(hour);
    sc.control.pause();
    assert_eq!(sc.tick(&mut w, late).unwrap().stop, Some(Stop::Paused));
    // past wall cap + grace: blocked, naming the attempt, and nothing unproven is killed
    let rep = mk(hour).tick(&mut w, late).unwrap();
    let Some(Stop::Blocked(why)) = &rep.stop else {
        panic!("{rep:?}")
    };
    assert!(
        why.contains(&a.id.to_string()) && why.contains("one"),
        "{why}"
    );
    assert_eq!(w.get_outcome(&id).unwrap().state, OutcomeState::Blocked);
    assert!(child.try_wait().unwrap().is_none(), "the worker was killed");
    assert_eq!(
        w.list_attempts(&a.work_item_id).unwrap()[0].state,
        AttemptState::Running
    );
    child.kill().unwrap();
    child.wait().unwrap();
}

#[test]
fn a_held_worker_past_the_limit_is_not_an_unknown_attempt() {
    need_git!();
    let r = rig();
    let (mut w, items) = r.fresh();
    let id = w.id().clone();
    let s = Script("sleep 30".into());
    let mut c = cfg();
    c.max_parallel = 1;
    c.max_cost_usd = Some(1.0);
    c.unknown_after = Some(Duration::from_secs(2));
    // `one` is held by this scheduler and takes the whole cap
    let mut sched = Scheduler::new(host(&r, &s), EvidenceVerifier::new(&[]), c);
    assert_eq!(sched.tick(&mut w, clock()).unwrap().leased, ["one"]);
    std::thread::sleep(Duration::from_secs(3));
    // `two` is a young unheld attempt of a dead owner: the held `one` is older than the limit
    // (supervised by its wall cap and stop grace), so nothing is unknown yet
    let (mut child, _) = adopted_live_of(&r, &mut w, &items, "two", 0.01);
    // what is reserved now is the whole cap
    sched.cfg.max_cost_usd = Some(w.outcome_spend(&id).unwrap());
    let rep = sched.tick(&mut w, clock()).unwrap();
    assert!(rep.stop.is_none(), "{rep:?}");
    assert_eq!(sched.running_count(), 1, "{:?}", states(&w));
    assert_eq!(w.get_outcome(&id).unwrap().state, OutcomeState::Running);
    child.kill().unwrap();
    child.wait().unwrap();
}
