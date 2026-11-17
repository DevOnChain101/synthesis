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

#[test]
fn an_answer_for_an_older_binding_is_not_an_answer_waiting_for_the_owner_to_unblock() {
    need_git!();
    let r = rig();
    let (mut w, it) = waiting(&r, &contract());
    let ctx = ctx_of(&w);
    let d = decide(
        &mut w,
        &it,
        "a",
        "ambiguity",
        binding(&ctx, &it, |_| {}),
        json!({"question": "Which API shape?", "options": ["REST"]}),
        2,
    );
    assert!(
        answered_waiting(&mut w).unwrap().is_empty(),
        "not answered yet"
    );
    answer(&mut w, &d, "REST");
    assert_eq!(answered_waiting(&mut w).unwrap(), [d.id.to_string()]);
    // the plan moved after the answer: the answer no longer fits what is blocked
    e_rewrite_binding(&r, &w, &d);
    assert!(answered_waiting(&mut w).unwrap().is_empty());
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

// ---- re-briefing after `request_evidence` (ADR-019 section 4a)

fn find(w: &OutcomeWriter, id: &DecisionId) -> DecisionRecord {
    w.list_decisions(w.id())
        .unwrap()
        .into_iter()
        .find(|d| d.id == *id)
        .unwrap()
}

fn open_of(w: &OutcomeWriter) -> Option<DecisionRecord> {
    w.list_decisions(w.id())
        .unwrap()
        .into_iter()
        .find(|d| d.row_state == DecisionState::Pending)
}

#[test]
fn rebrief_keeps_action_hash_and_grant_rules() {
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
    answer(&mut w, &d, "request_evidence");
    assert_eq!(state(&w, &d), DecisionState::NeedsMoreInfo);
    let mut driver = DecisionDriver::default();
    let notes = driver.drive(&mut w, OffsetDateTime::now_utc()).unwrap();
    assert!(notes.iter().any(|n| n.contains("re-briefed")), "{notes:?}");
    // idempotent: another tick (or a restart's fresh driver) adds nothing
    DecisionDriver::default()
        .drive(&mut w, OffsetDateTime::now_utc())
        .unwrap();
    assert_eq!(w.list_decisions(w.id()).unwrap().len(), 2);
    assert_eq!(state(&w, &d), DecisionState::Invalidated);
    let n = open_of(&w).unwrap();
    assert_eq!(n.brief["supersedes"], json!(d.id));
    assert_eq!(n.brief["rebrief"], 1);
    assert_eq!(n.brief["evidence_summary"]["rows"], json!([]));
    assert_eq!(
        n.binding_hash, d.binding_hash,
        "same stored action, same binding"
    );
    assert_eq!(n.binding["action_hash"], d.binding["action_hash"]);
    assert_eq!(
        n.expires_at, d.expires_at,
        "the expiry is carried over, not extended"
    );
    assert_ne!(n.brief_sha256, d.brief_sha256);
    assert_eq!(
        n.brief["alternatives"], d.brief["alternatives"],
        "dec-rebrief-cannot-widen"
    );
    assert_eq!(
        w.decision_action(&n.id).unwrap(),
        w.decision_action(&d.id).unwrap()
    );
    // the old card cannot be answered (nor re-briefed again); the answer to the new one grants the same rules
    assert!(
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
        .is_err()
    );
    assert!(
        w.rebrief_decision(&d.id, d.revision + 1, &d.binding, &json!({}))
            .is_err()
    );
    assert!(w.list_grants(w.id()).unwrap().is_empty());
    answer(&mut w, &n, "grant_once");
    driver.drive(&mut w, OffsetDateTime::now_utc()).unwrap();
    let grants = w.list_grants(w.id()).unwrap();
    assert_eq!(grants.len(), 1);
    assert_eq!(grants[0].rules, ["Edit(/src/new.txt)"]);
    assert_eq!(
        grants[0].action_hash,
        d.binding["action_hash"].as_str().unwrap()
    );
    assert_eq!(grants[0].decision_id, n.id);
}

#[test]
fn rebrief_shows_only_event_backed_evidence_rows_of_the_item() {
    need_git!();
    let r = rig();
    let (mut w, it) = waiting(&r, &contract());
    let ctx = ctx_of(&w);
    w.record_evidence(&NewEvidence {
        work_item_id: it.clone(),
        attempt_id: None,
        trust_class: TrustClass::IndependentCheck,
        producer: "independent_runner".into(),
        subject_commit: "abc".into(),
        check_id: "unit".into(),
        command_hash: None,
        criteria_ids: vec![],
        result: "failed".into(),
        exit_code: Some(1),
        env_fingerprint: "e".into(),
        artifact: None,
        fence: 1,
        flags: vec![],
    })
    .unwrap();
    let d = decide(
        &mut w,
        &it,
        "b",
        "blocked",
        binding(&ctx, &it, |_| {}),
        action(&[]),
        2,
    );
    answer(&mut w, &d, "request_evidence");
    DecisionDriver::default()
        .drive(&mut w, OffsetDateTime::now_utc())
        .unwrap();
    let rows = &open_of(&w).unwrap().brief["evidence_summary"]["rows"];
    assert_eq!(rows[0]["check"], "unit");
    assert_eq!(rows[0]["result"], "failed");
    assert_eq!(rows.as_array().unwrap().len(), 1);
}

#[test]
fn request_evidence_is_bounded_and_the_last_card_must_be_answered() {
    need_git!();
    let r = rig();
    let (mut w, it) = waiting(&r, &contract());
    let ctx = ctx_of(&w);
    let mut d = decide(
        &mut w,
        &it,
        "g",
        "approval_needed",
        binding(&ctx, &it, |_| {}),
        action(&["Edit(/a)"]),
        2,
    );
    for i in 1..=MAX_REBRIEFS {
        answer(&mut w, &d, "request_evidence");
        DecisionDriver::default()
            .drive(&mut w, OffsetDateTime::now_utc())
            .unwrap();
        d = open_of(&w).unwrap();
        assert_eq!(d.brief["rebrief"], i);
    }
    assert_eq!(d.brief["rebriefs_left"], 0);
    let refused = w.resolve_decision(
        &Resolution {
            decision_id: d.id.clone(),
            decision_revision: d.revision,
            approver: "user:cli".into(),
            choice: "request_evidence".into(),
            brief_sha256: d.brief_sha256.clone(),
        },
        &d.binding,
    );
    assert!(
        matches!(refused, Err(synthesis_session::Error::Invalid(_))),
        "{refused:?}"
    );
    assert_eq!(state(&w, &d), DecisionState::Pending, "nothing was written");
    assert_eq!(w.decision_approvals(&d.id).unwrap().len(), 0);
}

// answer vs supersede vs expiry vs invalidation: one of them commits first, the others are refused
#[test]
fn rebrief_loses_to_expiry_and_to_invalidation_and_to_a_lost_revision_race() {
    need_git!();
    let r = rig();
    // expiry first
    let (mut w, it) = waiting(&r, &contract());
    let ctx = ctx_of(&w);
    let d = decide(
        &mut w,
        &it,
        "e",
        "approval_needed",
        binding(&ctx, &it, |_| {}),
        action(&["Edit(/a)"]),
        1,
    );
    answer(&mut w, &d, "request_evidence");
    let d = find(&w, &d.id);
    let mut w = w.with_clock(later);
    assert!(matches!(
        w.rebrief_decision(&d.id, d.revision, &d.binding, &json!({})),
        Err(synthesis_session::Error::Stale(_))
    ));
    DecisionDriver::default().drive(&mut w, later()).unwrap();
    assert_eq!(state(&w, &d), DecisionState::Expired);
    assert_eq!(
        w.list_decisions(w.id()).unwrap().len(),
        1,
        "no successor, HOLD"
    );

    // invalidation first: the stored binding no longer matches the outcome
    let (mut w, it) = waiting(&r, &contract());
    let ctx = ctx_of(&w);
    let d = decide(
        &mut w,
        &it,
        "i",
        "approval_needed",
        binding(&ctx, &it, |_| {}),
        action(&["Edit(/a)"]),
        2,
    );
    answer(&mut w, &d, "request_evidence");
    e_rewrite_binding(&r, &w, &d);
    DecisionDriver::default()
        .drive(&mut w, OffsetDateTime::now_utc())
        .unwrap();
    assert_eq!(state(&w, &d), DecisionState::Invalidated);
    assert_eq!(w.list_decisions(w.id()).unwrap().len(), 1, "no successor");

    // a stale revision loses; a decision nobody asked evidence for is not re-briefed
    let (mut w, it) = waiting(&r, &contract());
    let ctx = ctx_of(&w);
    let d = decide(
        &mut w,
        &it,
        "c",
        "approval_needed",
        binding(&ctx, &it, |_| {}),
        action(&["Edit(/a)"]),
        2,
    );
    assert!(
        w.rebrief_decision(&d.id, d.revision, &d.binding, &json!({}))
            .is_err(),
        "pending"
    );
    answer(&mut w, &d, "edit_scope");
    let d = find(&w, &d.id);
    assert!(
        w.rebrief_decision(&d.id, d.revision, &d.binding, &json!({}))
            .is_err(),
        "edit_scope"
    );
    DecisionDriver::default()
        .drive(&mut w, OffsetDateTime::now_utc())
        .unwrap();
    assert_eq!(
        state(&w, &d),
        DecisionState::NeedsMoreInfo,
        "edit_scope is left as it is"
    );
    assert_eq!(w.list_decisions(w.id()).unwrap().len(), 1);
}

#[test]
fn the_rebrief_summary_shows_the_newest_twenty_rows_and_always_fits() {
    need_git!();
    let r = rig();
    let (mut w, it) = waiting(&r, &contract());
    let ctx = ctx_of(&w);
    for i in 0..25 {
        w.record_evidence(&NewEvidence {
            work_item_id: it.clone(),
            attempt_id: None,
            trust_class: TrustClass::IndependentCheck,
            producer: "independent_runner".into(),
            subject_commit: "abc".into(),
            check_id: format!("check-{i:02}-{}", "x".repeat(100)),
            command_hash: None,
            criteria_ids: vec![],
            result: "failed".into(),
            exit_code: Some(1),
            env_fingerprint: "e".into(),
            artifact: None,
            fence: 1,
            flags: vec![
                "definitions_changed".into(),
                "tests_weakened".into(),
                "outside_envelope".into(),
                "mode_changed".into(),
            ],
        })
        .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(3));
    }
    let d = decide(
        &mut w,
        &it,
        "b",
        "blocked",
        binding(&ctx, &it, |_| {}),
        action(&[]),
        2,
    );
    answer(&mut w, &d, "request_evidence");
    DecisionDriver::default()
        .drive(&mut w, OffsetDateTime::now_utc())
        .unwrap();
    let rows = open_of(&w).unwrap().brief["evidence_summary"]["rows"].clone();
    let rows = rows.as_array().unwrap();
    assert_eq!(rows.len(), 20);
    assert!(
        rows[0]["check"].as_str().unwrap().starts_with("check-24"),
        "newest first"
    );
    assert!(rows[19]["check"].as_str().unwrap().starts_with("check-05"));
}

#[test]
fn a_summary_of_four_byte_characters_still_fits_and_a_waiting_rebrief_is_found_by_a_reader() {
    need_git!();
    let r = rig();
    let (mut w, it) = waiting(&r, &contract());
    let ctx = ctx_of(&w);
    for i in 0..25 {
        w.record_evidence(&NewEvidence {
            work_item_id: it.clone(),
            attempt_id: None,
            trust_class: TrustClass::IndependentCheck,
            producer: "independent_runner".into(),
            subject_commit: "abc".into(),
            check_id: format!("c{i:02}"),
            command_hash: None,
            criteria_ids: vec![],
            result: "failed".into(),
            exit_code: Some(1),
            env_fingerprint: "e".into(),
            artifact: None,
            fence: 1,
            flags: vec![],
        })
        .unwrap();
    }
    let d = decide(
        &mut w,
        &it,
        "b",
        "blocked",
        binding(&ctx, &it, |_| {}),
        action(&[]),
        2,
    );
    answer(&mut w, &d, "request_evidence");
    // a Reader (what the owner's adoption pass has) sees the card waiting
    let reader = synthesis_session::Reader::open(&r.home).unwrap();
    assert!(awaiting_rebrief(&reader, w.id()).unwrap());
    DecisionDriver::default()
        .drive(&mut w, OffsetDateTime::now_utc())
        .unwrap();
    assert!(!awaiting_rebrief(&reader, w.id()).unwrap());
    assert!(
        open_of(&w).unwrap().brief["evidence_summary"]
            .to_string()
            .len()
            < 16 * 1024
    );
}
