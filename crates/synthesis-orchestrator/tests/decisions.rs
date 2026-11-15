//! The decision lifecycle after a person answers (plan PR-10, ADR-019 sections 4 and 5): expiry is
//! a HOLD, a moved binding invalidates, an approved `grant_once` becomes a one-shot grant derived
//! from the stored action after the contract is checked again, and a grant never outlives or
//! exceeds what the contract allows.

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
    OffsetDateTime::now_utc() + time::Duration::hours(3)
}

fn go(w: &mut OutcomeWriter, it: &WorkItemId, to: WorkItemState) {
    let rev = w.get_work_item(it).unwrap().revision;
    w.transition_work_item(it, rev, to, &Effects::default())
        .unwrap();
}

/// A running outcome whose item `one` waits for a decision.
fn waiting(r: &Rig, contract: &Contract) -> (OutcomeWriter, WorkItemId) {
    let (mut w, items) = r.fresh_with(contract, &plan());
    let it = items.iter().find(|(k, _)| k == "one").unwrap().1.clone();
    for to in [
        WorkItemState::Ready,
        WorkItemState::Leased,
        WorkItemState::Running,
        WorkItemState::WaitingDecision,
    ] {
        go(&mut w, &it, to);
    }
    (w, it)
}

/// What the driver compares a binding with (copied out, so building bindings does not borrow the writer).
struct Ctx {
    contract_version: u32,
    plan_version: u32,
    policy: String,
}

fn ctx_of(w: &OutcomeWriter) -> Ctx {
    let o = w.get_outcome(w.id()).unwrap();
    let (c, ..) = w.get_contract(w.id(), o.contract_version).unwrap();
    Ctx {
        contract_version: o.contract_version,
        plan_version: o.plan_version.unwrap(),
        policy: compile_envelope(&c.envelope, &[]).unwrap().policy_hash(),
    }
}

/// The binding the driver will compute for an item with no finished attempt, with fields changed.
fn binding(c: &Ctx, it: &WorkItemId, tweak: impl Fn(&mut Value)) -> Value {
    let mut b = decision_binding(
        c.contract_version,
        c.plan_version,
        None,
        &c.policy,
        "",
        0,
        it,
    );
    tweak(&mut b);
    b
}

fn decide(
    w: &mut OutcomeWriter,
    it: &WorkItemId,
    key: &str,
    kind: &str,
    b: Value,
    action: Value,
    hours: i64,
) -> DecisionRecord {
    let (d, _) = w
        .new_decision(&NewDecision {
            kind: kind.into(),
            dedupe_key: key.into(),
            brief: json!({"why": "x", "delta_from_contract": "Which API shape?", "alternatives": [
                {"id": "grant_once", "label": "allow it once"}, {"id": "REST", "label": "REST \u{1b}[31m"},
                {"id": "retry", "label": "again"}], "safe_default": "HOLD"}),
            binding: b,
            expires_at: stamp(hours),
            action: Some(action),
        })
        .unwrap();
    let _ = it;
    w.transition_decision(
        &d.id,
        1,
        DecisionState::Pending,
        &d.binding,
        &Effects::default(),
    )
    .unwrap();
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

fn action(rules: &[&str]) -> Value {
    json!({"needed": ["Write"], "calls": [], "grant_rules": rules})
}

fn state(w: &OutcomeWriter, d: &DecisionRecord) -> DecisionState {
    w.decision_state(&d.id).unwrap()
}

fn policy_events(w: &OutcomeWriter) -> usize {
    use synthesis_events::Record;
    w.read_events(w.control_session_id(), 0, 5000)
        .unwrap()
        .into_iter()
        .filter(|r| matches!(r, Record::Unknown(raw) if raw.kind == "policy.decision"))
        .count()
}

#[test]
fn ac09_a_moved_binding_invalidates_a_pending_decision_and_leaves_the_others() {
    need_git!();
    let r = rig();
    let (mut w, it) = waiting(&r, &contract());
    let ctx = ctx_of(&w);
    let current = decide(
        &mut w,
        &it,
        "ok",
        "approval_needed",
        binding(&ctx, &it, |_| {}),
        action(&["Edit(/a)"]),
        2,
    );
    let policy = decide(
        &mut w,
        &it,
        "p",
        "approval_needed",
        binding(&ctx, &it, |b| b["policy_hash"] = json!("old")),
        action(&["Edit(/b)"]),
        2,
    );
    let plan = decide(
        &mut w,
        &it,
        "v",
        "approval_needed",
        binding(&ctx, &it, |b| b["plan_version"] = json!(9)),
        action(&["Edit(/c)"]),
        2,
    );
    let contract_v = decide(
        &mut w,
        &it,
        "c",
        "approval_needed",
        binding(&ctx, &it, |b| b["contract_version"] = json!(7)),
        action(&["Edit(/d)"]),
        2,
    );
    let commit = decide(
        &mut w,
        &it,
        "m",
        "approval_needed",
        binding(&ctx, &it, |b| b["subject_commit"] = json!("abc")),
        action(&["Edit(/e)"]),
        2,
    );
    let mut driver = DecisionDriver::default();
    let notes = driver.drive(&mut w, OffsetDateTime::now_utc()).unwrap();
    assert_eq!(
        notes.iter().filter(|n| n.contains("invalidated")).count(),
        4,
        "{notes:?}"
    );
    assert_eq!(
        state(&w, &current),
        DecisionState::Pending,
        "an unrelated decision is untouched"
    );
    for d in [&policy, &plan, &contract_v, &commit] {
        assert_eq!(state(&w, d), DecisionState::Invalidated);
    }
    // an invalidated decision cannot be resolved (it is not pending) and grants nothing
    let err = w
        .resolve_decision(
            &Resolution {
                decision_id: policy.id.clone(),
                decision_revision: policy.revision + 1,
                approver: "user:cli".into(),
                choice: "grant_once".into(),
                brief_sha256: policy.brief_sha256.clone(),
            },
            &policy.binding,
        )
        .unwrap_err();
    assert!(!err.to_string().is_empty());
    assert!(policy_events(&w) >= 4, "each refusal is in the audit trail");
    // a second pass changes nothing
    assert!(
        driver
            .drive(&mut w, OffsetDateTime::now_utc())
            .unwrap()
            .is_empty()
    );
}

#[test]
fn expiry_moves_a_pending_decision_to_hold_and_never_to_approved() {
    need_git!();
    let r = rig();
    let (w, it) = waiting(&r, &contract());
    let ctx = ctx_of(&w);
    let mut w = w;
    let d = decide(
        &mut w,
        &it,
        "e",
        "approval_needed",
        binding(&ctx, &it, |_| {}),
        action(&["Edit(/a)"]),
        1,
    );
    let mut w = w.with_clock(later);
    let mut driver = DecisionDriver::default();
    let notes = driver.drive(&mut w, later()).unwrap();
    assert!(notes.iter().any(|n| n.contains("expired")), "{notes:?}");
    assert_eq!(state(&w, &d), DecisionState::Expired);
    assert_eq!(
        w.get_work_item(&it).unwrap().state,
        WorkItemState::WaitingDecision,
        "on hold"
    );
    assert!(w.list_grants(w.id()).unwrap().is_empty());
    // a late answer is refused: an expired decision is never approved
    assert!(
        w.resolve_decision(
            &Resolution {
                decision_id: d.id.clone(),
                decision_revision: d.revision + 1,
                approver: "user:cli".into(),
                choice: "grant_once".into(),
                brief_sha256: d.brief_sha256.clone(),
            },
            &d.binding,
        )
        .is_err()
    );
}

#[test]
fn an_approved_grant_once_becomes_one_grant_and_the_item_runs_again() {
    need_git!();
    let r = rig();
    let (mut w, it) = waiting(&r, &contract());
    let ctx = ctx_of(&w);
    let d = decide(
        &mut w,
        &it,
        "g",
        "approval_needed",
        binding(&ctx, &it, |_| {}),
        action(&["Edit(/src/new.txt)"]),
        2,
    );
    let mut driver = DecisionDriver::default();
    assert!(
        driver
            .drive(&mut w, OffsetDateTime::now_utc())
            .unwrap()
            .is_empty(),
        "nothing before an answer"
    );
    answer(&mut w, &d, "grant_once");
    let notes = driver.drive(&mut w, OffsetDateTime::now_utc()).unwrap();
    assert!(
        notes
            .iter()
            .any(|n| n.contains("grant") && n.contains("created")),
        "{notes:?}"
    );
    let grants = w.list_grants(w.id()).unwrap();
    assert_eq!(grants.len(), 1);
    assert_eq!(grants[0].rules, ["Edit(/src/new.txt)"]);
    assert_eq!(w.get_work_item(&it).unwrap().state, WorkItemState::Ready);
    // again: still one grant, no noise
    assert!(
        driver
            .drive(&mut w, OffsetDateTime::now_utc())
            .unwrap()
            .is_empty()
    );
    assert_eq!(w.list_grants(w.id()).unwrap().len(), 1);
}

#[test]
fn a_grant_is_not_made_when_the_contract_denies_the_rule_now_and_the_refusal_is_audited_once() {
    need_git!();
    let r = rig();
    // the contract does not allow writing at all: `Edit` is denied by the compiled rules
    let mut c = contract();
    c.envelope.capabilities.remove(&Capability::RepoWrite);
    let (mut w, it) = waiting(&r, &c);
    let ctx = ctx_of(&w);
    let d = decide(
        &mut w,
        &it,
        "g",
        "approval_needed",
        binding(&ctx, &it, |_| {}),
        action(&["Edit(/src/new.txt)"]),
        2,
    );
    answer(&mut w, &d, "grant_once");
    let mut driver = DecisionDriver::default();
    let before = policy_events(&w);
    let notes = driver.drive(&mut w, OffsetDateTime::now_utc()).unwrap();
    assert!(notes.iter().any(|n| n.contains("denies")), "{notes:?}");
    assert!(w.list_grants(w.id()).unwrap().is_empty());
    assert_eq!(
        w.get_work_item(&it).unwrap().state,
        WorkItemState::WaitingDecision
    );
    assert_eq!(policy_events(&w), before + 1);
    assert!(
        driver
            .drive(&mut w, OffsetDateTime::now_utc())
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        policy_events(&w),
        before + 1,
        "reported once, not on every tick"
    );
}

#[test]
fn an_unused_grant_is_withdrawn_when_the_binding_moves_and_an_answered_choice_reaches_the_prompt_as_data()
 {
    need_git!();
    let r = rig();
    let (mut w, it) = waiting(&r, &contract());
    let ctx = ctx_of(&w);
    // a grant exists for a decision whose binding is current now
    let d = decide(
        &mut w,
        &it,
        "g",
        "approval_needed",
        binding(&ctx, &it, |_| {}),
        action(&["Edit(/a)"]),
        2,
    );
    answer(&mut w, &d, "grant_once");
    let mut driver = DecisionDriver::default();
    driver.drive(&mut w, OffsetDateTime::now_utc()).unwrap();
    let g = w.list_grants(w.id()).unwrap().remove(0);
    // the world moves under the unused grant (a replan raises the outcome's plan version)
    e_rewrite_binding(&r, &w, &d);
    let notes = driver.drive(&mut w, OffsetDateTime::now_utc()).unwrap();
    assert!(notes.iter().any(|n| n.contains("revoked")), "{notes:?}");
    assert!(w.usable_grants(&it).unwrap().is_empty());
    assert!(
        w.list_grants(w.id())
            .unwrap()
            .iter()
            .any(|x| x.id == g.id && x.revoked_at.is_some())
    );

    // an approved alternative of an ambiguity is quoted as DATA, cleaned, for the next attempt
    let (mut w, it) = waiting(&r, &contract());
    let ctx = ctx_of(&w);
    let amb = decide(
        &mut w,
        &it,
        "a",
        "ambiguity",
        binding(&ctx, &it, |_| {}),
        json!({"question": "Which API shape?", "options": ["REST"]}),
        2,
    );
    answer(&mut w, &amb, "REST");
    let lines = decided_choices(&w, &it, 1).unwrap();
    assert_eq!(
        lines,
        ["Which API shape?: REST [31m"].map(|l| l.replace('\u{1b}', ""))
    );
    assert!(
        decided_choices(&w, &it, 0).unwrap().is_empty(),
        "only decisions of earlier attempts"
    );
}

/// Makes the stored binding of a decision stale by rewriting the OUTCOME's plan version (what a
/// replan does), not the decision (its binding is immutable).
fn e_rewrite_binding(r: &Rig, w: &OutcomeWriter, _d: &DecisionRecord) {
    let _ = _d;
    let db = rusqlite::Connection::open(r.home.join("synt.db")).unwrap();
    db.execute_batch("DROP TRIGGER IF EXISTS outcomes_revision_forward")
        .unwrap();
    db.execute(
        "UPDATE outcomes SET plan_version=plan_version+1 WHERE id=?1",
        [w.id().as_str()],
    )
    .unwrap();
}

#[test]
fn a_granted_rule_must_be_exact_and_inside_the_contract() {
    need_git!();
    let r = rig();
    let (mut w, one, _) = r.outcome();
    let env = r
        .allocator()
        .allocate(&mut w, &r.spec(&one, 1), OffsetDateTime::now_utc())
        .unwrap();
    let base = || {
        Profile::build(
            std::path::PathBuf::from("synt"),
            &env,
            &r.contract_for_profile(),
            &policy(),
            0.0,
            "g".into(),
        )
        .unwrap()
    };
    let ok = base()
        .with_granted_rules(&["Edit(/src/new.txt)".into(), "Bash(cargo test)".into()])
        .unwrap();
    let argv: Vec<String> = ok
        .argv()
        .iter()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    for rule in ["Edit(/src/new.txt)", "Bash(cargo test)"] {
        assert!(
            argv.windows(2).any(|w| w[0] == "--allow" && w[1] == rule),
            "{rule}: {argv:?}"
        );
    }
    // a profile without a grant has no such flags: the grant is for this attempt only
    assert!(!base().argv().iter().any(|a| a == "Edit(/src/new.txt)"));
    for bad in [
        "Bash",                       // a bare tool: everything
        "Read(/x)",                   // not a tool a grant covers
        "Edit(/x)\u{1b}",             // control characters
        "Bash(git push)",             // the contract denies git.push (unlisted)
        "Edit",                       // not exact
        "Bash(rm -rf /",              // not a rule
        "WebFetch(domain:evil.test)", // network is denied
        "Bash(python:*)",             // a prefix, not the command
        "Bash(*)",
        "Bash(ls *)",
        "Bash(git * main)",
        "Edit(/a*)",
    ] {
        assert!(
            matches!(
                base().with_granted_rules(&[bad.into()]),
                Err(ProfileError::Forbidden(_))
            ),
            "{bad:?}"
        );
    }
    let many: Vec<String> = (0..11).map(|i| format!("Edit(/f{i})")).collect();
    assert!(base().with_granted_rules(&many).is_err());
}

#[test]
fn a_live_unused_grant_moves_a_waiting_item_to_ready_on_the_next_tick() {
    need_git!();
    let r = rig();
    let (mut w, it) = waiting(&r, &contract());
    let ctx = ctx_of(&w);
    let d = decide(
        &mut w,
        &it,
        "g",
        "approval_needed",
        binding(&ctx, &it, |_| {}),
        action(&["Edit(/src/a.txt)"]),
        2,
    );
    answer(&mut w, &d, "grant_once");
    // the owner crashed right after the grant was written, before the item was made ready
    w.create_grant(&d.id, &d.binding, 24).unwrap();
    assert_eq!(
        w.get_work_item(&it).unwrap().state,
        WorkItemState::WaitingDecision
    );
    let mut driver = DecisionDriver::default();
    let notes = driver.drive(&mut w, OffsetDateTime::now_utc()).unwrap();
    assert!(notes.iter().any(|n| n.contains("runs again")), "{notes:?}");
    assert_eq!(w.get_work_item(&it).unwrap().state, WorkItemState::Ready);
    assert_eq!(w.list_grants(w.id()).unwrap().len(), 1, "no second grant");
}
