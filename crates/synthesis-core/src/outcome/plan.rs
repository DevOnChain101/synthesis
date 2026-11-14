//! The plan: a versioned DAG of work items for one contract version, and its validator
//! (ADR-017 "Scheduler": unknown dependency, cycle, unknown capability, uncovered mandatory
//! criterion are rejected). Pure function over the data; the planner that produces it is not here.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::outcome::contract::{
    Capability, CapabilityMode, Contract, ContractError, valid_key, validate_contract,
};

/// A planner is untrusted input (a model, later): bound what it can make us allocate.
pub const MAX_PLAN_ITEMS: usize = 256;

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
}

impl std::fmt::Display for PlanError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
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
            Self::BadKey(k) => write!(f, "invalid work item key {k:?}"),
            Self::DuplicateKey(k) => write!(f, "duplicate work item key {k:?}"),
            Self::UnknownDependency { item, dependency } => {
                write!(f, "work item {item:?} requires unknown item {dependency:?}")
            }
            Self::Cycle(keys) => write!(f, "dependency cycle among {}", keys.join(", ")),
            Self::UnknownCapability { item, capability } => {
                write!(
                    f,
                    "work item {item:?} needs unknown capability {capability:?}"
                )
            }
            Self::CapabilityDenied { item, capability } => write!(
                f,
                "work item {item:?} needs {}, which the envelope denies",
                capability.as_str()
            ),
            Self::UnknownCriterion { item, criterion } => {
                write!(
                    f,
                    "work item {item:?} covers unknown criterion {criterion:?}"
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
    for it in &plan.items {
        if !valid_key(&it.key) {
            return Err(PlanError::BadKey(it.key.clone()));
        }
        if !keys.insert(it.key.as_str()) {
            return Err(PlanError::DuplicateKey(it.key.clone()));
        }
    }
    let criteria: BTreeSet<&str> = contract.criteria.iter().map(|c| c.id.as_str()).collect();
    let mut coverage: BTreeMap<String, Vec<String>> =
        criteria.iter().map(|c| ((*c).to_owned(), vec![])).collect();
    for it in &plan.items {
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
    Ok(ValidPlan { order, coverage })
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
        let test = item("test", WorkItemKind::Verify, &["implement"], &["c3"]);
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
        assert_eq!(v.coverage["c1"], ["implement"]);
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
                    WorkItemKind::Agent,
                    &[],
                    &["c1", "c2", "c3"],
                )
            })
            .collect();
        assert_eq!(validate_plan(&p, &c).unwrap_err(), PlanError::TooManyItems);
        p.items.pop();
        assert!(validate_plan(&p, &c).is_ok());
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
                    kind: WorkItemKind::Agent,
                    requires: edges
                        .iter()
                        .filter(|e| e.0 == i)
                        .map(|e| key(e.1))
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
                    kind: WorkItemKind::Agent,
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
}
