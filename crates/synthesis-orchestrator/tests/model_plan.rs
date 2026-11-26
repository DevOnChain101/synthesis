//! A plan a model wrote (B6): the validator against hostile output, and the `plan_approval`
//! decision that is the only way such a plan is installed.

#[macro_use]
mod common;
use common::*;

use std::process::Command;

use serde_json::{Value, json};
use synthesis_core::WorkspaceContext;
use synthesis_core::outcome::*;
use synthesis_events::Record;
use synthesis_orchestrator::*;
use synthesis_session::outcome::*;
use time::OffsetDateTime;

fn limits() -> ModelPlanLimits {
    ModelPlanLimits {
        version: 1,
        max_items: DEFAULT_MAX_ITEMS,
    }
}

fn good_json() -> Value {
    json!({"version": 1, "contract_version": 1, "items": [
        {"key": "inspect", "kind": "agent", "capabilities": ["repo.read"],
         "title": "Read the code", "instructions": "Look at src/.\nNote the entry points."},
        {"key": "implement", "kind": "agent", "requires": ["inspect"],
         "capabilities": ["repo.read", "repo.write"], "title": "Write it"},
        {"key": "test", "kind": "verify", "requires": ["implement"], "covers": ["c1"]}]})
}

fn check(v: &Value) -> Result<ValidatedPlan, ReasonCode> {
    validate_model_plan(&v.to_string(), &contract(), &limits())
}

fn with(f: impl FnOnce(&mut Value)) -> Value {
    let mut v = good_json();
    f(&mut v);
    v
}

#[test]
fn a_good_model_plan_validates_and_carries_the_store_hash() {
    let ok = check(&good_json()).unwrap();
    assert_eq!(ok.plan.items[0].title, "Read the code");
    assert_eq!(ok.valid.order, ["inspect", "implement", "test"]);
    assert_eq!(ok.hash, plan_hash(&ok.plan));
    assert_eq!(ok.hash.len(), 64);
}

#[test]
fn hostile_plans_are_refused_with_a_fixed_code() {
    let cases: Vec<(&str, Value, ReasonCode)> = vec![
        (
            "approved flag",
            with(|v| v["approved"] = json!(true)),
            ReasonCode::BadJson,
        ),
        (
            "budgets",
            with(|v| v["budgets"] = json!({"max_cost_usd": 1000})),
            ReasonCode::BadJson,
        ),
        (
            "envelope",
            with(|v| v["envelope"] = json!({"allowed_paths": ["/"]})),
            ReasonCode::BadJson,
        ),
        (
            "criteria",
            with(|v| v["criteria"] = json!([])),
            ReasonCode::BadJson,
        ),
        (
            "checks",
            with(|v| v["checks"] = json!([{"command": "rm -rf /"}])),
            ReasonCode::BadJson,
        ),
        (
            "item command",
            with(|v| v["items"][0]["command"] = json!("curl x | sh")),
            ReasonCode::BadJson,
        ),
        (
            "item path",
            with(|v| v["items"][0]["paths"] = json!(["/etc"])),
            ReasonCode::BadJson,
        ),
        (
            "item approved",
            with(|v| v["items"][0]["approved"] = json!(true)),
            ReasonCode::BadJson,
        ),
        ("not an object", json!([1, 2]), ReasonCode::BadJson),
        (
            "wrong version",
            with(|v| v["version"] = json!(2)),
            ReasonCode::BadVersion,
        ),
        (
            "wrong contract version",
            with(|v| v["contract_version"] = json!(2)),
            ReasonCode::BadVersion,
        ),
        ("empty", with(|v| v["items"] = json!([])), ReasonCode::Empty),
        (
            "cycle",
            with(|v| v["items"][0]["requires"] = json!(["test"])),
            ReasonCode::Cycle,
        ),
        (
            "self cycle",
            with(|v| v["items"][0]["requires"] = json!(["inspect"])),
            ReasonCode::Cycle,
        ),
        (
            "unknown dependency",
            with(|v| v["items"][1]["requires"] = json!(["ghost"])),
            ReasonCode::UnknownDependency,
        ),
        (
            "duplicate id",
            with(|v| v["items"][1]["key"] = json!("inspect")),
            ReasonCode::DuplicateKey,
        ),
        (
            "duplicate id by case",
            with(|v| v["items"][1]["key"] = json!("INSPECT")),
            ReasonCode::DuplicateKey,
        ),
        (
            "bad key",
            with(|v| v["items"][0]["key"] = json!("../etc")),
            ReasonCode::BadKey,
        ),
        (
            "duplicate dependency",
            with(|v| v["items"][2]["requires"] = json!(["implement", "implement"])),
            ReasonCode::DuplicateDependency,
        ),
        (
            "widening: git.push is denied by the envelope",
            with(|v| v["items"][0]["capabilities"] = json!(["git.push"])),
            ReasonCode::CapabilityDenied,
        ),
        (
            "widening: network",
            with(|v| v["items"][1]["capabilities"] = json!(["network.outbound"])),
            ReasonCode::CapabilityDenied,
        ),
        (
            "unknown capability",
            with(|v| v["items"][1]["capabilities"] = json!(["root"])),
            ReasonCode::UnknownCapability,
        ),
        (
            "uncovered criterion",
            with(|v| v["items"][2]["covers"] = json!([])),
            ReasonCode::UncoveredCriterion,
        ),
        (
            "covers a criterion that does not exist",
            with(|v| v["items"][2]["covers"] = json!(["c1", "c9"])),
            ReasonCode::UnknownCriterion,
        ),
        (
            "covered by an agent only: no independent check",
            with(|v| {
                v["items"][2]["covers"] = json!([]);
                v["items"][1]["covers"] = json!(["c1"]);
            }),
            ReasonCode::NotIndependentlyVerifiable,
        ),
        (
            "title too long",
            with(|v| v["items"][0]["title"] = json!("t".repeat(121))),
            ReasonCode::TextTooLong,
        ),
        (
            "instructions too long",
            with(|v| v["items"][0]["instructions"] = json!("i".repeat(2001))),
            ReasonCode::TextTooLong,
        ),
        (
            "a title of 200k characters",
            with(|v| v["items"][0]["title"] = json!("t".repeat(200_000))),
            ReasonCode::TextTooLong,
        ),
        (
            "a title of 300k characters",
            with(|v| v["items"][0]["title"] = json!("t".repeat(300_000))),
            ReasonCode::TooLarge,
        ),
        (
            "line break in a title",
            with(|v| v["items"][0]["title"] = json!("two\nlines")),
            ReasonCode::HiddenText,
        ),
        (
            "carriage return in instructions",
            with(|v| v["items"][0]["instructions"] = json!("a\rb")),
            ReasonCode::HiddenText,
        ),
    ];
    for (name, v, want) in cases {
        assert_eq!(check(&v).unwrap_err(), want, "{name}");
    }
    // not JSON at all
    assert_eq!(
        validate_model_plan("```json\n{}\n```", &contract(), &limits()).unwrap_err(),
        ReasonCode::BadJson
    );
    assert_eq!(
        validate_model_plan("", &contract(), &limits()).unwrap_err(),
        ReasonCode::BadJson
    );
}

#[test]
fn hidden_bidi_and_control_characters_are_refused_in_every_text_field() {
    for c in [
        '\u{202e}',
        '\u{2066}',
        '\u{200b}',
        '\u{feff}',
        '\u{2028}',
        '\u{e0041}',
        '\u{1b}',
        '\0',
        '\u{a0}',
        '\u{3164}',
        '\u{061c}',
        '\u{fe0f}',
        '\u{0600}',
        '\u{7f}',
    ] {
        for field in ["title", "instructions"] {
            let v = with(|v| v["items"][1][field] = json!(format!("do a{c}b")));
            assert_eq!(
                check(&v).unwrap_err(),
                ReasonCode::HiddenText,
                "{:x} in {field}",
                c as u32
            );
        }
    }
    // a line break and a tab are fine in instructions, nothing else
    let v = with(|v| v["items"][0]["instructions"] = json!("a\n\tb"));
    assert!(check(&v).is_ok());
    // hidden characters in a key, a dependency or a capability name hit the other checks
    let v = with(|v| v["items"][1]["key"] = json!("imp\u{202e}lement"));
    assert!(check(&v).is_err());
}

#[test]
fn size_and_item_caps_hold_for_a_runaway_plan() {
    // 10,000 items: over the byte cap, refused before anything is built from them
    let many = |n: usize| {
        json!({"version": 1, "contract_version": 1, "items":
            (0..n).map(|i| json!({"key": format!("i{i}"), "kind": "agent"})).collect::<Vec<_>>()})
    };
    assert_eq!(check(&many(10_000)).unwrap_err(), ReasonCode::TooLarge);
    // small enough for the byte cap, over the item cap
    let big = ModelPlanLimits {
        version: 1,
        max_items: 100_000,
    };
    assert_eq!(
        validate_model_plan(&many(300).to_string(), &contract(), &big).unwrap_err(),
        ReasonCode::TooManyItems,
        "the core ceiling holds whatever the setting says"
    );
    assert_eq!(check(&many(17)).unwrap_err(), ReasonCode::TooManyItems);
    let three = ModelPlanLimits {
        version: 1,
        max_items: 3,
    };
    let four = with(|v| {
        v["items"]
            .as_array_mut()
            .unwrap()
            .push(json!({"key": "x", "kind": "agent"}))
    });
    assert_eq!(
        validate_model_plan(&four.to_string(), &contract(), &three).unwrap_err(),
        ReasonCode::TooManyItems
    );
}

#[test]
fn a_chain_deeper_than_the_cap_is_refused() {
    let chain = |n: usize| {
        let mut items: Vec<Value> = (0..n)
            .map(|i| {
                let mut it = json!({"key": format!("s{i:02}"), "kind": "agent"});
                if i > 0 {
                    it["requires"] = json!([format!("s{:02}", i - 1)]);
                }
                it
            })
            .collect();
        items.push(json!({"key": "zz", "kind": "verify", "requires": [format!("s{:02}", n - 1)], "covers": ["c1"]}));
        json!({"version": 1, "contract_version": 1, "items": items})
    };
    let l = ModelPlanLimits {
        version: 1,
        max_items: 64,
    };
    let run = |n| validate_model_plan(&chain(n).to_string(), &contract(), &l);
    assert!(run(MAX_PLAN_DEPTH - 1).is_ok());
    assert_eq!(run(MAX_PLAN_DEPTH).unwrap_err(), ReasonCode::TooDeep);
}

#[test]
fn a_read_only_envelope_never_widens() {
    let mut c = contract();
    c.envelope.capabilities.remove(&Capability::RepoWrite);
    let r = validate_model_plan(&good_json().to_string(), &c, &limits());
    assert_eq!(r.unwrap_err(), ReasonCode::CapabilityDenied);
}

// ---------------------------------------------------------------- the decision

/// An outcome in `contract_pending` with contract v1 approved and no plan.
fn pending(r: &Rig) -> OutcomeWriter {
    let ws = WorkspaceContext::open("w", &r.repo, &r.repo, &[], r.base.join("cart"), "pg").unwrap();
    let mut w = r
        .store
        .create_outcome(NewOutcome {
            workspace: &ws,
            base_commit: r.head.clone(),
            contract: &contract(),
            policy: &policy(),
        })
        .unwrap();
    let rev = w.get_outcome(w.id()).unwrap().revision;
    w.transition_outcome(
        rev,
        OutcomeState::ContractPending,
        None,
        &Effects::default(),
    )
    .unwrap();
    let h = w.get_contract(w.id(), 1).unwrap().1;
    w.approve_contract(1, &h, "user").unwrap();
    w
}

fn proposal(w: &mut OutcomeWriter, run: &str) -> DecisionRecord {
    let v = check(&good_json()).unwrap();
    record_plan_approval(w, run, &v, OffsetDateTime::now_utc()).unwrap()
}

fn answer(
    w: &mut OutcomeWriter,
    d: &DecisionRecord,
    choice: &str,
) -> synthesis_session::Result<DecisionState> {
    w.resolve_decision(
        &Resolution {
            decision_id: d.id.clone(),
            decision_revision: d.revision,
            approver: "user:cli".into(),
            choice: choice.into(),
            brief_sha256: d.brief_sha256.clone(),
            audit: Default::default(),
        },
        &plan_binding_for(w, d).unwrap(),
    )
}

fn plans_stored(r: &Rig) -> i64 {
    rusqlite::Connection::open(r.home.join("synt.db"))
        .unwrap()
        .query_row("SELECT COUNT(*) FROM plans", [], |x| x.get(0))
        .unwrap()
}

fn policy_events(w: &OutcomeWriter, action: &str) -> Vec<Value> {
    w.read_events(w.control_session_id(), 0, 5000)
        .unwrap()
        .into_iter()
        .filter_map(|r| match r {
            Record::Unknown(raw) if raw.kind == "policy.decision" => Some(raw.payload),
            _ => None,
        })
        .filter(|p| p["action"] == action)
        .collect()
}

fn drive(w: &mut OutcomeWriter, d: &mut DecisionDriver) -> Vec<String> {
    d.drive(w, OffsetDateTime::now_utc()).unwrap()
}

#[test]
fn the_brief_shows_everything_and_preselects_nothing() {
    need_git!();
    let r = rig();
    let mut w = pending(&r);
    let d = proposal(&mut w, "run-1");
    assert_eq!(d.kind, "plan_approval");
    assert_eq!(w.decision_state(&d.id).unwrap(), DecisionState::Pending);
    let ids: Vec<&str> = d.brief["alternatives"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| a["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, ["approve_model_plan", "use_stub_plan"]);
    for k in ["selected", "recommended", "default", "preselected"] {
        assert!(d.brief.get(k).is_none(), "{k}");
    }
    let labels = d.brief["alternatives"].to_string();
    assert!(
        labels.contains("install exactly the model's plan (hash "),
        "{labels}"
    );
    let plan = &d.brief["plan"];
    assert_eq!(plan["items"].as_array().unwrap().len(), 3);
    assert_eq!(
        plan["items"][0]["instructions"],
        "Look at src/.\nNote the entry points."
    );
    assert_eq!(
        plan["items"][1]["capabilities"],
        json!(["repo.read", "repo.write"])
    );
    assert_eq!(plan["agent_items"], 2);
    assert!(plan["diff_vs_stub_plan"]["diff"]["changed"].is_array());
    assert!(plan["diff_vs_current_plan"].is_null(), "no plan yet");
    // the stored action IS the plan, and the binding names its hash
    let action = w.decision_action(&d.id).unwrap().unwrap();
    assert_eq!(action["plan"]["items"][0]["key"], "inspect");
    assert!(action["stub_plan"].is_object());
    assert_eq!(d.binding["plan_version"], Value::Null);
    assert!(d.binding["action_hash"].as_str().unwrap().len() == 64);
}

#[test]
fn approving_installs_exactly_the_stored_plan_once() {
    need_git!();
    let r = rig();
    let mut w = pending(&r);
    let d = proposal(&mut w, "run-1");
    let mut driver = DecisionDriver::default();
    assert!(drive(&mut w, &mut driver).is_empty());
    assert_eq!(plans_stored(&r), 0, "nothing is installed before an answer");
    answer(&mut w, &d, "approve_model_plan").unwrap();
    let notes = drive(&mut w, &mut driver);
    assert!(
        notes.iter().any(|n| n.contains("plan v1 installed")),
        "{notes:?}"
    );
    let stored = w.get_plan(w.id(), 1).unwrap();
    assert_eq!(stored, check(&good_json()).unwrap().plan);
    assert_eq!(stored.items[0].title, "Read the code");
    assert_eq!(w.get_outcome(w.id()).unwrap().plan_version, Some(1));
    // the hash the store computed is the hash of the validated plan
    let h: String = rusqlite::Connection::open(r.home.join("synt.db"))
        .unwrap()
        .query_row("SELECT hash FROM plans WHERE version=1", [], |x| x.get(0))
        .unwrap();
    assert_eq!(h, plan_hash(&stored));
    // the install went through the gateway and was audited
    let a = policy_events(&w, "plan.install");
    assert_eq!(a.len(), 1);
    assert_eq!(a[0]["result"], "allow");
    assert_eq!(a[0]["detail"]["gateway"]["principal"], "orchestrator");
    // a second pass installs nothing and says nothing
    assert!(drive(&mut w, &mut driver).is_empty());
    assert_eq!(plans_stored(&r), 1);
}

#[test]
fn the_stub_alternative_installs_the_stub_plan_not_the_model_plan() {
    need_git!();
    let r = rig();
    let mut w = pending(&r);
    let d = proposal(&mut w, "run-1");
    answer(&mut w, &d, "use_stub_plan").unwrap();
    drive(&mut w, &mut DecisionDriver::default());
    let stored = w.get_plan(w.id(), 1).unwrap();
    let (stub, _) = make_plan(&StubPlanner, &contract(), 1).unwrap();
    assert_eq!(stored, stub);
}

#[test]
fn nothing_installs_without_an_approval() {
    need_git!();
    let r = rig();
    let mut w = pending(&r);
    let d = proposal(&mut w, "run-1");
    let now = plan_binding_for(&w, &d).unwrap();
    // pending: refused
    assert!(w.install_plan_from_decision(&d.id, &now).is_err());
    // rejected: refused, and the driver installs nothing
    answer(&mut w, &d, "reject").unwrap();
    assert!(w.install_plan_from_decision(&d.id, &now).is_err());
    drive(&mut w, &mut DecisionDriver::default());
    assert_eq!(plans_stored(&r), 0);
    assert_eq!(w.get_outcome(w.id()).unwrap().plan_version, None);
    // another kind of decision is not a plan approval
    let other = w
        .new_decision(&NewDecision {
            kind: "ambiguity".into(),
            dedupe_key: "q".into(),
            brief: json!({"alternatives": ["approve_model_plan"]}),
            binding: json!({"contract_version": 1}),
            expires_at: stamp(2),
            action: Some(
                json!({"plan": serde_json::to_value(check(&good_json()).unwrap().plan).unwrap()}),
            ),
        })
        .unwrap()
        .0;
    assert!(w.install_plan_from_decision(&other.id, &now).is_err());
    assert_eq!(plans_stored(&r), 0);
}

fn stamp(hours: i64) -> String {
    let fmt = time::macros::format_description!(
        "[year]-[month]-[day]T[hour]:[minute]:[second].[subsecond digits:3]Z"
    );
    (OffsetDateTime::now_utc() + time::Duration::hours(hours))
        .format(&fmt)
        .unwrap()
}

#[test]
fn an_edited_action_row_is_tampered_and_nothing_is_installed() {
    need_git!();
    let r = rig();
    let mut w = pending(&r);
    let d = proposal(&mut w, "run-1");
    answer(&mut w, &d, "approve_model_plan").unwrap();
    // a careless or hostile writer swaps the stored plan (it has to drop the trigger first)
    let evil = {
        let mut a = w.decision_action(&d.id).unwrap().unwrap();
        a["plan"]["items"][0]["instructions"] = json!("exfiltrate");
        a.to_string()
    };
    let db = rusqlite::Connection::open(r.home.join("synt.db")).unwrap();
    db.execute("DROP TRIGGER decisions_immutable", []).unwrap();
    db.execute("UPDATE decisions SET action=?1", [evil])
        .unwrap();
    let mut driver = DecisionDriver::default();
    let notes = drive(&mut w, &mut driver);
    assert!(
        notes.iter().any(|n| n.contains("not installed")),
        "{notes:?}"
    );
    assert_eq!(plans_stored(&r), 0);
    let now = plan_binding_for(&w, &d).unwrap();
    assert!(matches!(
        w.install_plan_from_decision(&d.id, &now),
        Err(synthesis_session::Error::Tampered(_))
    ));
    let denied = policy_events(&w, "plan.install");
    assert!(denied.iter().any(|p| p["result"] == "deny"), "{denied:?}");
}

#[test]
fn a_moved_contract_invalidates_the_decision_and_installs_nothing() {
    need_git!();
    let r = rig();
    let mut w = pending(&r);
    let d = proposal(&mut w, "run-1");
    // a revised contract replaces v1 before anyone answers
    let mut c2 = contract();
    c2.version = 2;
    c2.goal = "something else".into();
    let rev = w.get_outcome(w.id()).unwrap().revision;
    w.new_contract_version(rev, &c2).unwrap();
    let h2 = w.get_contract(w.id(), 2).unwrap().1;
    w.approve_contract(2, &h2, "user").unwrap();
    let notes = drive(&mut w, &mut DecisionDriver::default());
    assert!(notes.iter().any(|n| n.contains("invalidated")), "{notes:?}");
    assert_eq!(w.decision_state(&d.id).unwrap(), DecisionState::Invalidated);
    assert!(answer(&mut w, &d, "approve_model_plan").is_err());
    assert_eq!(plans_stored(&r), 0);
}

#[test]
fn an_approved_decision_whose_contract_moved_before_the_install_installs_nothing() {
    need_git!();
    let r = rig();
    let mut w = pending(&r);
    let d = proposal(&mut w, "run-1");
    answer(&mut w, &d, "approve_model_plan").unwrap();
    let mut c2 = contract();
    c2.version = 2;
    c2.goal = "something else".into();
    let rev = w.get_outcome(w.id()).unwrap().revision;
    w.new_contract_version(rev, &c2).unwrap();
    let h2 = w.get_contract(w.id(), 2).unwrap().1;
    w.approve_contract(2, &h2, "user").unwrap();
    drive(&mut w, &mut DecisionDriver::default());
    assert_eq!(
        plans_stored(&r),
        0,
        "an answer about the old contract installs nothing"
    );
    // and the store refuses it directly, with the binding the person saw
    assert!(w.install_plan_from_decision(&d.id, &d.binding).is_err());
    assert_eq!(plans_stored(&r), 0);
}

fn later() -> OffsetDateTime {
    OffsetDateTime::now_utc() + time::Duration::hours(25)
}

#[test]
fn a_decision_that_expires_is_a_hold_and_a_late_answer_is_refused() {
    need_git!();
    let r = rig();
    let mut w = pending(&r);
    let d = proposal(&mut w, "run-1");
    // the clock passes the expiry before the person answers
    let mut w = w.with_clock(later);
    assert!(answer(&mut w, &d, "approve_model_plan").is_err());
    let notes = DecisionDriver::default().drive(&mut w, later()).unwrap();
    assert!(notes.iter().any(|n| n.contains("expired")), "{notes:?}");
    assert_eq!(w.decision_state(&d.id).unwrap(), DecisionState::Expired);
    assert_eq!(plans_stored(&r), 0);
}

#[test]
fn an_answer_given_before_the_expiry_stands_after_it() {
    need_git!();
    let r = rig();
    let mut w = pending(&r);
    let d = proposal(&mut w, "run-1");
    answer(&mut w, &d, "approve_model_plan").unwrap();
    let mut w = w.with_clock(later);
    DecisionDriver::default().drive(&mut w, later()).unwrap();
    assert_eq!(
        plans_stored(&r),
        1,
        "the approval was valid when it was given"
    );
}

#[test]
fn request_evidence_and_edit_scope_are_not_answers_to_a_plan() {
    need_git!();
    let r = rig();
    let mut w = pending(&r);
    let d = proposal(&mut w, "run-1");
    for c in ["request_evidence", "edit_scope", "grant_once", "retry"] {
        assert!(answer(&mut w, &d, c).is_err(), "{c}");
    }
    assert_eq!(w.decision_state(&d.id).unwrap(), DecisionState::Pending);
}

#[test]
fn a_plan_approval_can_never_become_a_grant() {
    need_git!();
    let r = rig();
    let mut w = pending(&r);
    let d = proposal(&mut w, "run-1");
    answer(&mut w, &d, "approve_model_plan").unwrap();
    let b = plan_binding_for(&w, &d).unwrap();
    assert!(w.create_grant(&d.id, &b, 1).is_err());
    assert!(w.list_grants(w.id()).unwrap().is_empty());
}

#[test]
fn the_store_refuses_a_plan_approval_that_offers_more_than_its_action_holds() {
    need_git!();
    let r = rig();
    let mut w = pending(&r);
    let good = check(&good_json()).unwrap().plan;
    let mut new = |alts: Value, action: Value| {
        w.new_decision(&NewDecision {
            kind: "plan_approval".into(),
            dedupe_key: format!("k{}", alts),
            brief: json!({"alternatives": alts}),
            binding: json!({"contract_version": 1, "plan_version": null}),
            expires_at: stamp(2),
            action: Some(action),
        })
    };
    let plan = serde_json::to_value(&good).unwrap();
    // another alternative: refused
    assert!(new(json!([{"id": "grant_once"}]), json!({"plan": plan})).is_err());
    assert!(
        new(
            json!([{"id": "approve_model_plan"}, {"id": "retry"}]),
            json!({"plan": plan})
        )
        .is_err()
    );
    // an alternative whose plan is not in the action
    assert!(new(json!([{"id": "use_stub_plan"}]), json!({"plan": plan})).is_err());
    // no alternatives
    assert!(new(json!([]), json!({"plan": plan})).is_err());
    // an invalid plan: a criterion left uncovered
    let mut bad = good.clone();
    bad.items[2].covers.clear();
    assert!(new(json!([{"id": "approve_model_plan"}]), json!({"plan": bad})).is_err());
    // the wrong version
    let mut v2 = good.clone();
    v2.version = 2;
    assert!(new(json!([{"id": "approve_model_plan"}]), json!({"plan": v2})).is_err());
    // an unknown field in a stored plan
    let mut sneaky = plan.clone();
    sneaky["budgets"] = json!({"max_cost_usd": 1000});
    assert!(
        new(
            json!([{"id": "approve_model_plan"}]),
            json!({"plan": sneaky})
        )
        .is_err()
    );
    // keeping the current plan needs a current plan: the outcome has none yet
    assert!(
        new(
            json!([{"id": "approve_model_plan"}, {"id": "keep_current_plan"}]),
            json!({"plan": plan})
        )
        .is_err()
    );
    // the right shape is accepted
    assert!(new(json!([{"id": "approve_model_plan"}]), json!({"plan": plan})).is_ok());
}

#[test]
fn the_same_run_asked_twice_is_one_decision() {
    need_git!();
    let r = rig();
    let mut w = pending(&r);
    let a = proposal(&mut w, "run-1");
    let b = proposal(&mut w, "run-1");
    assert_eq!(a.id, b.id);
    assert_eq!(w.list_decisions(w.id()).unwrap().len(), 1);
}

#[test]
fn a_replan_brief_diffs_against_the_current_plan() {
    need_git!();
    let r = rig();
    let (mut w, _) = r.fresh();
    // blocked outcomes may be replanned: put it there with a plan of version 1 in place
    let rev = w.get_outcome(w.id()).unwrap().revision;
    w.transition_outcome(
        rev,
        OutcomeState::Blocked,
        Some("test"),
        &Effects::default(),
    )
    .unwrap();
    let mut v = good_json();
    v["version"] = json!(2);
    // the current plan (v1) uses no capability: a replan may not start using one (see validate_replan)
    for it in v["items"].as_array_mut().unwrap() {
        it["capabilities"] = json!([]);
    }
    let l = ModelPlanLimits {
        version: 2,
        max_items: 16,
    };
    let valid = validate_model_plan(&v.to_string(), &contract(), &l).unwrap();
    let d = record_plan_approval(&mut w, "run-2", &valid, OffsetDateTime::now_utc()).unwrap();
    let cur = &d.brief["plan"]["diff_vs_current_plan"];
    assert_eq!(cur["current_plan_version"], 1);
    assert!(
        cur["diff"]["added"]
            .as_array()
            .unwrap()
            .iter()
            .any(|k| k == "inspect")
    );
    assert_eq!(d.binding["plan_version"], 1);
    answer(&mut w, &d, "approve_model_plan").unwrap();
    drive(&mut w, &mut DecisionDriver::default());
    assert_eq!(w.get_outcome(w.id()).unwrap().plan_version, Some(2));
}
