//! Retry from the blocked attempt's commit (B9, ADR-019 G-3): after a person answers a decision of
//! a blocked attempt, the next attempt of the item starts from that attempt's commit, not from the
//! base commit. The commit it carries is still judged against the BASE commit (a change outside
//! the envelope inside the carried work is flagged), and evidence of the earlier attempt does not
//! count for the new one.
#![cfg(unix)]

#[macro_use]
mod common;
use common::*;

use std::fs;
use std::path::Path;
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use serde_json::json;
use synthesis_core::TaskStatus;
use synthesis_core::outcome::*;
use synthesis_orchestrator::*;
use synthesis_session::outcome::*;
use time::OffsetDateTime;

struct Fp;

impl CheckRunner for Fp {
    fn fingerprint(&self) -> Result<String, VerifyError> {
        Ok("fp".into())
    }
    fn run(&self, _: &Path, _: &Path, _: &ContractCheck, _: Duration, _: &AtomicBool) -> CheckRun {
        unreachable!("an agent item runs no check")
    }
}

fn stamp() -> String {
    let fmt = time::macros::format_description!(
        "[year]-[month]-[day]T[hour]:[minute]:[second].[subsecond digits:3]Z"
    );
    (OffsetDateTime::now_utc() + time::Duration::hours(2))
        .format(&fmt)
        .unwrap()
}

/// Attempt `ordinal` of `one`, started at `from`, that writes `files` and finishes: its commit.
fn attempt(
    r: &Rig,
    w: &mut OutcomeWriter,
    one: &WorkItemId,
    ordinal: u32,
    from: &str,
    files: &[(&str, &str)],
) -> (AttemptRecord, String) {
    let spec = AllocSpec {
        base_commit: from,
        ..r.spec(one, ordinal)
    };
    let e = r.allocator().allocate(w, &spec, now()).unwrap();
    assert_eq!(
        g(&e.git.worktree, &["rev-parse", "HEAD"]),
        from,
        "the attempt starts at the commit it was given"
    );
    w.transition_attempt(
        &e.attempt.id,
        e.attempt.revision,
        AttemptState::Running,
        &AttemptEnd::default(),
        &Effects::default(),
    )
    .unwrap();
    for (p, t) in files {
        let f = e.git.worktree.join(p);
        fs::create_dir_all(f.parent().unwrap()).unwrap();
        fs::write(f, t).unwrap();
    }
    let commit = finish_attempt(&r.wm, w, &e.attempt, &e.git, TaskStatus::Completed).unwrap();
    let a = w
        .list_attempts(one)
        .unwrap()
        .into_iter()
        .find(|a| a.ordinal == ordinal)
        .unwrap();
    (a, commit)
}

/// A decision of attempt `ordinal` (bound to `commit`), pending.
fn decision(
    w: &mut OutcomeWriter,
    one: &WorkItemId,
    ordinal: u32,
    commit: &str,
    key: &str,
) -> DecisionRecord {
    let o = w.get_outcome(w.id()).unwrap();
    let (c, ..) = w.get_contract(w.id(), o.contract_version).unwrap();
    let policy = compile_envelope(&c.envelope, &[]).unwrap().policy_hash();
    let (d, _) = w
        .new_decision(&NewDecision {
            kind: "ambiguity".into(),
            dedupe_key: key.into(),
            brief: json!({"why": "x", "delta_from_contract": "Which API shape?", "alternatives": [
                {"id": "REST", "label": "REST"}, {"id": "retry", "label": "again"}],
                "safe_default": "HOLD"}),
            binding: decision_binding(
                o.contract_version,
                o.plan_version.unwrap(),
                Some(commit),
                &policy,
                "",
                ordinal,
                one,
            ),
            expires_at: stamp(),
            action: Some(json!({"needed": [], "calls": [], "grant_rules": []})),
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
    find(w, &d)
}

fn find(w: &OutcomeWriter, d: &DecisionRecord) -> DecisionRecord {
    w.list_decisions(w.id())
        .unwrap()
        .into_iter()
        .find(|x| x.id == d.id)
        .unwrap()
}

fn answer(w: &mut OutcomeWriter, d: &DecisionRecord, choice: &str) {
    w.resolve_decision(
        &Resolution {
            decision_id: d.id.clone(),
            decision_revision: d.revision,
            approver: "user:cli".into(),
            choice: choice.into(),
            brief_sha256: d.brief_sha256.clone(),
        },
        &d.binding,
    )
    .unwrap();
}

#[test]
fn only_a_person_s_answer_to_a_decision_of_the_previous_attempt_carries_its_commit() {
    need_git!();
    let r = rig();
    let (mut w, one, two) = r.outcome();
    let (a1, c1) = attempt(&r, &mut w, &one, 1, &r.head, &[("src/a.txt", "x\n")]);
    assert_ne!(c1, r.head);
    assert_eq!(a1.subject_commit.as_deref(), Some(c1.as_str()));
    // a retry with no decision starts from the base commit
    assert_eq!(carried_commit(&w, &one, 2).unwrap(), None);
    // a pending decision carries nothing; a rejected one neither
    let d = decision(&mut w, &one, 1, &c1, "first");
    assert_eq!(carried_commit(&w, &one, 2).unwrap(), None);
    answer(&mut w, &d, "reject");
    assert_eq!(carried_commit(&w, &one, 2).unwrap(), None);
    // an answer carries it, for the NEXT attempt only, and not for another item
    let p = decision(&mut w, &one, 1, &c1, "second");
    answer(&mut w, &p, "REST");
    assert_eq!(
        carried_commit(&w, &one, 2).unwrap().as_deref(),
        Some(c1.as_str())
    );
    assert_eq!(carried_commit(&w, &one, 1).unwrap(), None);
    assert_eq!(carried_commit(&w, &two, 2).unwrap(), None);
    // a decision bound to another commit than the attempt's does not carry it
    let (mut w2, one2, _) = r.outcome();
    let (_, k1) = attempt(&r, &mut w2, &one2, 1, &r.head, &[("src/a.txt", "x\n")]);
    let wrong = decision(&mut w2, &one2, 1, &r.head, "wrong");
    answer(&mut w2, &wrong, "REST");
    assert_ne!(k1, r.head);
    assert_eq!(carried_commit(&w2, &one2, 2).unwrap(), None);
}

#[test]
fn the_carried_commit_is_judged_against_the_base_and_earlier_evidence_does_not_count() {
    need_git!();
    let r = rig();
    let (mut w, one, _) = r.outcome();
    // attempt 1 wrote a file OUTSIDE the envelope (`src/**`) and was blocked with a decision
    let (a1, c1) = attempt(
        &r,
        &mut w,
        &one,
        1,
        &r.head,
        &[("src/ok.txt", "ok\n"), ("docs/out.txt", "outside\n")],
    );
    // a row of attempt 1 for that very commit says it passed the scope check
    let ev = NewEvidence {
        work_item_id: one.clone(),
        attempt_id: Some(a1.id.clone()),
        trust_class: TrustClass::IndependentCheck,
        producer: "independent_runner".into(),
        subject_commit: c1.clone(),
        check_id: SCOPE_CHECK.into(),
        command_hash: None,
        criteria_ids: vec![],
        result: "passed".into(),
        exit_code: None,
        env_fingerprint: "fp".into(),
        artifact: None,
        fence: a1.fence,
        flags: vec![],
    };
    w.record_evidence(&ev).unwrap();
    let d = decision(&mut w, &one, 1, &c1, "first");
    answer(&mut w, &d, "REST");
    let carried = carried_commit(&w, &one, 2).unwrap().unwrap();
    assert_eq!(carried, c1);
    // the item runs again (a person's answer), the new attempt starts from the carried commit and
    // adds nothing: its subject is the carried commit
    let it = w.get_work_item(&one).unwrap();
    if it.state == WorkItemState::Leased {
        for to in [
            WorkItemState::Running,
            WorkItemState::WaitingDecision,
            WorkItemState::Ready,
            WorkItemState::Leased,
        ] {
            let rev = w.get_work_item(&one).unwrap().revision;
            w.transition_work_item(&one, rev, to, &Effects::default())
                .unwrap();
        }
    }
    let (a2, c2) = attempt(&r, &mut w, &one, 2, &carried, &[]);
    assert_eq!(c2, c1, "no new change: the subject is the carried commit");
    assert_eq!(
        g(
            &r.repo,
            &["rev-list", "--count", &format!("{}..{c2}", r.head)]
        ),
        "1"
    );

    let mut v = IndependentVerifier::for_outcome(
        &w,
        r.repo.clone(),
        r.home.clone(),
        Arc::new(Fp),
        Duration::from_secs(5),
    )
    .unwrap();
    let item = w
        .list_work_items(w.id())
        .unwrap()
        .into_iter()
        .find(|i| i.id == one)
        .unwrap();
    let t = std::time::Instant::now();
    let verdict = loop {
        match v.verify(&mut w, &item).unwrap() {
            Verdict::Pending => {
                assert!(t.elapsed() < Duration::from_secs(20), "never ended");
                std::thread::sleep(Duration::from_millis(30));
            }
            other => break other,
        }
    };
    // the stale pass of attempt 1 was NOT reused: the commit was inspected against the base and
    // the change outside the envelope inside the carried work is flagged on the new attempt's row
    match verdict {
        Verdict::Failed(why) => assert!(why.contains("outside its envelope"), "{why}"),
        other => panic!("expected a failure, got {other:?}"),
    }
    let rows: Vec<_> = w
        .recent_evidence(w.id(), 50)
        .unwrap()
        .into_iter()
        .filter(|e| e.check_id == SCOPE_CHECK)
        .collect();
    let mine = rows
        .iter()
        .find(|e| e.fence == a2.fence)
        .expect("a row for attempt 2");
    assert_eq!(mine.result, "failed");
    assert!(
        mine.flags.iter().any(|f| f == "outside_envelope"),
        "{mine:?}"
    );
    // and the old row does not count as the current one
    assert!(
        w.authentic_evidence(w.id())
            .unwrap()
            .iter()
            .filter(|e| e.check_id == SCOPE_CHECK && e.result == "passed")
            .all(|e| !e.fence_current),
        "evidence of the earlier attempt must not be current"
    );
}
