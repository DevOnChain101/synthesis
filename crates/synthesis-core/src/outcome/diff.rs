//! The difference between two contract versions, computed from the two stored contracts (ADR-019
//! section 3, orchestrator B4). A person approves a revised contract by its hash after reading this:
//! it is derived here, from data, and carries no recommendation. `widens` lists every change that
//! gives an attempt more room (paths, capabilities, budgets) and `weakens` every change that lowers
//! the bar of "done" (a criterion or gate removed, a mandatory criterion made optional); both are
//! conservative: a doubt is reported.

use std::collections::{BTreeMap, BTreeSet};

use serde::Serialize;

use super::{Capability, CapabilityMode, Contract};

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Change {
    pub area: &'static str,
    pub key: String,
    pub old: Option<String>,
    pub new: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct ContractDiff {
    pub changes: Vec<Change>,
    pub widens: Vec<String>,
    pub weakens: Vec<String>,
}

fn rank(m: CapabilityMode) -> u8 {
    match m {
        CapabilityMode::Deny => 0,
        CapabilityMode::ApprovalRequired => 1,
        CapabilityMode::Allow => 2,
    }
}

fn mode_word(m: CapabilityMode) -> &'static str {
    match m {
        CapabilityMode::Deny => "deny",
        CapabilityMode::ApprovalRequired => "approval_required",
        CapabilityMode::Allow => "allow",
    }
}

/// A limit that is absent is unlimited.
fn limit_up<T: PartialOrd>(old: Option<T>, new: Option<T>) -> bool {
    match (old, new) {
        (Some(_), None) => true,
        (Some(o), Some(n)) => n > o,
        _ => false,
    }
}

/// `old` is the approved contract, `new` the revision. The version numbers are not compared.
pub fn diff_contracts(old: &Contract, new: &Contract) -> ContractDiff {
    let mut d = ContractDiff::default();
    let put = |d: &mut ContractDiff, area, key: &str, o: Option<String>, n: Option<String>| {
        if o != n {
            d.changes.push(Change {
                area,
                key: key.into(),
                old: o,
                new: n,
            });
        }
    };
    put(
        &mut d,
        "goal",
        "",
        Some(old.goal.clone()),
        Some(new.goal.clone()),
    );

    fn by(c: &Contract) -> BTreeMap<&str, &super::ContractCriterion> {
        c.criteria.iter().map(|k| (k.id.as_str(), k)).collect()
    }
    let (oc, nc) = (by(old), by(new));
    for id in oc.keys().chain(nc.keys()).collect::<BTreeSet<_>>() {
        match (oc.get(id), nc.get(id)) {
            (Some(o), None) => {
                put(&mut d, "criteria", id, Some(o.text.clone()), None);
                d.weakens.push(format!("criterion {id} removed"));
            }
            (None, Some(n)) => put(&mut d, "criteria", id, None, Some(n.text.clone())),
            (Some(o), Some(n)) => {
                let f = |field: &str| format!("{id}.{field}");
                put(
                    &mut d,
                    "criteria",
                    &f("text"),
                    Some(o.text.clone()),
                    Some(n.text.clone()),
                );
                put(
                    &mut d,
                    "criteria",
                    &f("mandatory"),
                    Some(o.mandatory.to_string()),
                    Some(n.mandatory.to_string()),
                );
                put(
                    &mut d,
                    "criteria",
                    &f("manual"),
                    Some(o.manual.to_string()),
                    Some(n.manual.to_string()),
                );
                put(
                    &mut d,
                    "criteria",
                    &f("required_trust"),
                    Some(format!("{:?}", o.required_trust)),
                    Some(format!("{:?}", n.required_trust)),
                );
                put(
                    &mut d,
                    "criteria",
                    &f("check_ids"),
                    Some(o.check_ids.join(",")),
                    Some(n.check_ids.join(",")),
                );
                if o.mandatory && !n.mandatory {
                    d.weakens
                        .push(format!("criterion {id} is no longer mandatory"));
                }
            }
            (None, None) => {}
        }
    }

    fn ch(c: &Contract) -> BTreeMap<&str, String> {
        c.checks
            .iter()
            .map(|k| (k.id.as_str(), k.command.clone()))
            .collect()
    }
    let (oh, nh) = (ch(old), ch(new));
    for id in oh.keys().chain(nh.keys()).collect::<BTreeSet<_>>() {
        put(
            &mut d,
            "checks",
            id,
            oh.get(id).cloned(),
            nh.get(id).cloned(),
        );
        if !nh.contains_key(id) {
            d.weakens.push(format!("check {id} removed"));
        }
    }

    let (ob, nb) = (&old.envelope.budgets, &new.envelope.budgets);
    let budget =
        |d: &mut ContractDiff, name: &str, o: Option<String>, n: Option<String>, up: bool| {
            put(d, "budgets", name, o, n);
            if up {
                d.widens.push(format!("budget {name} is raised or removed"));
            }
        };
    budget(
        &mut d,
        "max_cost_usd",
        Some(ob.max_cost_usd.to_string()),
        Some(nb.max_cost_usd.to_string()),
        nb.max_cost_usd > ob.max_cost_usd,
    );
    budget(
        &mut d,
        "max_parallel_workers",
        Some(ob.max_parallel_workers.to_string()),
        Some(nb.max_parallel_workers.to_string()),
        nb.max_parallel_workers > ob.max_parallel_workers,
    );
    budget(
        &mut d,
        "max_attempts_per_work_item",
        Some(ob.max_attempts_per_work_item.to_string()),
        Some(nb.max_attempts_per_work_item.to_string()),
        nb.max_attempts_per_work_item > ob.max_attempts_per_work_item,
    );
    budget(
        &mut d,
        "attempt_max_cost_usd",
        ob.attempt_max_cost_usd.map(|v| v.to_string()),
        nb.attempt_max_cost_usd.map(|v| v.to_string()),
        limit_up(ob.attempt_max_cost_usd, nb.attempt_max_cost_usd),
    );
    budget(
        &mut d,
        "attempt_max_wall_secs",
        ob.attempt_max_wall_secs.map(|v| v.to_string()),
        nb.attempt_max_wall_secs.map(|v| v.to_string()),
        limit_up(ob.attempt_max_wall_secs, nb.attempt_max_wall_secs),
    );

    let (oe, ne) = (&old.envelope, &new.envelope);
    // a pattern added to the allowed paths, or taken off the denied ones, gives more room (a pattern
    // that happens to be narrower is still reported: no glob algebra here)
    for (area, o, n, widen_on_add) in [
        ("allowed_paths", &oe.allowed_paths, &ne.allowed_paths, true),
        ("denied_paths", &oe.denied_paths, &ne.denied_paths, false),
    ] {
        let (o, n): (BTreeSet<_>, BTreeSet<_>) = (o.iter().collect(), n.iter().collect());
        for p in n.difference(&o) {
            put(&mut d, area, p, None, Some("added".into()));
            if widen_on_add {
                d.widens.push(format!("{area}: {p} added"));
            }
        }
        for p in o.difference(&n) {
            put(&mut d, area, p, Some("removed".into()), None);
            if !widen_on_add {
                d.widens.push(format!("{area}: {p} removed"));
            }
        }
    }
    for c in Capability::ALL {
        let (o, n) = (oe.mode(c), ne.mode(c));
        put(
            &mut d,
            "capabilities",
            c.as_str(),
            Some(mode_word(o).into()),
            Some(mode_word(n).into()),
        );
        if rank(n) > rank(o) {
            d.widens.push(format!(
                "capability {} {} -> {}",
                c.as_str(),
                mode_word(o),
                mode_word(n)
            ));
        }
    }
    let (og, ng): (BTreeSet<_>, BTreeSet<_>) = (
        oe.quality_gates.iter().collect(),
        ne.quality_gates.iter().collect(),
    );
    for g in ng.difference(&og) {
        put(&mut d, "quality_gates", g, None, Some("added".into()));
    }
    for g in og.difference(&ng) {
        put(&mut d, "quality_gates", g, Some("removed".into()), None);
        d.weakens.push(format!("quality gate {g} removed"));
    }
    d
}

#[cfg(test)]
mod tests {
    use super::super::contract::fixtures::contract;
    use super::*;

    #[test]
    fn identical_contracts_have_no_diff_and_the_version_is_ignored() {
        let a = contract(&["c1"]);
        let mut b = a.clone();
        b.version = 2;
        assert_eq!(diff_contracts(&a, &b), ContractDiff::default());
    }

    #[test]
    fn widening_the_envelope_or_budget_is_flagged() {
        let a = contract(&["c1"]);
        let mut b = a.clone();
        b.envelope.allowed_paths.push("docs/**".into());
        b.envelope.denied_paths.clear();
        b.envelope
            .capabilities
            .insert(Capability::NetworkOutbound, CapabilityMode::Allow);
        b.envelope.budgets.max_cost_usd = 9.0;
        let d = diff_contracts(&a, &b);
        for w in [
            "allowed_paths: docs/** added",
            "denied_paths: infra/** removed",
            "capability network.outbound deny -> allow",
            "budget max_cost_usd",
        ] {
            assert!(
                d.widens.iter().any(|x| x.starts_with(w)),
                "{w}: {:?}",
                d.widens
            );
        }
        assert!(d.weakens.is_empty());
    }

    #[test]
    fn narrowing_is_not_flagged_but_is_listed() {
        let a = contract(&["c1"]);
        let mut b = a.clone();
        b.envelope.allowed_paths = vec!["src/**".into()];
        b.envelope.denied_paths.push("secrets/**".into());
        b.envelope
            .capabilities
            .insert(Capability::RepoWrite, CapabilityMode::ApprovalRequired);
        b.envelope.budgets.max_cost_usd = 1.0;
        b.envelope.budgets.attempt_max_cost_usd = Some(0.5);
        let d = diff_contracts(&a, &b);
        assert!(d.widens.is_empty(), "{:?}", d.widens);
        assert!(
            d.changes
                .iter()
                .any(|c| c.area == "budgets" && c.key == "max_cost_usd")
        );
        assert!(
            d.changes
                .iter()
                .any(|c| c.area == "capabilities" && c.key == "repo.write")
        );
    }

    #[test]
    fn removing_the_cap_of_an_attempt_widens_and_a_lower_bar_weakens() {
        let mut a = contract(&["c1", "c2"]);
        a.envelope.budgets.attempt_max_wall_secs = Some(60);
        let mut b = a.clone();
        b.envelope.budgets.attempt_max_wall_secs = None;
        b.criteria.pop();
        b.criteria[0].mandatory = false;
        let d = diff_contracts(&a, &b);
        assert!(d.widens.iter().any(|x| x.contains("attempt_max_wall_secs")));
        assert_eq!(d.weakens.len(), 2, "{:?}", d.weakens);
    }
}
