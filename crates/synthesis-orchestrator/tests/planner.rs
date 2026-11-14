//! The stub planner, the validation door every planner goes through, replanning as a new plan
//! version, and approval invalidation when the contract or plan changes (plan PR-07).

#[macro_use]
mod common;
use common::*;

use std::process::Command;

use synthesis_core::WorkspaceContext;
use synthesis_core::outcome::*;
use synthesis_orchestrator::*;
use synthesis_session::outcome::*;

/// A planner that returns whatever it is given (a model that misbehaves).
struct Fixed(Result<Plan, PlannerError>);
impl Planner for Fixed {
    fn plan(&self, _: &Contract, _: u32) -> Result<Plan, PlannerError> {
        self.0.clone()
    }
}

/// The stub's plan with one more step `lint` before `implement`.
struct WithLint;
impl Planner for WithLint {
    fn plan(&self, c: &Contract, v: u32) -> Result<Plan, PlannerError> {
        let mut p = StubPlanner.plan(c, v)?;
        p.items[1].requires.push("lint".into());
        p.items.push(PlanItem {
            key: "lint".into(),
            kind: WorkItemKind::Agent,
            requires: vec![],
            capabilities: vec!["repo.read".into()],
            covers: vec![],
        });
        Ok(p)
    }
}

fn names(p: &Plan) -> Vec<&str> {
    p.items.iter().map(|i| i.key.as_str()).collect()
}

#[test]
fn the_stub_plans_inspect_implement_test_deterministically_within_the_envelope() {
    let c = contract();
    let (plan, valid) = make_plan(&StubPlanner, &c, 1).unwrap();
    assert_eq!(names(&plan), ["inspect", "implement", "test"]);
    assert_eq!(valid.order, ["inspect", "implement", "test"]);
    assert_eq!(valid.coverage["c1"], ["test"]);
    assert_eq!(plan.items[0].capabilities, ["repo.read"]);
    assert_eq!(
        plan.items[1].capabilities,
        ["repo.read", "repo.write"],
        "tests.run is not in this envelope"
    );
    assert_eq!(
        plan,
        make_plan(&StubPlanner, &c, 1).unwrap().0,
        "same input, same plan"
    );
    // a contract that denies writing gets an implement step that cannot write
    let mut ro = contract();
    ro.envelope
        .capabilities
        .insert(Capability::RepoWrite, CapabilityMode::Deny);
    let (plan, _) = make_plan(&StubPlanner, &ro, 1).unwrap();
    assert_eq!(plan.items[1].capabilities, ["repo.read"]);
}

#[test]
fn whatever_a_planner_returns_is_validated_before_it_can_be_stored() {
    let c = contract();
    let good = StubPlanner.plan(&c, 1).unwrap();
    let with = |f: &dyn Fn(&mut Plan)| {
        let mut p = good.clone();
        f(&mut p);
        make_plan(&Fixed(Ok(p)), &c, 1).unwrap_err()
    };
    assert!(matches!(
        with(&|p| p.items[1].requires = vec!["ghost".into()]),
        PlanError::UnknownDependency { .. }
    ));
    assert!(matches!(
        with(&|p| p.items[0].requires = vec!["test".into()]),
        PlanError::Cycle(_)
    ));
    assert!(matches!(
        with(&|p| p.items[1].capabilities.push("repo.teleport".into())),
        PlanError::UnknownCapability { .. }
    ));
    assert!(matches!(
        with(&|p| p.items[1].capabilities.push("deploy.production".into())),
        PlanError::CapabilityDenied { .. }
    ));
    assert!(matches!(
        with(&|p| p.items[2].covers.clear()),
        PlanError::UncoveredCriterion(_)
    ));
    assert!(matches!(
        with(&|p| p.items[2].covers.push("c9".into())),
        PlanError::UnknownCriterion { .. }
    ));
    assert!(matches!(with(&|p| p.items.clear()), PlanError::Empty));
    assert!(matches!(
        make_plan(&Fixed(Err(PlannerError("model down".into()))), &c, 1).unwrap_err(),
        PlanError::Json(m) if m.contains("model down")
    ));
}

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
    go(&mut w, OutcomeState::ContractPending).unwrap();
    let h = w.get_contract(w.id(), 1).unwrap().1;
    w.approve_contract(1, &h, "user").unwrap();
    w
}

fn go(w: &mut OutcomeWriter, to: OutcomeState) -> Result<(), synthesis_session::Error> {
    let rev = w.get_outcome(w.id()).unwrap().revision;
    w.transition_outcome(rev, to, None, &Effects::default())
        .map(|_| ())
}

fn plan_row(r: &Rig, w: &OutcomeWriter, version: u32) -> (String, String) {
    rusqlite::Connection::open(r.home.join("synt.db"))
        .unwrap()
        .query_row(
            "SELECT hash,dag FROM plans WHERE outcome_id=?1 AND version=?2",
            rusqlite::params![w.id().as_str(), version],
            |x| Ok((x.get(0)?, x.get(1)?)),
        )
        .unwrap()
}

#[test]
fn a_replan_is_a_new_version_with_a_diff_and_the_old_plan_stays_as_it_was() {
    need_git!();
    let r = rig();
    let mut w = pending(&r);
    let c = contract();
    let (p1, _) = make_plan(&StubPlanner, &c, 1).unwrap();
    w.insert_plan(&p1).unwrap();
    let before = plan_row(&r, &w, 1);
    let v1_items = w.list_work_items(w.id()).unwrap();

    let re = replan(&WithLint, &c, &p1).unwrap();
    assert_eq!(re.plan.version, 2);
    assert_eq!(re.diff.added, ["lint"]);
    assert_eq!(re.diff.changed, ["implement"]);
    assert_eq!(
        re.diff.rerun,
        ["implement", "lint", "test"],
        "impact: downstream reruns"
    );
    assert_eq!(re.diff.unchanged, ["inspect"]);
    // storing it creates plan v2 and new work items; nothing of v1 is touched
    w.insert_plan(&re.plan).unwrap();
    assert_eq!(w.get_outcome(w.id()).unwrap().plan_version, Some(2));
    assert_eq!(
        plan_row(&r, &w, 1),
        before,
        "the stored plan v1 is unchanged"
    );
    let all = w.list_work_items(w.id()).unwrap();
    assert_eq!(all.len(), v1_items.len() + 4);
    for old in &v1_items {
        let now = all.iter().find(|i| i.id == old.id).unwrap();
        assert_eq!(
            (&now.state, now.revision, &now.spec),
            (&old.state, old.revision, &old.spec)
        );
    }
    // a version must follow the current one; a plan row cannot be edited behind the store's back
    let mut skip = re.plan.clone();
    skip.version = 4;
    assert!(w.insert_plan(&skip).is_err());
    assert!(w.insert_plan(&re.plan).is_err(), "v2 again");
    let db = rusqlite::Connection::open(r.home.join("synt.db")).unwrap();
    assert!(
        db.execute("UPDATE plans SET dag='{}' WHERE version=1", [])
            .is_err()
    );
    // the walk reads only the current plan's items: v1's are not leased any more
    let current: Vec<_> = all.iter().filter(|i| i.plan_version == 2).collect();
    assert_eq!(current.len(), 4);
}

#[test]
fn a_changed_contract_is_a_new_unapproved_version_and_the_old_approval_and_plan_do_not_carry_over()
{
    need_git!();
    let r = rig();
    let mut w = pending(&r);
    let c1 = contract();
    let (p1, _) = make_plan(&StubPlanner, &c1, 1).unwrap();
    w.insert_plan(&p1).unwrap();
    let (_, h1, ok1) = w.get_contract(w.id(), 1).unwrap();
    assert!(ok1);

    // a contract version must be the next one, and only before the work starts
    let mut c2 = contract();
    c2.version = 2;
    c2.goal = "a different goal".into();
    let mut skip = c2.clone();
    skip.version = 3;
    assert!(w.new_contract_version(&skip).is_err());
    w.new_contract_version(&c2).unwrap();
    let (stored, h2, ok2) = w.get_contract(w.id(), 2).unwrap();
    assert_eq!(stored.goal, "a different goal");
    assert_ne!(h1, h2, "the hash covers the text");
    assert!(!ok2, "the new version is not approved");
    assert!(
        w.get_contract(w.id(), 1).unwrap().2,
        "v1 keeps its own approval"
    );
    assert_eq!(w.get_outcome(w.id()).unwrap().contract_version, 2);

    // the approval of v1 does not cover v2: its hash is refused, the plan made for v1 does not
    // start the outcome, and no plan can be stored for an unapproved contract
    assert!(w.approve_contract(2, &h1, "user").is_err());
    assert!(go(&mut w, OutcomeState::Ready).is_err());
    let (p2, _) = make_plan(&StubPlanner, &c2, 2).unwrap();
    assert!(w.insert_plan(&p2).is_err());

    // approve the new text, plan for it, then it can start
    w.approve_contract(2, &h2, "user").unwrap();
    assert!(
        go(&mut w, OutcomeState::Ready).is_err(),
        "the plan is still the one for v1"
    );
    w.insert_plan(&p2).unwrap();
    go(&mut w, OutcomeState::Ready).unwrap();
    // from here the contract is frozen
    let mut c3 = c2.clone();
    c3.version = 3;
    assert!(w.new_contract_version(&c3).is_err());
}
