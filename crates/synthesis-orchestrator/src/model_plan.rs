//! A plan a model wrote (plan D13, ADR-017 P-B, ADR-019): the validator that stands between model
//! output and everything else, and the `plan_approval` decision that puts the plan in front of a
//! person.
//!
//! A model chooses items, dependencies, a subset of the capabilities the envelope already allows,
//! which criteria an item covers, and the short `title` / `instructions` text of an item. It cannot
//! choose anything else: [`synthesis_core::outcome::Plan`] has no field for criteria, checks,
//! budgets, the envelope, paths or commands, and an unknown field is refused. Nothing here calls a
//! model (the planning run is a later PR); the output arrives as a string and is data.
//!
//! The plan is installed only from a decision a person approved: the decision's stored action IS
//! the plan (hash-bound by the store), and [`install_approved_plan`] reads it from there, never from
//! the caller.

use std::collections::BTreeMap;

use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use synthesis_config::is_invisible_or_blank;
use synthesis_core::outcome::{
    Contract, DecisionState, MAX_INSTRUCTIONS_CHARS, MAX_PLAN_ITEMS, MAX_PLAN_JSON_BYTES,
    MAX_TITLE_CHARS, Plan, PlanDiff, PlanError, ValidPlan, WorkItemKind, diff_plans, validate_plan,
};
use synthesis_session::Error as StoreError;
use synthesis_session::outcome::{DecisionRecord, NewDecision, OutcomeWriter};
use time::OffsetDateTime;

use crate::gateway::{Action, Principal, audit, authorize};
use crate::grants::plan_binding;
use crate::planner::{StubPlanner, make_plan};

/// Longest chain of `requires` a model plan may have.
pub const MAX_PLAN_DEPTH: usize = 32;
/// Items a model plan has when `synt.orchestrator.planner.max_items` is not set.
pub const DEFAULT_MAX_ITEMS: usize = 16;

/// What the caller requires of a model plan besides the contract.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModelPlanLimits {
    /// The version the plan must carry (the next one).
    pub version: u32,
    /// At most this many items (the core ceiling `MAX_PLAN_ITEMS` applies too).
    pub max_items: usize,
}

/// Why a model plan was refused: a fixed code, never text from the plan.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReasonCode {
    TooLarge,
    /// Not JSON, not shaped like a plan, or a field a plan does not have (`approved`, `budgets`,
    /// `command`, `paths`, ...).
    BadJson,
    BadVersion,
    ContractInvalid,
    Empty,
    TooManyItems,
    TooDeep,
    BadKey,
    DuplicateKey,
    DuplicateDependency,
    TooManyEntries,
    UnknownDependency,
    Cycle,
    UnknownCapability,
    /// A capability the envelope denies: a plan never widens the envelope.
    CapabilityDenied,
    UnknownCriterion,
    UncoveredCriterion,
    NotIndependentlyVerifiable,
    TextTooLong,
    HiddenText,
    PlannerFailed,
}

impl ReasonCode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::TooLarge => "too_large",
            Self::BadJson => "bad_json",
            Self::BadVersion => "bad_version",
            Self::ContractInvalid => "contract_invalid",
            Self::Empty => "empty",
            Self::TooManyItems => "too_many_items",
            Self::TooDeep => "too_deep",
            Self::BadKey => "bad_key",
            Self::DuplicateKey => "duplicate_key",
            Self::DuplicateDependency => "duplicate_dependency",
            Self::TooManyEntries => "too_many_entries",
            Self::UnknownDependency => "unknown_dependency",
            Self::Cycle => "cycle",
            Self::UnknownCapability => "unknown_capability",
            Self::CapabilityDenied => "capability_denied",
            Self::UnknownCriterion => "unknown_criterion",
            Self::UncoveredCriterion => "uncovered_criterion",
            Self::NotIndependentlyVerifiable => "not_independently_verifiable",
            Self::TextTooLong => "text_too_long",
            Self::HiddenText => "hidden_text",
            Self::PlannerFailed => "planner_failed",
        }
    }
}

impl std::fmt::Display for ReasonCode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl From<&PlanError> for ReasonCode {
    fn from(e: &PlanError) -> Self {
        match e {
            PlanError::Planner(_) => Self::PlannerFailed,
            PlanError::Json(_) => Self::BadJson,
            PlanError::Contract(_) => Self::ContractInvalid,
            PlanError::Version | PlanError::ContractVersionMismatch { .. } => Self::BadVersion,
            PlanError::Empty => Self::Empty,
            PlanError::TooManyItems => Self::TooManyItems,
            PlanError::BadKey(_) => Self::BadKey,
            PlanError::DuplicateKey(_) => Self::DuplicateKey,
            PlanError::DuplicateDependency { .. } => Self::DuplicateDependency,
            PlanError::TooManyEntries(_) => Self::TooManyEntries,
            PlanError::BadText(_) => Self::HiddenText,
            PlanError::UnknownDependency { .. } => Self::UnknownDependency,
            PlanError::Cycle(_) => Self::Cycle,
            PlanError::UnknownCapability { .. } => Self::UnknownCapability,
            PlanError::CapabilityDenied { .. } => Self::CapabilityDenied,
            PlanError::UnknownCriterion { .. } => Self::UnknownCriterion,
            PlanError::UncoveredCriterion(_) => Self::UncoveredCriterion,
            PlanError::NotIndependentlyVerifiable(_) => Self::NotIndependentlyVerifiable,
        }
    }
}

/// A model plan that passed [`validate_model_plan`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidatedPlan {
    pub plan: Plan,
    pub valid: ValidPlan,
    /// `sha256` of the plan as the store hashes it (`plans.hash`).
    pub hash: String,
}

/// The hash the store gives a stored plan: `sha256` of its JSON.
pub fn plan_hash(plan: &Plan) -> String {
    let text = serde_json::to_value(plan)
        .map(|v| v.to_string())
        .unwrap_or_default();
    Sha256::digest(text.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// Checks the raw output of a planner that is a model, against the contract it must serve. Pure.
/// Order: size, shape (unknown fields refused), version, item count, text (too long, or any
/// hidden, bidi or control character: refused, never stripped, so the person reads exactly what
/// the worker gets), then the core validator (keys, dependencies, cycles, capabilities within the
/// envelope, criteria that exist, every mandatory criterion covered and independently verifiable),
/// then depth.
pub fn validate_model_plan(
    plan_json: &str,
    contract: &Contract,
    limits: &ModelPlanLimits,
) -> Result<ValidatedPlan, ReasonCode> {
    if plan_json.len() > MAX_PLAN_JSON_BYTES {
        return Err(ReasonCode::TooLarge);
    }
    let plan: Plan = serde_json::from_str(plan_json).map_err(|_| ReasonCode::BadJson)?;
    if plan.version != limits.version {
        return Err(ReasonCode::BadVersion);
    }
    if plan.items.len() > limits.max_items.min(MAX_PLAN_ITEMS) {
        return Err(ReasonCode::TooManyItems);
    }
    for it in &plan.items {
        if it.title.chars().count() > MAX_TITLE_CHARS
            || it.instructions.chars().count() > MAX_INSTRUCTIONS_CHARS
        {
            return Err(ReasonCode::TextTooLong);
        }
        // a title is one line; instructions may have line breaks and tabs, nothing else hidden
        let hidden = |t: &str, multiline: bool| {
            t.chars()
                .any(|c| is_invisible_or_blank(c) && !(multiline && matches!(c, '\n' | '\t')))
        };
        if hidden(&it.title, false) || hidden(&it.instructions, true) {
            return Err(ReasonCode::HiddenText);
        }
    }
    let valid = validate_plan(&plan, contract).map_err(|e| ReasonCode::from(&e))?;
    let mut depth: BTreeMap<&str, usize> = BTreeMap::new();
    for key in &valid.order {
        let item = plan.items.iter().find(|i| &i.key == key);
        let d = 1 + item
            .and_then(|i| {
                i.requires
                    .iter()
                    .filter_map(|r| depth.get(r.as_str()))
                    .max()
            })
            .copied()
            .unwrap_or(0);
        if d > MAX_PLAN_DEPTH {
            return Err(ReasonCode::TooDeep);
        }
        depth.insert(key, d);
    }
    let hash = plan_hash(&plan);
    Ok(ValidatedPlan { plan, valid, hash })
}

fn diff_json(d: &PlanDiff) -> Value {
    json!({"added": d.added, "removed": d.removed, "changed": d.changed,
           "unchanged": d.unchanged, "rerun": d.rerun})
}

/// Every field of every item, as the person will see it (no preselection, nothing left out).
fn items_json(p: &Plan) -> Value {
    p.items
        .iter()
        .map(|i| {
            json!({"key": i.key, "kind": i.kind, "requires": i.requires,
                   "capabilities": i.capabilities, "covers": i.covers,
                   "title": i.title, "instructions": i.instructions})
        })
        .collect()
}

/// Creates the `plan_approval` decision for a validated model plan and opens it (`pending`).
///
/// The stored action is `{source: "model", plan_run_id, plan, stub_plan?}`: the model's plan and,
/// when the deterministic stub plan is valid for this contract, that plan as the alternative. The
/// store hashes the action into the binding. The brief is built here from the plans themselves:
/// the item table, the diff against the stub plan, and (when a plan exists) against the current
/// plan. The person is offered `approve_model_plan`, `use_stub_plan` and `reject`; nothing is
/// preselected. The same run asked twice returns the same decision.
pub fn record_plan_approval(
    w: &mut OutcomeWriter,
    plan_run_id: &str,
    model: &ValidatedPlan,
    now: OffsetDateTime,
) -> Result<DecisionRecord, StoreError> {
    let o = w.get_outcome(w.id())?;
    let next = o.plan_version.unwrap_or(0) + 1;
    let (contract, _, approved) = w.get_contract(w.id(), o.contract_version)?;
    if !approved || model.plan.version != next || model.plan.contract_version != o.contract_version
    {
        return Err(StoreError::Invalid(
            "a plan approval needs the approved current contract and the next plan version".into(),
        ));
    }
    let current = match o.plan_version {
        Some(v) => Some(w.get_plan(w.id(), v)?),
        None => None,
    };
    let stub = make_plan(&StubPlanner, &contract, next)
        .ok()
        .map(|(p, _)| p);
    let mut action = json!({"source": "model", "plan_run_id": plan_run_id,
                            "plan": serde_json::to_value(&model.plan)?});
    let mut alternatives = vec![json!({"id": "approve_model_plan", "label": ""})];
    if let Some(s) = &stub {
        action["stub_plan"] = serde_json::to_value(s)?;
        alternatives.push(json!({"id": "use_stub_plan", "label": ""}));
    }
    let agents = model
        .plan
        .items
        .iter()
        .filter(|i| i.kind == WorkItemKind::Agent)
        .count();
    let brief = json!({
        "why": "a model wrote this plan; nothing runs until you choose",
        "why_human": "a model wrote this plan",
        "delta_from_contract": "none: the plan cannot change the goal, criteria, checks, envelope or budgets",
        "alternatives": alternatives,
        "plan": {
            "source": "model",
            "plan_run_id": plan_run_id,
            "plan_version": next,
            "plan_hash": model.hash,
            "items": items_json(&model.plan),
            "agent_items": agents,
            "max_attempts_per_work_item": contract.envelope.budgets.max_attempts_per_work_item,
            "outcome_cost_cap_usd": contract.envelope.budgets.max_cost_usd,
            "diff_vs_stub_plan": stub.as_ref().map(|s| json!({
                "stub_plan_hash": plan_hash(s), "stub_items": items_json(s),
                "diff": diff_json(&diff_plans(s, &model.plan))})),
            "diff_vs_current_plan": current.as_ref().map(|c| json!({
                "current_plan_version": c.version, "diff": diff_json(&diff_plans(c, &model.plan))})),
        },
        "unknowns": [],
        "safe_default": "HOLD",
    });
    let binding = plan_binding(w, "")?;
    let hours = i64::from(w.decision_expiry_hours().min(24));
    let fmt = time::macros::format_description!(
        "[year]-[month]-[day]T[hour]:[minute]:[second].[subsecond digits:3]Z"
    );
    let expires_at = (now + time::Duration::hours(hours))
        .format(&fmt)
        .map_err(|e| StoreError::Invalid(e.to_string()))?;
    let (d, fresh) = w.new_decision(&NewDecision {
        kind: "plan_approval".into(),
        dedupe_key: format!("plan:{plan_run_id}").chars().take(128).collect(),
        brief,
        binding,
        expires_at,
        action: Some(action),
    })?;
    if fresh {
        w.transition_decision(
            &d.id,
            d.revision,
            DecisionState::Pending,
            &d.binding,
            &synthesis_session::outcome::Effects::default(),
        )?;
    }
    w.list_decisions(w.id())?
        .into_iter()
        .find(|x| x.id == d.id)
        .ok_or_else(|| StoreError::NotFound(format!("decision {}", d.id)))
}

/// Installs the plan of an approved `plan_approval` decision: the gateway allows `plan.install`
/// for the orchestrator (the verdict is audited either way), the plan is read from the decision's
/// stored action (hash-checked by the store), a model plan is validated AGAIN against the current
/// contract (replay re-validates the stored output; the model is never run again), and the store
/// refuses a moved binding. Returns the version installed.
pub fn install_approved_plan(
    w: &mut OutcomeWriter,
    d: &DecisionRecord,
    current: &Value,
) -> Result<u32, StoreError> {
    let allow = authorize(Principal::Orchestrator, Some(Action::PlanInstall)).allow;
    let a = audit(Principal::Orchestrator, Some(Action::PlanInstall));
    w.audit_policy(
        Action::PlanInstall.name(),
        a["result"].as_str().unwrap_or("deny"),
        json!({"decision_id": d.id, "gateway": a["detail"]}),
    )?;
    if !allow {
        return Err(StoreError::Invalid(
            "the gateway denies plan.install".into(),
        ));
    }
    let choice = w
        .verified_resolution(&d.id)?
        .map(|r| r.choice)
        .unwrap_or_default();
    let action = w
        .decision_action(&d.id)?
        .ok_or_else(|| StoreError::Invalid("the decision has no stored action".into()))?;
    let o = w.get_outcome(w.id())?;
    let (contract, ..) = w.get_contract(w.id(), o.contract_version)?;
    if choice == "approve_model_plan" {
        let text = action
            .get("plan")
            .map(Value::to_string)
            .ok_or_else(|| StoreError::Invalid("the stored action carries no plan".into()))?;
        let limits = ModelPlanLimits {
            version: o.plan_version.unwrap_or(0) + 1,
            max_items: MAX_PLAN_ITEMS,
        };
        validate_model_plan(&text, &contract, &limits).map_err(|c| {
            StoreError::Invalid(format!("the stored plan no longer validates: {c}"))
        })?;
    }
    w.install_plan_from_decision(&d.id, current)?;
    Ok(o.plan_version.unwrap_or(0) + 1)
}
