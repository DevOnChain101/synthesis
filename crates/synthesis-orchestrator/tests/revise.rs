//! `edit_scope` and `outcome.contract.revise` (orchestrator B4): a changed scope is a NEW contract
//! version the person approves again by its hash. Until then the approved contract stays in force;
//! after it, every decision and unused grant of the old version is dead, the old plan and its
//! evidence do not carry over, and the outcome is replanned.

#[macro_use]
mod common;
use common::*;

use std::process::Command;

use serde_json::{Value, json};
use synthesis_core::outcome::*;
use synthesis_orchestrator::*;
use synthesis_session::outcome::*;
use time::OffsetDateTime;

fn stamp(hours: i64) -> String {
    let fmt = time::macros::format_description!(
        "[year]-[month]-[day]T[hour]:[minute]:[second].[subsecond digits:3]Z"
    );
    (OffsetDateTime::now_utc() + time::Duration::hours(hours))
        .format(&fmt)
        .unwrap()
}

fn later() -> OffsetDateTime {
    OffsetDateTime::now_utc() + time::Duration::hours(5)
}

fn go(w: &mut OutcomeWriter, it: &WorkItemId, to: WorkItemState) {
    let rev = w.get_work_item(it).unwrap().revision;
    w.transition_work_item(it, rev, to, &Effects::default())
        .unwrap();
}

fn outcome_to(w: &mut OutcomeWriter, to: OutcomeState) -> Result<(), synthesis_session::Error> {
    let rev = w.get_outcome(w.id()).unwrap().revision;
    w.transition_outcome(rev, to, Some("test"), &Effects::default())
        .map(|_| ())
}

fn bound(w: &OutcomeWriter, it: &WorkItemId, action_hash: &str) -> Value {
    let o = w.get_outcome(w.id()).unwrap();
    let (c, ..) = w.get_contract(w.id(), o.contract_version).unwrap();
    decision_binding(
        o.contract_version,
        o.plan_version.unwrap(),
        None,
        &compile_envelope(&c.envelope, &[]).unwrap().policy_hash(),
        action_hash,
        0,
        it,
    )
}

/// A pending decision of `kind` for `it` (a stored action with one grant rule).
fn decide(w: &mut OutcomeWriter, it: &WorkItemId, key: &str, kind: &str) -> DecisionRecord {
    let b = bound(w, it, "");
    let (d, _) = w
        .new_decision(&NewDecision {
            kind: kind.into(),
            dedupe_key: key.into(),
            brief: json!({"why": "x", "delta_from_contract": "scope?", "alternatives": [
                {"id": "grant_once", "label": "allow once"}, {"id": "retry", "label": "again"}],
                "safe_default": "HOLD"}),
            binding: b,
            expires_at: stamp(2),
            action: Some(
                json!({"needed": ["Write"], "calls": [], "grant_rules": ["Edit(/src/new.txt)"]}),
            ),
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
    find(w, &d.id)
}

fn find(w: &OutcomeWriter, id: &DecisionId) -> DecisionRecord {
    w.list_decisions(w.id())
        .unwrap()
        .into_iter()
        .find(|d| d.id == *id)
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

fn queue(r: &Rig, w: &OutcomeWriter, key: &str, kind: &str, params: Value, rev: Option<i64>) {
    r.store
        .enqueue_command(NewCommand {
            outcome_id: w.id().clone(),
            idempotency_key: key.into(),
            kind: kind.into(),
            params,
            expected_revision: rev,
            from_surface: Surface::Cli,
        })
        .unwrap();
}

fn cmd(w: &OutcomeWriter, key: &str) -> CommandRecord {
    w.get_command(w.id(), key).unwrap().unwrap()
}

fn revision(w: &OutcomeWriter) -> i64 {
    w.get_outcome(w.id()).unwrap().revision
}

fn widened() -> Contract {
    let mut c = contract();
    c.envelope.allowed_paths.push("docs/**".into());
    c.envelope.budgets.max_cost_usd = 9.0;
    c
}

fn narrowed() -> Contract {
    let mut c = contract();
    c.envelope.budgets.max_cost_usd = 1.0;
    c
}

fn revise(r: &Rig, w: &mut OutcomeWriter, key: &str, c: &Contract) -> CommandRecord {
    queue(
        r,
        w,
        key,
        "outcome.contract.revise",
        json!({"contract": serde_json::to_value(c).unwrap()}),
        Some(revision(w)),
    );
    apply_queued(w, OffsetDateTime::now_utc()).unwrap();
    cmd(w, key)
}

fn approve(r: &Rig, w: &mut OutcomeWriter, key: &str, version: u32) -> CommandRecord {
    let hash = w.get_contract(w.id(), version).unwrap().1;
    queue(
        r,
        w,
        key,
        "outcome.contract.approve",
        json!({"version": version, "hash": hash}),
        None,
    );
    apply_queued(w, OffsetDateTime::now_utc()).unwrap();
    cmd(w, key)
}

/// A blocked outcome (contract v1 approved, plan v1): item `one` waited for a decision that was
/// answered `grant_once` (an unused grant exists), another decision is pending, and the person
/// answered a third with `edit_scope`.
struct Blocked {
    w: OutcomeWriter,
    one: WorkItemId,
    granted: DecisionRecord,
    pending: DecisionRecord,
    edit: DecisionRecord,
}

fn blocked(r: &Rig) -> Blocked {
    let (mut w, items) = r.fresh();
    let one = items.iter().find(|(k, _)| k == "one").unwrap().1.clone();
    for to in [
        WorkItemState::Ready,
        WorkItemState::Leased,
        WorkItemState::Running,
        WorkItemState::WaitingDecision,
    ] {
        go(&mut w, &one, to);
    }
    let granted = decide(&mut w, &one, "g", "approval_needed");
    let pending = decide(&mut w, &one, "p", "ambiguity");
    let edit = decide(&mut w, &one, "e", "blocked");
    answer(&mut w, &granted, "grant_once");
    answer(&mut w, &edit, "edit_scope");
    DecisionDriver::default()
        .drive(&mut w, OffsetDateTime::now_utc())
        .unwrap();
    assert_eq!(w.list_grants(w.id()).unwrap().len(), 1);
    outcome_to(&mut w, OutcomeState::Blocked).unwrap();
    Blocked {
        w,
        one,
        granted,
        pending,
        edit,
    }
}

#[test]
fn a_revision_is_stored_unapproved_and_changes_nothing_until_it_is_approved() {
    need_git!();
    let r = rig();
    let mut b = blocked(&r);
    let w = &mut b.w;
    let rev = revision(w);
    let c = revise(&r, w, "rev-1", &widened());
    assert_eq!(c.state, CommandState::Applied, "{:?}", c.result);
    assert_eq!(c.result.as_ref().unwrap()["version"], 2);
    // v2 exists and is not approved; the outcome still stands on v1
    let (stored, hash, approved) = w.get_contract(w.id(), 2).unwrap();
    assert!(!approved);
    assert_eq!(c.result.as_ref().unwrap()["hash"], hash);
    assert_eq!(stored.version, 2);
    let o = w.get_outcome(w.id()).unwrap();
    assert_eq!((o.contract_version, o.state), (1, OutcomeState::Blocked));
    assert_eq!(o.revision, rev + 1);
    assert!(w.get_contract(w.id(), 1).unwrap().2);
    // decisions and the grant are untouched, and the old contract can still run (resume)
    assert_eq!(
        w.decision_state(&b.pending.id).unwrap(),
        DecisionState::Pending
    );
    assert!(w.usable_grants(&b.one).unwrap().len() == 1);
    outcome_to(w, OutcomeState::Running).unwrap();
}

#[test]
fn approving_the_revision_rebinds_kills_old_decisions_and_grants_and_replans() {
    need_git!();
    let r = rig();
    let mut b = blocked(&r);
    let w = &mut b.w;
    assert_eq!(
        revise(&r, w, "rev-1", &widened()).state,
        CommandState::Applied
    );
    let a = approve(&r, w, "ap-1", 2);
    assert_eq!(a.state, CommandState::Applied, "{:?}", a.result);
    let o = w.get_outcome(w.id()).unwrap();
    assert_eq!((o.contract_version, o.state), (2, OutcomeState::Blocked));
    // the old action can never be granted: every open decision of v1 is invalidated, the unused
    // grant of the answered one is revoked (an answered decision stays as the record it is)
    assert_eq!(
        w.decision_state(&b.granted.id).unwrap(),
        DecisionState::Approved
    );
    for d in [&b.pending, &b.edit] {
        assert_eq!(
            w.decision_state(&d.id).unwrap(),
            DecisionState::Invalidated,
            "{}",
            d.kind
        );
    }
    let g = &w.list_grants(w.id()).unwrap()[0];
    assert!(g.revoked_at.is_some());
    assert!(w.usable_grants(&b.one).unwrap().is_empty());
    // the old plan does not fit the new contract: the outcome cannot run until it is replanned
    assert!(awaiting_replan(w, w.id()).unwrap());
    assert!(outcome_to(w, OutcomeState::Running).is_err());
    assert!(replan_after_revise(w, &StubPlanner).unwrap());
    let o = w.get_outcome(w.id()).unwrap();
    assert_eq!((o.state, o.plan_version), (OutcomeState::Running, Some(2)));
    assert_eq!(w.get_plan(w.id(), 2).unwrap().contract_version, 2);
    assert!(!awaiting_replan(w, w.id()).unwrap());
    // once only
    assert!(!replan_after_revise(w, &StubPlanner).unwrap());
}

#[test]
fn an_answer_to_a_decision_of_the_old_version_is_stale_after_the_rebind() {
    need_git!();
    let r = rig();
    let mut b = blocked(&r);
    let w = &mut b.w;
    revise(&r, w, "rev-1", &widened());
    approve(&r, w, "ap-1", 2);
    let d = find(w, &b.pending.id);
    queue(
        &r,
        w,
        "late",
        "decision.resolve",
        json!({"decision_id": d.id, "choice": "grant_once", "brief_sha256": d.brief_sha256}),
        Some(d.revision),
    );
    apply_queued(w, OffsetDateTime::now_utc()).unwrap();
    assert_eq!(cmd(w, "late").state, CommandState::Stale);
    assert_eq!(w.list_grants(w.id()).unwrap().len(), 1, "no new grant");
    assert_eq!(w.decision_state(&d.id).unwrap(), DecisionState::Invalidated);
}

#[test]
fn only_a_blocked_outcome_is_revised_and_a_revision_must_change_something() {
    need_git!();
    let r = rig();
    // running
    let (mut w, _) = r.fresh();
    let c = revise(&r, &mut w, "rev-run", &widened());
    assert_eq!(c.state, CommandState::Denied, "{:?}", c.result);
    assert!(
        c.result.as_ref().unwrap()["reason"]
            .as_str()
            .unwrap()
            .contains("blocked")
    );
    assert!(w.get_contract(w.id(), 2).is_err());
    // blocked, but no change
    outcome_to(&mut w, OutcomeState::Blocked).unwrap();
    let c = revise(&r, &mut w, "rev-same", &contract());
    assert_eq!(c.state, CommandState::Denied);
    assert!(
        c.result.as_ref().unwrap()["reason"]
            .as_str()
            .unwrap()
            .contains("changes nothing")
    );
    // a stale revision is stale, and a missing one is denied
    queue(
        &r,
        &w,
        "rev-stale",
        "outcome.contract.revise",
        json!({"contract": serde_json::to_value(widened()).unwrap()}),
        Some(1),
    );
    queue(
        &r,
        &w,
        "rev-norev",
        "outcome.contract.revise",
        json!({"contract": serde_json::to_value(widened()).unwrap()}),
        None,
    );
    queue(
        &r,
        &w,
        "rev-junk",
        "outcome.contract.revise",
        json!({"contract": {"goal": "x"}}),
        Some(revision(&w)),
    );
    apply_queued(&mut w, OffsetDateTime::now_utc()).unwrap();
    assert_eq!(cmd(&w, "rev-stale").state, CommandState::Stale);
    assert_eq!(cmd(&w, "rev-norev").state, CommandState::Denied);
    assert_eq!(cmd(&w, "rev-junk").state, CommandState::Denied);
    assert!(w.get_contract(w.id(), 2).is_err(), "nothing was stored");
}

#[test]
fn an_invalid_revision_is_refused_and_stores_nothing() {
    need_git!();
    let r = rig();
    let mut b = blocked(&r);
    let mut bad = widened();
    bad.criteria.clear();
    assert_eq!(
        revise(&r, &mut b.w, "bad-1", &bad).state,
        CommandState::Denied
    );
    assert!(b.w.get_contract(b.w.id(), 2).is_err());
}

#[test]
fn a_revision_is_approved_only_while_blocked_and_only_the_newest_version() {
    need_git!();
    let r = rig();
    let mut b = blocked(&r);
    let w = &mut b.w;
    assert_eq!(
        revise(&r, w, "rev-1", &widened()).result.as_ref().unwrap()["version"],
        2
    );
    assert_eq!(
        revise(&r, w, "rev-2", &narrowed()).result.as_ref().unwrap()["version"],
        3
    );
    // v2 is no longer the newest
    let a = approve(&r, w, "ap-2", 2);
    assert_eq!(a.state, CommandState::Denied, "{:?}", a.result);
    assert!(w.get_contract(w.id(), 2).map(|c| !c.2).unwrap());
    // the person resumes under the old contract (still approved): the draft can no longer be approved
    outcome_to(w, OutcomeState::Running).unwrap();
    let a = approve(&r, w, "ap-3", 3);
    assert_eq!(a.state, CommandState::Denied);
    assert!(
        a.result.as_ref().unwrap()["reason"]
            .as_str()
            .unwrap()
            .contains("blocked")
    );
    assert_eq!(w.get_outcome(w.id()).unwrap().contract_version, 1);
    // a wrong hash is refused too
    outcome_to(w, OutcomeState::Blocked).unwrap();
    queue(
        &r,
        w,
        "ap-bad",
        "outcome.contract.approve",
        json!({"version": 3, "hash": "0".repeat(64)}),
        None,
    );
    apply_queued(w, OffsetDateTime::now_utc()).unwrap();
    assert_eq!(cmd(w, "ap-bad").state, CommandState::Denied);
    assert_eq!(approve(&r, w, "ap-ok", 3).state, CommandState::Applied);
    assert_eq!(w.get_outcome(w.id()).unwrap().contract_version, 3);
}

#[test]
fn the_same_key_revises_once_and_a_cancel_wins_over_a_pending_approval() {
    need_git!();
    let r = rig();
    let mut b = blocked(&r);
    let w = &mut b.w;
    revise(&r, w, "rev-1", &widened());
    // a retry of the same key finds the stored command and stores no second version
    queue(
        &r,
        w,
        "rev-1",
        "outcome.contract.revise",
        json!({"contract": serde_json::to_value(widened()).unwrap()}),
        Some(revision(w) - 1),
    );
    assert!(w.get_contract(w.id(), 3).is_err());
    // cancel, then approve: the cancelled outcome is not rebound
    queue(&r, w, "cx", "outcome.cancel", json!({}), None);
    apply_lifecycle(w, &Control::default(), &StubPlanner).unwrap();
    assert_eq!(
        w.get_outcome(w.id()).unwrap().state,
        OutcomeState::Cancelled
    );
    let a = approve(&r, w, "ap-1", 2);
    assert_eq!(a.state, CommandState::Denied);
    assert_eq!(w.get_outcome(w.id()).unwrap().contract_version, 1);
}

#[test]
fn an_expired_edit_scope_decision_does_not_stop_a_revision() {
    need_git!();
    let r = rig();
    let b = blocked(&r);
    let mut w = b.w.with_clock(later);
    // the person's edit_scope card expires (HOLD); the revision is the person's own, separate call
    DecisionDriver::default().drive(&mut w, later()).unwrap();
    assert_eq!(
        w.decision_state(&b.edit.id).unwrap(),
        DecisionState::Expired
    );
    assert_eq!(
        revise(&r, &mut w, "rev-1", &widened()).state,
        CommandState::Applied
    );
}

#[test]
fn evidence_of_the_old_plan_does_not_count_for_the_new_contract() {
    need_git!();
    let r = rig();
    let mut b = blocked(&r);
    let w = &mut b.w;
    w.record_evidence(&NewEvidence {
        work_item_id: b.one.clone(),
        attempt_id: None,
        trust_class: TrustClass::IndependentCheck,
        producer: "independent_runner".into(),
        subject_commit: "abc".into(),
        check_id: "unit".into(),
        command_hash: None,
        criteria_ids: vec!["c1".into()],
        result: "passed".into(),
        exit_code: Some(0),
        env_fingerprint: "e".into(),
        artifact: None,
        fence: 1,
        flags: vec![],
    })
    .unwrap();
    assert_eq!(w.authentic_evidence(w.id()).unwrap().len(), 1);
    revise(&r, w, "rev-1", &widened());
    assert_eq!(approve(&r, w, "ap-1", 2).state, CommandState::Applied);
    assert!(
        w.authentic_evidence(w.id()).unwrap().is_empty(),
        "evidence of plan v1 is not evidence for contract v2"
    );
    // the row is still listed (data), it just counts for nothing
    assert_eq!(w.list_evidence(w.id()).unwrap().len(), 1);
    replan_after_revise(w, &StubPlanner).unwrap();
    assert!(w.authentic_evidence(w.id()).unwrap().is_empty());
}

#[test]
fn the_rebind_event_says_whether_the_scope_widened() {
    need_git!();
    use synthesis_events::Record;
    let r = rig();
    for (c, widens) in [(widened(), true), (narrowed(), false)] {
        let mut b = blocked(&r);
        let w = &mut b.w;
        revise(&r, w, "rev-1", &c);
        approve(&r, w, "ap-1", 2);
        let ev: Vec<Value> = w
            .read_events(w.control_session_id(), 0, 5000)
            .unwrap()
            .into_iter()
            .filter_map(|e| match e {
                Record::Unknown(raw) if raw.kind == "outcome.contract_rebound" => Some(raw.payload),
                _ => None,
            })
            .collect();
        assert_eq!(ev.len(), 1);
        assert_eq!(
            !ev[0]["widens"].as_array().unwrap().is_empty(),
            widens,
            "{}",
            ev[0]
        );
    }
}

#[test]
fn unapproved_revisions_are_capped_per_outcome() {
    need_git!();
    let r = rig();
    let mut b = blocked(&r);
    for i in 0..20 {
        let mut c = contract();
        c.envelope.budgets.max_cost_usd = 6.0 + f64::from(i);
        assert_eq!(
            revise(&r, &mut b.w, &format!("rev-{i}"), &c).state,
            CommandState::Applied,
            "{i}"
        );
    }
    let mut c = contract();
    c.envelope.budgets.max_cost_usd = 99.0;
    let d = revise(&r, &mut b.w, "rev-over", &c);
    assert_eq!(d.state, CommandState::Denied);
    assert!(
        d.result.as_ref().unwrap()["reason"]
            .as_str()
            .unwrap()
            .contains("wait for approval")
    );
}

#[test]
fn the_gateway_allows_a_revision_to_a_person_only_and_audits_it() {
    need_git!();
    use synthesis_events::Record;
    let r = rig();
    let mut b = blocked(&r);
    let w = &mut b.w;
    assert_eq!(
        revise(&r, w, "rev-1", &widened()).state,
        CommandState::Applied
    );
    let audited = w
        .read_events(w.control_session_id(), 0, 5000)
        .unwrap()
        .into_iter()
        .any(|e| matches!(e, Record::Unknown(raw) if raw.kind == "policy.decision"
            && raw.payload["action"] == "outcome.contract.revise" && raw.payload["result"] == "allow"));
    assert!(audited);
}
