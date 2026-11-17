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

/// `old` is the approved contract, `new` the revision. The version numbers are not compared. Every
/// field of `Contract` and its parts is destructured WITHOUT `..`: a field added later does not
/// compile until the diff says what to do with it.
pub fn diff_contracts(old: &Contract, new: &Contract) -> ContractDiff {
    let Contract {
        version: _,
        goal: og,
        criteria: oc,
        envelope: oe,
        checks: oh,
    } = old;
    let Contract {
        version: _,
        goal: ng,
        criteria: nc,
        envelope: ne,
        checks: nh,
    } = new;
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
    put(&mut d, "goal", "", Some(og.clone()), Some(ng.clone()));

    let by = |v: &[super::ContractCriterion]| {
        v.iter()
            .map(|k| (k.id.clone(), k.clone()))
            .collect::<BTreeMap<_, _>>()
    };
    let (oc, nc) = (by(oc), by(nc));
    for id in oc.keys().chain(nc.keys()).collect::<BTreeSet<_>>() {
        match (oc.get(id), nc.get(id)) {
            (Some(o), None) => {
                put(&mut d, "criteria", id, Some(o.text.clone()), None);
                d.weakens.push(format!("criterion {id} removed"));
            }
            (None, Some(n)) => put(&mut d, "criteria", id, None, Some(n.text.clone())),
            (Some(o), Some(n)) => {
                let super::ContractCriterion {
                    id: _,
                    text: ot,
                    mandatory: om,
                    manual: oman,
                    required_trust: ot_trust,
                    check_ids: ocheck,
                } = o;
                let super::ContractCriterion {
                    id: _,
                    text: nt,
                    mandatory: nm,
                    manual: nman,
                    required_trust: nt_trust,
                    check_ids: ncheck,
                } = n;
                let f = |field: &str| format!("{id}.{field}");
                put(
                    &mut d,
                    "criteria",
                    &f("text"),
                    Some(ot.clone()),
                    Some(nt.clone()),
                );
                put(
                    &mut d,
                    "criteria",
                    &f("mandatory"),
                    Some(om.to_string()),
                    Some(nm.to_string()),
                );
                put(
                    &mut d,
                    "criteria",
                    &f("manual"),
                    Some(oman.to_string()),
                    Some(nman.to_string()),
                );
                put(
                    &mut d,
                    "criteria",
                    &f("required_trust"),
                    Some(format!("{ot_trust:?}")),
                    Some(format!("{nt_trust:?}")),
                );
                put(
                    &mut d,
                    "criteria",
                    &f("check_ids"),
                    Some(ocheck.join(",")),
                    Some(ncheck.join(",")),
                );
                if ot != nt {
                    d.weakens.push(format!(
                        "criterion {id} text changed (what counts as done may differ)"
                    ));
                }
                if *om && !*nm {
                    d.weakens
                        .push(format!("criterion {id} is no longer mandatory"));
                }
                if *oman && !*nman {
                    d.weakens
                        .push(format!("criterion {id} no longer needs a person's review"));
                }
                if nt_trust < ot_trust {
                    d.weakens
                        .push(format!("criterion {id} accepts weaker evidence"));
                }
                if ocheck.iter().any(|c| !ncheck.contains(c)) {
                    d.weakens
                        .push(format!("criterion {id} is checked by fewer checks"));
                }
            }
            (None, None) => {}
        }
    }

    let ch = |v: &[super::ContractCheck]| {
        v.iter()
            .map(|k| {
                let super::ContractCheck { id, command } = k;
                (id.clone(), command.clone())
            })
            .collect::<BTreeMap<_, _>>()
    };
    let (oh, nh) = (ch(oh), ch(nh));
    for id in oh.keys().chain(nh.keys()).collect::<BTreeSet<_>>() {
        put(
            &mut d,
            "checks",
            id,
            oh.get(id).cloned(),
            nh.get(id).cloned(),
        );
        match (oh.get(id), nh.get(id)) {
            (Some(_), None) => d.weakens.push(format!("check {id} removed")),
            // a check command runs on the independent runner: a new or changed one is a new thing
            // that runs, and a swapped command can make a check pass that did not
            (None, Some(_)) => d
                .widens
                .push(format!("check {id} added (a new command runs)")),
            (Some(o), Some(n)) if o != n => {
                d.widens.push(format!(
                    "check {id} command changed (a different command runs)"
                ));
                d.weakens
                    .push(format!("check {id} command changed (it may check less)"));
            }
            _ => {}
        }
    }

    let super::Envelope {
        allowed_paths: oa,
        denied_paths: od,
        capabilities: _,
        budgets: ob,
        quality_gates: og2,
    } = oe;
    let super::Envelope {
        allowed_paths: na,
        denied_paths: nd,
        capabilities: _,
        budgets: nb,
        quality_gates: ng2,
    } = ne;
    let super::Budgets {
        max_cost_usd: oco,
        max_parallel_workers: opw,
        max_attempts_per_work_item: oat,
        attempt_max_cost_usd: oac,
        attempt_max_wall_secs: oaw,
    } = ob;
    let super::Budgets {
        max_cost_usd: nco,
        max_parallel_workers: npw,
        max_attempts_per_work_item: nat,
        attempt_max_cost_usd: nac,
        attempt_max_wall_secs: naw,
    } = nb;
    let budget =
        |d: &mut ContractDiff, name: &str, o: Option<String>, n: Option<String>, up: bool| {
            put(d, "budgets", name, o, n);
            if up {
                d.widens.push(format!("budget {name} is raised or removed"));
            }
        };
    let s = |v: &dyn std::fmt::Display| Some(v.to_string());
    budget(&mut d, "max_cost_usd", s(oco), s(nco), nco > oco);
    budget(&mut d, "max_parallel_workers", s(opw), s(npw), npw > opw);
    budget(
        &mut d,
        "max_attempts_per_work_item",
        s(oat),
        s(nat),
        nat > oat,
    );
    budget(
        &mut d,
        "attempt_max_cost_usd",
        oac.map(|v| v.to_string()),
        nac.map(|v| v.to_string()),
        limit_up(*oac, *nac),
    );
    budget(
        &mut d,
        "attempt_max_wall_secs",
        oaw.map(|v| v.to_string()),
        naw.map(|v| v.to_string()),
        limit_up(*oaw, *naw),
    );

    // a pattern added to the allowed paths, or taken off the denied ones, gives more room (a pattern
    // that happens to be narrower is still reported: no glob algebra here)
    for (area, o, n, widen_on_add) in [
        ("allowed_paths", oa, na, true),
        ("denied_paths", od, nd, false),
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
    let (og2, ng2): (BTreeSet<_>, BTreeSet<_>) = (og2.iter().collect(), ng2.iter().collect());
    for g in ng2.difference(&og2) {
        put(&mut d, "quality_gates", g, None, Some("added".into()));
    }
    for g in og2.difference(&ng2) {
        put(&mut d, "quality_gates", g, Some("removed".into()), None);
        d.weakens.push(format!("quality gate {g} removed"));
    }
    d
}

#[cfg(test)]
mod tests {
    use super::super::contract::fixtures::contract;
    use super::super::{ContractCheck, TrustClass};
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

    #[test]
    fn a_swapped_or_new_check_a_lowered_trust_and_fewer_checks_are_flagged() {
        let mut a = contract(&["c1", "c2"]);
        a.checks = vec![ContractCheck {
            id: "t".into(),
            command: "cargo test".into(),
        }];
        a.criteria[0].check_ids = vec!["t".into()];
        a.criteria[1].manual = true;
        a.criteria[1].required_trust = TrustClass::HumanReview;
        let any = |v: &[String], w: &str| v.iter().any(|x| x.contains(w));

        let mut b = a.clone();
        b.checks[0].command = "true".into();
        let d = diff_contracts(&a, &b);
        assert!(
            any(&d.widens, "check t command changed") && any(&d.weakens, "check t command changed")
        );

        let mut b = a.clone();
        b.checks.push(ContractCheck {
            id: "u".into(),
            command: "make".into(),
        });
        assert!(any(&diff_contracts(&a, &b).widens, "check u added"));

        let mut b = a.clone();
        b.criteria[0].check_ids.clear();
        assert!(any(&diff_contracts(&a, &b).weakens, "fewer checks"));

        let mut b = a.clone();
        b.criteria[1].required_trust = TrustClass::ObservedInAttempt;
        assert!(any(&diff_contracts(&a, &b).weakens, "weaker evidence"));

        let mut b = a.clone();
        b.criteria[1].manual = false;
        assert!(any(&diff_contracts(&a, &b).weakens, "person's review"));

        let mut b = a.clone();
        b.criteria[0].text = "something easier".into();
        assert!(any(&diff_contracts(&a, &b).weakens, "text changed"));
    }

    /// Every field of the contract and its parts is read by `diff_contracts` through a destructuring
    /// without `..` (a new field does not compile until classified); this checks that each field the
    /// serialised contract has today moves the diff when changed.
    #[test]
    fn every_serialised_field_moves_the_diff() {
        let mut base = contract(&["c1"]);
        base.checks = vec![ContractCheck {
            id: "t".into(),
            command: "x".into(),
        }];
        base.envelope.quality_gates = vec!["g".into()];
        base.envelope.budgets.attempt_max_cost_usd = Some(1.0);
        base.envelope.budgets.attempt_max_wall_secs = Some(10);
        let v = serde_json::to_value(&base).unwrap();
        fn paths(v: &serde_json::Value, at: String, out: &mut Vec<String>) {
            match v {
                serde_json::Value::Object(m) => m
                    .iter()
                    .for_each(|(k, x)| paths(x, format!("{at}/{k}"), out)),
                serde_json::Value::Array(a) => a
                    .iter()
                    .enumerate()
                    .for_each(|(i, x)| paths(x, format!("{at}/{i}"), out)),
                _ => out.push(at),
            }
        }
        let mut leaves = vec![];
        paths(&v, String::new(), &mut leaves);
        for p in leaves.iter().filter(|p| *p != "/version") {
            let orig = v.pointer(p).unwrap().clone();
            let changed = match &orig {
                serde_json::Value::String(s) => serde_json::json!(format!("{s}x")),
                serde_json::Value::Bool(b) => serde_json::json!(!b),
                serde_json::Value::Number(_) => serde_json::json!(orig.as_f64().unwrap() + 1.0),
                o => o.clone(),
            };
            let mut w = v.clone();
            *w.pointer_mut(p).unwrap() = changed;
            let Ok(other) = serde_json::from_value::<Contract>(w) else {
                continue;
            };
            assert!(
                !diff_contracts(&base, &other).changes.is_empty(),
                "{p} does not move the diff"
            );
        }
    }
}
