//! Planning (plan PR-07, D13): the interface a planner implements and the deterministic stub.
//!
//! A planner is untrusted input: whatever it returns goes through [`make_plan`], which runs the
//! core validator (missing dependency, cycle, unknown or denied capability, unknown or uncovered
//! criterion, size caps) before anything is stored. The model planner will implement
//! [`Planner`] AFTER the bypass gate (D13); it is not built here. A changed plan is a new
//! version: [`replan`] returns the new plan with its diff against the old one and never touches
//! a stored record (the store keeps plans and contracts immutable).

use synthesis_core::outcome::{
    Capability, CapabilityMode, Contract, Plan, PlanDiff, PlanError, PlanItem, ValidPlan,
    WorkItemKind, diff_plans, validate_plan,
};

/// A planner that could not produce a plan (a model that failed, a refusal). The text is shown
/// to the person; it is never executed or trusted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannerError(pub String);

impl std::fmt::Display for PlannerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "the planner failed: {}", self.0)
    }
}
impl std::error::Error for PlannerError {}

pub trait Planner {
    /// A plan of `version` for `contract`. The result is a proposal: see [`make_plan`].
    fn plan(&self, contract: &Contract, version: u32) -> Result<Plan, PlannerError>;
}

/// `inspect` (agent, read-only) then `implement` (agent, write) then `test` (verify, covers every
/// criterion). Capabilities the envelope denies are left out, so a read-only contract plans an
/// `implement` that cannot write (and the validator still decides).
pub struct StubPlanner;

impl Planner for StubPlanner {
    fn plan(&self, contract: &Contract, version: u32) -> Result<Plan, PlannerError> {
        let allowed = |caps: &[Capability]| -> Vec<String> {
            caps.iter()
                .filter(|c| contract.envelope.mode(**c) != CapabilityMode::Deny)
                .map(|c| c.as_str().to_owned())
                .collect()
        };
        let item = |key: &str, kind, requires: &[&str], capabilities, covers| PlanItem {
            key: key.into(),
            kind,
            requires: requires.iter().map(|s| (*s).into()).collect(),
            capabilities,
            covers,
        };
        Ok(Plan {
            version,
            contract_version: contract.version,
            items: vec![
                item(
                    "inspect",
                    WorkItemKind::Agent,
                    &[],
                    allowed(&[Capability::RepoRead]),
                    vec![],
                ),
                item(
                    "implement",
                    WorkItemKind::Agent,
                    &["inspect"],
                    allowed(&[
                        Capability::RepoRead,
                        Capability::RepoWrite,
                        Capability::TestsRun,
                    ]),
                    vec![],
                ),
                item(
                    "test",
                    WorkItemKind::Verify,
                    &["implement"],
                    vec![],
                    contract.criteria.iter().map(|c| c.id.clone()).collect(),
                ),
            ],
        })
    }
}

/// Asks the planner and validates what it returned against the contract. The only way a plan
/// from a planner reaches the store.
pub fn make_plan(
    planner: &dyn Planner,
    contract: &Contract,
    version: u32,
) -> Result<(Plan, ValidPlan), PlanError> {
    let plan = planner
        .plan(contract, version)
        .map_err(|e| PlanError::Json(e.to_string()))?;
    let valid = validate_plan(&plan, contract)?;
    Ok((plan, valid))
}

/// A new plan version and what it changes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Replan {
    pub plan: Plan,
    pub valid: ValidPlan,
    /// Against the plan it replaces; `diff.rerun` is the impact (items that must run again).
    pub diff: PlanDiff,
}

/// Plans again for `contract` as version `old.version + 1`. Nothing is stored or changed: store
/// `replan.plan` with `OutcomeWriter::insert_plan`, which refuses anything but the next version
/// of a plan made for the approved current contract.
pub fn replan(planner: &dyn Planner, contract: &Contract, old: &Plan) -> Result<Replan, PlanError> {
    let (plan, valid) = make_plan(planner, contract, old.version + 1)?;
    let diff = diff_plans(old, &plan);
    Ok(Replan { plan, valid, diff })
}
