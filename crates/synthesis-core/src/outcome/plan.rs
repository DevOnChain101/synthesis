//! The plan: a versioned DAG of work items for one contract version, and its validator
//! (ADR-017 "Scheduler": unknown dependency, cycle, unknown capability, uncovered mandatory
//! criterion are rejected). Pure function over the data; the planner that produces it is not here.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::outcome::contract::{
    Capability, CapabilityMode, Contract, ContractError, MAX_LIST_ENTRIES, valid_key,
    validate_contract,
};

/// A planner is untrusted input (a model, later): bound what it can make us allocate.
pub const MAX_PLAN_ITEMS: usize = 256;

/// A plan as JSON (from a planner that is a model, later) is never larger than this.
pub const MAX_PLAN_JSON_BYTES: usize = 256 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkItemKind {
    /// Runs an attempt: a governed session in its own worktree.
    Agent,
    /// Host-side independent runner: no model, no session.
    Verify,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlanItem {
    /// Unique within the plan (becomes `work_items.key`).
    pub key: String,
    pub kind: WorkItemKind,
    /// Keys of the items that must be `Succeeded` first (`requires` is the only dependency type).
    #[serde(default)]
    pub requires: Vec<String>,
    /// Capability names such as `repo.write`; checked against `Capability` and the envelope.
    #[serde(default)]
    pub capabilities: Vec<String>,
    /// Criterion ids this item produces evidence for.
    #[serde(default)]
    pub covers: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Plan {
    pub version: u32,
    pub contract_version: u32,
    pub items: Vec<PlanItem>,
}

/// A plan that passed [`validate_plan`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidPlan {
    /// Keys in canonical order: a topological order, ties broken by key.
    pub order: Vec<String>,
    /// Criterion id to the item keys that cover it, for every criterion of the contract.
    pub coverage: BTreeMap<String, Vec<String>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlanError {
    /// The planner itself failed (a model that errored or refused); the text is its own.
    Planner(String),
    /// The planner's JSON is too large, not JSON, or not shaped like a [`Plan`] (unknown fields
    /// are refused).
    Json(String),
    Contract(ContractError),
    Version,
    ContractVersionMismatch {
        plan: u32,
        contract: u32,
    },
    Empty,
    TooManyItems,
    BadKey(String),
    DuplicateKey(String),
    /// The same dependency listed twice (the `work_item_deps` key would refuse it).
    DuplicateDependency {
        item: String,
        dependency: String,
    },
    /// A `requires`, `capabilities` or `covers` list over the cap.
    TooManyEntries(String),
    UnknownDependency {
        item: String,
        dependency: String,
    },
    /// Keys of the items on a cycle or waiting behind one (sorted).
    Cycle(Vec<String>),
    UnknownCapability {
        item: String,
        capability: String,
    },
    /// The envelope denies a capability an item needs.
    CapabilityDenied {
        item: String,
        capability: Capability,
    },
    UnknownCriterion {
        item: String,
        criterion: String,
    },
    UncoveredCriterion(String),
    /// A mandatory, non-manual criterion covered only by `agent` items: only a `verify` item
    /// yields independent evidence, so it could never be verified.
    NotIndependentlyVerifiable(String),
}

/// Text that came from a planner, safe to print: control characters (ESC...) dropped, at most 200
/// characters.
fn shown(s: &str) -> String {
    let clean: String = s.chars().filter(|c| !c.is_control()).collect();
    if clean.chars().count() > 200 {
        format!("{}...", clean.chars().take(200).collect::<String>())
    } else {
        clean
    }
}

impl std::fmt::Display for PlanError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Json(e) => write!(f, "plan JSON: {}", shown(e)),
            Self::Planner(e) => write!(f, "the planner failed: {}", shown(e)),
            Self::Contract(e) => write!(f, "contract: {e}"),
            Self::Version => write!(f, "plan version must be 1 or more"),
            Self::ContractVersionMismatch { plan, contract } => {
                write!(
                    f,
                    "plan is for contract v{plan}, the contract is v{contract}"
                )
            }
            Self::Empty => write!(f, "plan has no work items"),
            Self::TooManyItems => write!(f, "plan has more than {MAX_PLAN_ITEMS} work items"),
            Self::BadKey(k) => write!(f, "invalid work item key {:?}", shown(k)),
            Self::DuplicateKey(k) => write!(f, "duplicate work item key {:?}", shown(k)),
            Self::DuplicateDependency { item, dependency } => {
                write!(
                    f,
                    "work item {:?} lists dependency {:?} twice",
                    shown(item),
                    shown(dependency)
                )
            }
            Self::TooManyEntries(i) => {
                write!(
                    f,
                    "work item {:?} has a list over {MAX_LIST_ENTRIES} entries",
                    shown(i)
                )
            }
            Self::NotIndependentlyVerifiable(c) => write!(
                f,
                "mandatory criterion {c:?} is covered by agent items only; a verify item must cover it"
            ),
            Self::UnknownDependency { item, dependency } => {
                write!(
                    f,
                    "work item {:?} requires unknown item {:?}",
                    shown(item),
                    shown(dependency)
                )
            }
            Self::Cycle(keys) => write!(f, "dependency cycle among {}", keys.join(", ")),
            Self::UnknownCapability { item, capability } => {
                write!(
                    f,
                    "work item {:?} needs unknown capability {:?}",
                    shown(item),
                    shown(capability)
                )
            }
            Self::CapabilityDenied { item, capability } => write!(
                f,
                "work item {:?} needs {}, which the envelope denies",
                shown(item),
                capability.as_str()
            ),
            Self::UnknownCriterion { item, criterion } => {
                write!(
                    f,
                    "work item {:?} covers unknown criterion {:?}",
                    shown(item),
                    shown(criterion)
                )
            }
            Self::UncoveredCriterion(c) => {
                write!(f, "mandatory criterion {c:?} is covered by no work item")
            }
        }
    }
}
impl std::error::Error for PlanError {}

impl From<ContractError> for PlanError {
    fn from(e: ContractError) -> Self {
        Self::Contract(e)
    }
}

/// Checks `plan` against `contract`. The first problem found is returned, in a fixed order, so
/// the same bad plan always gives the same error.
pub fn validate_plan(plan: &Plan, contract: &Contract) -> Result<ValidPlan, PlanError> {
    validate_contract(contract)?;
    if plan.version == 0 {
        return Err(PlanError::Version);
    }
    if plan.contract_version != contract.version {
        return Err(PlanError::ContractVersionMismatch {
            plan: plan.contract_version,
            contract: contract.version,
        });
    }
    if plan.items.is_empty() {
        return Err(PlanError::Empty);
    }
    if plan.items.len() > MAX_PLAN_ITEMS {
        return Err(PlanError::TooManyItems);
    }
    let mut keys = BTreeSet::new();
    let mut folded = BTreeSet::new();
    for it in &plan.items {
        if !valid_key(&it.key) {
            return Err(PlanError::BadKey(it.key.clone()));
        }
        if !keys.insert(it.key.as_str()) || !folded.insert(it.key.to_ascii_lowercase()) {
            return Err(PlanError::DuplicateKey(it.key.clone()));
        }
        if [it.requires.len(), it.capabilities.len(), it.covers.len()]
            .iter()
            .any(|n| *n > MAX_LIST_ENTRIES)
        {
            return Err(PlanError::TooManyEntries(it.key.clone()));
        }
    }
    let criteria: BTreeSet<&str> = contract.criteria.iter().map(|c| c.id.as_str()).collect();
    let mut coverage: BTreeMap<String, Vec<String>> =
        criteria.iter().map(|c| ((*c).to_owned(), vec![])).collect();
    let mut verified_by: BTreeSet<&str> = BTreeSet::new();
    for it in &plan.items {
        let mut deps = BTreeSet::new();
        if let Some(d) = it.requires.iter().find(|d| !deps.insert(d.as_str())) {
            return Err(PlanError::DuplicateDependency {
                item: it.key.clone(),
                dependency: d.clone(),
            });
        }
        if let Some(d) = it.requires.iter().find(|d| !keys.contains(d.as_str())) {
            return Err(PlanError::UnknownDependency {
                item: it.key.clone(),
                dependency: d.clone(),
            });
        }
        for name in &it.capabilities {
            let cap = Capability::parse(name).ok_or_else(|| PlanError::UnknownCapability {
                item: it.key.clone(),
                capability: name.clone(),
            })?;
            if contract.envelope.mode(cap) == CapabilityMode::Deny {
                return Err(PlanError::CapabilityDenied {
                    item: it.key.clone(),
                    capability: cap,
                });
            }
        }
        for c in &it.covers {
            let Some(v) = coverage.get_mut(c) else {
                return Err(PlanError::UnknownCriterion {
                    item: it.key.clone(),
                    criterion: c.clone(),
                });
            };
            if !v.contains(&it.key) {
                v.push(it.key.clone());
            }
            if it.kind == WorkItemKind::Verify {
                verified_by.insert(c.as_str());
            }
        }
    }
    // Kahn's algorithm; the ready set is ordered, so the result is canonical.
    let mut waiting: BTreeMap<&str, BTreeSet<&str>> = plan
        .items
        .iter()
        .map(|i| {
            (
                i.key.as_str(),
                i.requires.iter().map(String::as_str).collect(),
            )
        })
        .collect();
    let mut order = Vec::with_capacity(plan.items.len());
    while let Some(next) = waiting.iter().find(|(_, d)| d.is_empty()).map(|(k, _)| *k) {
        waiting.remove(next);
        for d in waiting.values_mut() {
            d.remove(next);
        }
        order.push(next.to_owned());
    }
    if !waiting.is_empty() {
        return Err(PlanError::Cycle(
            waiting.keys().map(|k| (*k).to_owned()).collect(),
        ));
    }
    // Last, so a structurally broken plan reports its structure first.
    if let Some(c) = contract
        .criteria
        .iter()
        .find(|c| c.mandatory && coverage[&c.id].is_empty())
    {
        return Err(PlanError::UncoveredCriterion(c.id.clone()));
    }
    if let Some(c) = contract
        .criteria
        .iter()
        .find(|c| c.mandatory && !c.manual && !verified_by.contains(c.id.as_str()))
    {
        return Err(PlanError::NotIndependentlyVerifiable(c.id.clone()));
    }
    Ok(ValidPlan { order, coverage })
}

/// The planner's output as JSON, checked: size cap, strict shape (unknown fields refused), then
/// [`validate_plan`] against the contract. The same door for the stub planner and, later, a model.
pub fn parse_plan(json: &str, contract: &Contract) -> Result<(Plan, ValidPlan), PlanError> {
    if json.len() > MAX_PLAN_JSON_BYTES {
        return Err(PlanError::Json(format!(
            "larger than {MAX_PLAN_JSON_BYTES} bytes"
        )));
    }
    let plan: Plan = serde_json::from_str(json).map_err(|e| PlanError::Json(e.to_string()))?;
    let valid = validate_plan(&plan, contract)?;
    Ok((plan, valid))
}

/// What a new plan version changes against the one it replaces, by item key.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PlanDiff {
    pub added: Vec<String>,
    /// In the old plan only: its work items are not carried over.
    pub removed: Vec<String>,
    /// Same key, different kind, dependencies, capabilities or coverage.
    pub changed: Vec<String>,
    /// Same item and nothing upstream of it changed: it can be carried over as it is.
    pub unchanged: Vec<String>,
    /// Items of the new plan that must run (again): added, changed, and everything that requires
    /// them, directly or not. Sorted.
    pub rerun: Vec<String>,
}

/// Compares two plans (both valid). Pure: neither is modified; the new plan is a new version with
/// new work items, the old records stay as they are.
pub fn diff_plans(old: &Plan, new: &Plan) -> PlanDiff {
    let old_by: BTreeMap<&str, &PlanItem> = old.items.iter().map(|i| (i.key.as_str(), i)).collect();
    let mut d = PlanDiff::default();
    // another contract version is another definition of done: nothing is carried over
    let contract_changed = old.contract_version != new.contract_version;
    let mut dirty: BTreeSet<&str> = BTreeSet::new();
    for it in &new.items {
        match old_by.get(it.key.as_str()) {
            None => {
                d.added.push(it.key.clone());
                dirty.insert(&it.key);
            }
            Some(o) if contract_changed || **o != *it => {
                d.changed.push(it.key.clone());
                dirty.insert(&it.key);
            }
            Some(_) => d.unchanged.push(it.key.clone()),
        }
    }
    let new_keys: BTreeSet<&str> = new.items.iter().map(|i| i.key.as_str()).collect();
    d.removed = old
        .items
        .iter()
        .filter(|i| !new_keys.contains(i.key.as_str()))
        .map(|i| i.key.clone())
        .collect();
    // everything downstream of a dirty item reruns (fixed point; the plan is a DAG of at most 256)
    loop {
        let more: Vec<&str> = new
            .items
            .iter()
            .filter(|i| !dirty.contains(i.key.as_str()))
            .filter(|i| i.requires.iter().any(|r| dirty.contains(r.as_str())))
            .map(|i| i.key.as_str())
            .collect();
        if more.is_empty() {
            break;
        }
        dirty.extend(more);
    }
    d.rerun = dirty.into_iter().map(str::to_owned).collect();
    for v in [
        &mut d.added,
        &mut d.removed,
        &mut d.changed,
        &mut d.unchanged,
    ] {
        v.sort();
    }
    d.unchanged.retain(|k| !d.rerun.contains(k));
    d
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::outcome::contract::fixtures::contract;

    fn item(key: &str, kind: WorkItemKind, requires: &[&str], covers: &[&str]) -> PlanItem {
        PlanItem {
            key: key.into(),
            kind,
            requires: requires.iter().map(|s| (*s).into()).collect(),
            capabilities: vec![],
            covers: covers.iter().map(|s| (*s).into()).collect(),
        }
    }

    /// inspect -> implement -> test, as the stub planner will emit it.
    fn good() -> (Plan, Contract) {
        let mut inspect = item("inspect", WorkItemKind::Agent, &[], &[]);
        inspect.capabilities = vec!["repo.read".into()];
        let mut implement = item(
            "implement",
            WorkItemKind::Agent,
            &["inspect"],
            &["c1", "c2"],
        );
        implement.capabilities = vec!["repo.write".into(), "tests.run".into()];
        let test = item(
            "test",
            WorkItemKind::Verify,
            &["implement"],
            &["c1", "c2", "c3"],
        );
        (
            Plan {
                version: 1,
                contract_version: 1,
                items: vec![test, implement, inspect],
            },
            contract(&["c1", "c2", "c3"]),
        )
    }

    #[test]
    fn a_valid_plan_is_accepted_with_order_and_coverage() {
        let (p, c) = good();
        let v = validate_plan(&p, &c).unwrap();
        assert_eq!(
            v.order,
            ["inspect", "implement", "test"],
            "input order is irrelevant"
        );
        assert_eq!(v.coverage["c1"], ["test", "implement"]);
        assert_eq!(v.coverage["c3"], ["test"]);
        assert_eq!(v.coverage.len(), 3);
        let j = serde_json::to_string(&p).unwrap();
        assert_eq!(serde_json::from_str::<Plan>(&j).unwrap(), p);
    }

    #[test]
    fn each_mutation_gets_its_own_error() {
        let (base, c) = good();
        let mutate = |f: fn(&mut Plan)| {
            let mut p = base.clone();
            f(&mut p);
            validate_plan(&p, &c).unwrap_err()
        };
        assert!(matches!(
            mutate(|p| p.items[1].requires.push("ghost".into())),
            PlanError::UnknownDependency { .. }
        ));
        assert!(matches!(
            mutate(|p| p.items[2].requires.push("implement".into())),
            PlanError::Cycle(k) if k == ["implement", "inspect", "test"]
        ));
        assert!(matches!(
            mutate(|p| p.items[0].requires.push("test".into())),
            PlanError::Cycle(_)
        ));
        assert!(matches!(
            mutate(|p| p.items[1].capabilities.push("repo.delete".into())),
            PlanError::UnknownCapability { .. }
        ));
        assert!(matches!(
            mutate(|p| p.items[1].capabilities.push("git.push".into())),
            PlanError::CapabilityDenied {
                capability: Capability::GitPush,
                ..
            }
        ));
        // unlisted capabilities are denied by the envelope too
        assert!(matches!(
            mutate(|p| p.items[1].capabilities.push("network.outbound".into())),
            PlanError::CapabilityDenied { .. }
        ));
        assert!(matches!(
            mutate(|p| p.items[1].covers.push("c9".into())),
            PlanError::UnknownCriterion { .. }
        ));
        assert!(matches!(
            mutate(|p| p.items[0].covers.clear()),
            PlanError::UncoveredCriterion(c) if c == "c3"
        ));
        assert!(matches!(mutate(|p| p.items.clear()), PlanError::Empty));
        assert!(matches!(mutate(|p| p.version = 0), PlanError::Version));
        assert!(matches!(
            mutate(|p| p.contract_version = 2),
            PlanError::ContractVersionMismatch {
                plan: 2,
                contract: 1
            }
        ));
        assert!(matches!(
            mutate(|p| p.items[0].key = "implement".into()),
            PlanError::DuplicateKey(_)
        ));
        assert!(matches!(
            mutate(|p| p.items[0].key = "../x".into()),
            PlanError::BadKey(_)
        ));
    }

    #[test]
    fn a_non_mandatory_criterion_may_stay_uncovered_but_a_mandatory_one_may_not() {
        let (p, mut c) = good();
        c.criteria
            .push(crate::outcome::contract::fixtures::criterion("c4"));
        assert_eq!(
            validate_plan(&p, &c).unwrap_err(),
            PlanError::UncoveredCriterion("c4".into())
        );
        c.criteria[3].mandatory = false;
        assert!(validate_plan(&p, &c).is_ok());
    }

    #[test]
    fn approval_required_capabilities_are_plannable() {
        let (mut p, mut c) = good();
        c.envelope
            .capabilities
            .insert(Capability::GitPush, CapabilityMode::ApprovalRequired);
        p.items[1].capabilities.push("git.push".into());
        assert!(validate_plan(&p, &c).is_ok());
    }

    #[test]
    fn a_bad_contract_is_reported_as_such() {
        let (p, mut c) = good();
        c.goal.clear();
        assert!(matches!(
            validate_plan(&p, &c),
            Err(PlanError::Contract(ContractError::Goal))
        ));
    }

    #[test]
    fn plan_size_is_bounded() {
        let (mut p, c) = good();
        p.items = (0..=MAX_PLAN_ITEMS)
            .map(|i| {
                item(
                    &format!("i{i}"),
                    WorkItemKind::Verify,
                    &[],
                    &["c1", "c2", "c3"],
                )
            })
            .collect();
        assert_eq!(validate_plan(&p, &c).unwrap_err(), PlanError::TooManyItems);
        p.items.pop();
        assert_eq!(validate_plan(&p, &c).map(|_| ()), Ok(()));
    }

    #[test]
    fn serde_refuses_unknown_fields_and_kinds() {
        assert!(serde_json::from_str::<PlanItem>(r#"{"key":"a","kind":"agent","x":1}"#).is_err());
        assert!(serde_json::from_str::<PlanItem>(r#"{"key":"a","kind":"task"}"#).is_err());
        assert!(serde_json::from_str::<PlanItem>(r#"{"key":"a","kind":"verify"}"#).is_ok());
    }

    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }
    }

    /// Reference: transitive closure by repeated relaxation; a cycle is any node reaching itself.
    fn has_cycle(n: usize, edges: &[(usize, usize)]) -> bool {
        let mut r = vec![vec![false; n]; n];
        for &(a, b) in edges {
            r[a][b] = true;
        }
        for k in 0..n {
            for i in 0..n {
                for j in 0..n {
                    if r[i][k] && r[k][j] {
                        r[i][j] = true;
                    }
                }
            }
        }
        (0..n).any(|i| r[i][i])
    }

    #[test]
    fn random_graphs_agree_with_a_reference_cycle_check() {
        let mut rng = Rng(0xD1B5_4A32_D192_ED03);
        let c = contract(&["c1"]);
        for _ in 0..1500 {
            let n = 1 + (rng.next() % 8) as usize;
            let mut edges = vec![];
            for _ in 0..(rng.next() % 14) {
                edges.push((
                    (rng.next() % n as u64) as usize,
                    (rng.next() % n as u64) as usize,
                ));
            }
            // key order is shuffled relative to index so ordering bugs cannot hide
            let key = |i: usize| format!("k{:02}", (i * 7 + 3) % 97);
            let items: Vec<PlanItem> = (0..n)
                .map(|i| PlanItem {
                    key: key(i),
                    kind: if i == 0 {
                        WorkItemKind::Verify
                    } else {
                        WorkItemKind::Agent
                    },
                    requires: edges
                        .iter()
                        .filter(|e| e.0 == i)
                        .map(|e| key(e.1))
                        .collect::<BTreeSet<_>>()
                        .into_iter()
                        .collect(),
                    capabilities: vec![],
                    covers: if i == 0 { vec!["c1".into()] } else { vec![] },
                })
                .collect();
            let plan = Plan {
                version: 1,
                contract_version: 1,
                items,
            };
            match validate_plan(&plan, &c) {
                Ok(v) => {
                    assert!(!has_cycle(n, &edges), "{edges:?}");
                    assert_eq!(v.order.len(), n);
                    let pos = |k: &str| v.order.iter().position(|x| x == k).unwrap();
                    for &(a, b) in &edges {
                        assert!(pos(&key(b)) < pos(&key(a)), "{edges:?} {:?}", v.order);
                    }
                    assert_eq!(
                        v.order,
                        validate_plan(&plan, &c).unwrap().order,
                        "deterministic"
                    );
                }
                Err(PlanError::Cycle(ks)) => {
                    assert!(has_cycle(n, &edges), "{edges:?}");
                    assert!(!ks.is_empty() && ks.windows(2).all(|w| w[0] < w[1]));
                }
                Err(e) => panic!("unexpected {e}"),
            }
        }
    }

    #[test]
    fn dependencies_that_only_point_backwards_never_cycle_and_one_back_edge_always_does() {
        let mut rng = Rng(0xA5A5_1234_5678_9ABC);
        let c = contract(&["c1"]);
        for _ in 0..300 {
            let n = 2 + (rng.next() % 12) as usize;
            let key = |i: usize| format!("w{i}");
            let mut items: Vec<PlanItem> = (0..n)
                .map(|i| PlanItem {
                    key: key(i),
                    kind: if i == 0 {
                        WorkItemKind::Verify
                    } else {
                        WorkItemKind::Agent
                    },
                    requires: (0..i)
                        .filter(|_| rng.next().is_multiple_of(3))
                        .map(key)
                        .collect(),
                    capabilities: vec![],
                    covers: if i == 0 { vec!["c1".into()] } else { vec![] },
                })
                .collect();
            let mut plan = Plan {
                version: 1,
                contract_version: 1,
                items: vec![],
            };
            plan.items = items.clone();
            assert!(validate_plan(&plan, &c).is_ok());
            // chain every item to its predecessor, then close the loop
            for (i, it) in items.iter_mut().enumerate().skip(1) {
                if !it.requires.contains(&key(i - 1)) {
                    it.requires.push(key(i - 1));
                }
            }
            items[0].requires.push(key(n - 1));
            plan.items = items;
            assert!(matches!(validate_plan(&plan, &c), Err(PlanError::Cycle(k)) if k.len() == n));
        }
    }

    #[test]
    fn a_self_dependency_is_a_cycle() {
        let (mut p, c) = good();
        p.items[0].requires.push("test".into());
        assert_eq!(
            validate_plan(&p, &c).unwrap_err(),
            PlanError::Cycle(vec!["test".into()])
        );
    }

    #[test]
    fn duplicate_dependencies_long_lists_and_agent_only_coverage_are_refused() {
        let (base, c) = good();
        let mut p = base.clone();
        p.items[1].requires.push("inspect".into());
        assert!(matches!(
            validate_plan(&p, &c),
            Err(PlanError::DuplicateDependency { .. })
        ));
        let mut p = base.clone();
        p.items[1].capabilities = vec!["repo.read".into(); MAX_LIST_ENTRIES + 1];
        assert_eq!(
            validate_plan(&p, &c).unwrap_err(),
            PlanError::TooManyEntries("implement".into())
        );
        // c3 is covered by the agent item only: it can never get independent evidence
        let mut p = base.clone();
        p.items[0].covers = vec!["c1".into(), "c2".into()];
        p.items[1].covers.push("c3".into());
        assert_eq!(
            validate_plan(&p, &c).unwrap_err(),
            PlanError::NotIndependentlyVerifiable("c3".into())
        );
        // a manual criterion is a person's review, not a verify item's
        let mut c2 = c.clone();
        c2.criteria[2].manual = true;
        c2.criteria[2].required_trust = crate::outcome::TrustClass::HumanReview;
        assert!(validate_plan(&p, &c2).is_ok());
    }

    #[test]
    fn keys_that_collide_or_are_unsafe_on_windows_are_refused() {
        let (base, c) = good();
        for bad in ["NUL", "con", "Com1", "lpt9.txt", "aux.log", "ends.", "x/y"] {
            let mut p = base.clone();
            p.items[0].key = bad.into();
            assert!(
                matches!(validate_plan(&p, &c), Err(PlanError::BadKey(_))),
                "{bad}"
            );
        }
        for ok in ["com0", "console", "nulx", "a.b", "COM10"] {
            let mut p = base.clone();
            p.items[0].key = ok.into();
            assert!(validate_plan(&p, &c).is_ok(), "{ok}");
        }
        // case-insensitive duplicates
        let mut p = base.clone();
        p.items[0].key = "INSPECT".into();
        p.items[2].requires.clear();
        let mut q = p.clone();
        q.items[0].key = "Implement".into();
        assert!(matches!(
            validate_plan(&q, &c),
            Err(PlanError::DuplicateKey(_))
        ));
        // criteria ids collide the same way
        let mut c3 = c.clone();
        c3.criteria[1].id = "C1".into();
        assert!(matches!(
            validate_plan(&base, &c3),
            Err(PlanError::Contract(ContractError::DuplicateCriterion(_)))
        ));
    }

    /// The shape a planner emits (documented in ADR-017).
    const EXAMPLE: &str = r#"{
        "version": 1, "contract_version": 1,
        "items": [
            {"key": "inspect", "kind": "agent", "capabilities": ["repo.read"]},
            {"key": "implement", "kind": "agent", "requires": ["inspect"],
             "capabilities": ["repo.read", "repo.write", "tests.run"]},
            {"key": "test", "kind": "verify", "requires": ["implement"],
             "covers": ["c1", "c2", "c3"]}
        ]}"#;

    #[test]
    fn the_documented_json_parses_and_every_bad_shape_is_refused() {
        let c = contract(&["c1", "c2", "c3"]);
        let (plan, valid) = parse_plan(EXAMPLE, &c).unwrap();
        assert_eq!(valid.order, ["inspect", "implement", "test"]);
        assert_eq!(plan.items.len(), 3);
        let bad = |json: &str| parse_plan(json, &c).unwrap_err();
        assert!(matches!(bad("not json"), PlanError::Json(_)));
        assert!(matches!(
            bad(&EXAMPLE.replace("\"version\": 1,", "\"version\": 1, \"x\": 1,")),
            PlanError::Json(_)
        ));
        assert!(matches!(
            bad(&" ".repeat(MAX_PLAN_JSON_BYTES + 1)),
            PlanError::Json(_)
        ));
        assert!(matches!(
            bad(&EXAMPLE.replace("\"requires\": [\"inspect\"]", "\"requires\": [\"nope\"]")),
            PlanError::UnknownDependency { .. }
        ));
        assert!(matches!(
            bad(&EXAMPLE.replace(
                "\"inspect\", \"kind\": \"agent\", \"capabilities\"",
                "\"inspect\", \"requires\": [\"test\"], \"kind\": \"agent\", \"capabilities\""
            )),
            PlanError::Cycle(_)
        ));
        assert!(matches!(
            bad(&EXAMPLE.replace("repo.read\"]},", "repo.teleport\"]},")),
            PlanError::UnknownCapability { .. }
        ));
        assert!(matches!(
            bad(&EXAMPLE.replace("\"c3\"]", "\"c9\"]")),
            PlanError::UnknownCriterion { .. }
        ));
        assert!(matches!(
            bad(&EXAMPLE.replace(", \"c3\"]", "]")),
            PlanError::UncoveredCriterion(_)
        ));
        assert!(matches!(
            bad(&EXAMPLE.replace("\"contract_version\": 1", "\"contract_version\": 2")),
            PlanError::ContractVersionMismatch { .. }
        ));
    }

    #[test]
    fn a_capability_the_envelope_denies_is_refused() {
        let mut c = contract(&["c1", "c2", "c3"]);
        c.envelope
            .capabilities
            .insert(Capability::RepoWrite, CapabilityMode::Deny);
        assert!(matches!(
            parse_plan(EXAMPLE, &c).unwrap_err(),
            PlanError::CapabilityDenied { .. }
        ));
    }

    #[test]
    fn a_diff_names_what_was_added_removed_changed_and_what_reruns_downstream() {
        let c = contract(&["c1", "c2", "c3"]);
        let (old, _) = parse_plan(EXAMPLE, &c).unwrap();
        // identical: nothing reruns
        let mut same = old.clone();
        same.version = 2;
        let d = diff_plans(&old, &same);
        assert_eq!((d.rerun.len(), d.unchanged.len()), (0, 3));
        // `implement` changes: it and `test` rerun, `inspect` is carried over
        let mut new = same.clone();
        new.items[1].capabilities.pop();
        let d = diff_plans(&old, &new);
        assert_eq!(d.changed, ["implement"]);
        assert_eq!(d.rerun, ["implement", "test"]);
        assert_eq!(d.unchanged, ["inspect"]);
        // an item added before `implement`, `inspect` dropped
        let mut new = same.clone();
        new.items.remove(0);
        new.items[0].requires = vec!["lint".into()];
        new.items.push(item("lint", WorkItemKind::Agent, &[], &[]));
        let d = diff_plans(&old, &new);
        assert_eq!(d.added, ["lint"]);
        assert_eq!(d.removed, ["inspect"]);
        assert_eq!(d.rerun, ["implement", "lint", "test"]);
        // the inputs are untouched (pure)
        assert_eq!(old.items.len(), 3);
    }

    #[test]
    fn planner_text_is_truncated_and_stripped_when_an_error_is_shown() {
        let evil = format!("a\x1b[31m{}\u{7}b", "x".repeat(5000));
        for e in [
            PlanError::Json(evil.clone()),
            PlanError::Planner(evil.clone()),
            PlanError::BadKey(evil.clone()),
            PlanError::UnknownDependency {
                item: "i".into(),
                dependency: evil.clone(),
            },
            PlanError::UnknownCriterion {
                item: evil.clone(),
                criterion: evil.clone(),
            },
            PlanError::UnknownCapability {
                item: "i".into(),
                capability: evil.clone(),
            },
        ] {
            let t = e.to_string();
            assert!(!t.chars().any(char::is_control), "{t:?}");
            assert!(t.chars().count() < 500, "{} chars", t.chars().count());
        }
    }

    #[test]
    fn another_contract_version_reruns_everything() {
        let c = contract(&["c1", "c2", "c3"]);
        let (old, _) = parse_plan(EXAMPLE, &c).unwrap();
        let mut new = old.clone();
        new.version = 2;
        new.contract_version = 2;
        let d = diff_plans(&old, &new);
        assert!(d.unchanged.is_empty(), "nothing is carried over: {d:?}");
        assert_eq!(d.rerun, ["implement", "inspect", "test"]);
    }
}
