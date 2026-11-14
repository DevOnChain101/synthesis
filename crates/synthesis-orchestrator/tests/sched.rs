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
use synthesis_session::OsProbe;
use synthesis_session::outcome::*;
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
        "echo start >> {l}; sleep 1; echo end >> {l}; echo made > out.txt",
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
            "d={d}; touch $d/live.$$; n=$(ls $d | grep -c '^live\\.'); echo $n >> $d/seen; sleep 1.2; rm $d/live.$$",
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
    r.wm.mark_started(&env.git).unwrap();
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
        })
        .collect();
    items.push(PlanItem {
        key: "z-check".into(),
        kind: WorkItemKind::Verify,
        requires: vec!["i000".into()],
        capabilities: vec![],
        covers: vec!["c1".into()],
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
    let rev = w.get_work_item(it).unwrap().revision;
    w.transition_work_item(it, rev, WorkItemState::Ready, &Effects::default())
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
    r.wm.mark_started(&env.git).unwrap();
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
fn a_walk_that_only_reports_errors_gives_up_and_stops_its_workers() {
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
    let s = ByItem {
        slow: "two",
        fail: None,
    };
    let mut c = cfg();
    c.max_stalled_ticks = 4;
    let mut sched = Scheduler::new(host(&r, &s), Broken, c);
    let err = sched
        .run(&mut w, &clock, Duration::from_millis(10))
        .unwrap_err();
    assert!(err.to_string().contains("no progress"), "{err}");
    assert_eq!(sched.running_count(), 0);
    assert!(pids(&w).iter().all(|p| !alive(*p)));
}
