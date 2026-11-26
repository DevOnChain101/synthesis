//! The read side of "Needs you" (docs/specs/approvals.md section 6.1): `approvals.queue`,
//! `approvals.history` and the `blocked_by` of work items. Everything is derived from store rows on
//! each read, so a restart loses nothing; tiers come from `synthesis_orchestrator::tier`.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::{Value, json};
use synthesis_core::outcome::{DecisionState, OutcomeState};
use synthesis_events::Record;
use synthesis_events::outcome as ev;
use synthesis_harness::sanitize_for_display as clean;
use synthesis_orchestrator::tier::{self, CLASSIFIER_VERSION, Tier};
use synthesis_session::Reader;
use synthesis_session::outcome::{DecisionRecord, OutcomeRecord, WorkItemRecord};
use time::OffsetDateTime;
use time::format_description::FormatItem;

/// Most rows one reply carries (the counts cover every row).
const MAX_ROWS: usize = 1000;
/// Most held work items one row names (`blocks_total` counts them all).
const MAX_BLOCKS: usize = 20;
/// Most work items of a plan the dependency walk looks at.
const MAX_ITEMS: usize = 1000;
const MAX_TEXT: usize = 300;
/// Matching records per `approvals.history` page, and the events one page may read to find them.
const HISTORY_PAGE: usize = 200;
const SCAN_PAGE: usize = 1000;
const SCAN_PAGES: usize = 20;
/// How long an expired or invalidated decision stays listed.
const LAPSED: time::Duration = time::Duration::hours(24);

fn s<E: ToString>(e: E) -> String {
    e.to_string()
}

fn capped(t: &str) -> String {
    clean(t).chars().take(MAX_TEXT).collect()
}

/// The current plan of an outcome: its items and, for each item key, the keys it requires.
struct Plan {
    items: Vec<WorkItemRecord>,
    requires: BTreeMap<String, Vec<String>>,
}

fn plan_of(r: &Reader, o: &OutcomeRecord) -> Result<Plan, String> {
    let items: Vec<WorkItemRecord> = r
        .list_work_items(&o.id)
        .map_err(s)?
        .into_iter()
        .filter(|i| Some(i.plan_version) == o.plan_version)
        .take(MAX_ITEMS)
        .collect();
    let mut requires = BTreeMap::new();
    for i in &items {
        requires.insert(i.key.clone(), r.work_item_deps(&i.id).map_err(s)?);
    }
    Ok(Plan { items, requires })
}

/// The work items an open decision holds: the bound one, or every item of the plan for a plan
/// approval (it holds the whole outcome).
fn blocks<'a>(plan: &'a Plan, d: &DecisionRecord) -> Vec<&'a WorkItemRecord> {
    if d.kind == "plan_approval" {
        return plan.items.iter().collect();
    }
    let bound = d.binding.get("work_item_id").and_then(Value::as_str);
    plan.items
        .iter()
        .filter(|i| Some(i.id.as_str()) == bound)
        .collect()
}

/// How many items transitively require one of `blocked` (item keys). Linear in items and edges.
fn waiting_on(blocked: &BTreeSet<&str>, requires: &BTreeMap<String, Vec<String>>) -> usize {
    let mut dependents: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for (key, deps) in requires {
        for d in deps {
            dependents.entry(d).or_default().push(key);
        }
    }
    let mut seen = BTreeSet::new();
    let mut next: Vec<&str> = blocked.iter().copied().collect();
    while let Some(k) = next.pop() {
        for dependent in dependents.get(k).into_iter().flatten() {
            if seen.insert(*dependent) {
                next.push(dependent);
            }
        }
    }
    seen.len()
}

/// `blocked_by`: for each work item id of the current plan, the pending decisions that hold it.
pub(crate) fn blocked_by(
    r: &Reader,
    o: &OutcomeRecord,
) -> Result<BTreeMap<String, Vec<String>>, String> {
    let plan = plan_of(r, o)?;
    let mut by: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for d in r.list_decisions(&o.id).map_err(s)? {
        if d.row_state == DecisionState::Pending {
            for i in blocks(&plan, &d) {
                by.entry(i.id.to_string())
                    .or_default()
                    .push(d.id.to_string());
            }
        }
    }
    Ok(by)
}

/// The store's timestamps sort as text.
fn ts(t: OffsetDateTime) -> String {
    const FMT: &[FormatItem<'_>] = time::macros::format_description!(
        "[year]-[month]-[day]T[hour]:[minute]:[second].[subsecond digits:3]Z"
    );
    t.format(FMT).expect("format utc timestamp")
}

struct Row {
    tier: Tier,
    weight: usize,
    created_at: String,
    id: String,
    body: Value,
}

/// A row before the work of rating it: a pending or lapsed decision, or a waiting contract.
struct Ask {
    /// Index into the outcomes.
    o: usize,
    /// `None`: the outcome's contract waits.
    decision: Option<DecisionRecord>,
}

impl Ask {
    /// What stays when there are more than [`MAX_ROWS`]: waiting before lapsed, then the newest.
    fn keep<'a>(&'a self, outcomes: &'a [OutcomeRecord]) -> (bool, &'a str) {
        match &self.decision {
            Some(d) => (d.row_state == DecisionState::Pending, &d.created_at),
            None => (true, &outcomes[self.o].created_at),
        }
    }
}

/// The asks of one outcome. An `Err` is this outcome's alone.
fn asks_of(r: &Reader, i: usize, o: &OutcomeRecord, since: &str) -> Result<Vec<Ask>, String> {
    let mut asks = vec![];
    if o.state == OutcomeState::ContractPending {
        asks.push(Ask {
            o: i,
            decision: None,
        });
    }
    for d in r.list_decisions(&o.id).map_err(s)? {
        let lapsed = matches!(
            d.row_state,
            DecisionState::Expired | DecisionState::Invalidated
        ) && d.expires_at.as_str() >= since;
        if d.row_state == DecisionState::Pending || lapsed {
            asks.push(Ask {
                o: i,
                decision: Some(d),
            });
        }
    }
    Ok(asks)
}

fn rate(r: &Reader, o: &OutcomeRecord, plan: &Plan, ask: &Ask) -> Result<Row, String> {
    let Some(d) = &ask.decision else {
        let (_, hash, _) = r.get_contract(&o.id, o.contract_version).map_err(s)?;
        return Ok(row(
            o,
            Tier::T3,
            plan,
            plan.items.iter().collect(),
            o.created_at.clone(),
            o.id.to_string(),
            json!({"source": "contract", "kind": "contract", "state": "pending",
                   "version": o.contract_version, "hash": hash, "expires_at": null}),
        ));
    };
    let lapsed = d.row_state != DecisionState::Pending;
    let (tier, recommended, held) = if lapsed {
        (Tier::T0, None, vec![])
    } else {
        let a = tier::assess(r, o, d);
        (a.tier, a.recommended, blocks(plan, d))
    };
    let mut body = json!({
        "source": if lapsed { "lapsed" } else { "decision" },
        "decision_id": d.id, "kind": d.kind, "state": d.row_state,
        "revision": d.revision, "brief_sha256": d.brief_sha256, "expires_at": d.expires_at,
    });
    if let Some((id, label)) = recommended {
        body["recommended"] = json!({"id": capped(&id), "label": capped(&label)});
    }
    Ok(row(
        o,
        tier,
        plan,
        held,
        d.created_at.clone(),
        d.id.to_string(),
        body,
    ))
}

pub(crate) fn queue(r: &Reader, workspace: Option<&str>) -> Result<Value, String> {
    // shortcut: an invalidated row has no time of its own, so "the last 24 h" is judged by its
    // expiry (never later than 24 h after it was made); add a column if the Lapsed list gets noisy
    let since = ts(OffsetDateTime::now_utc() - LAPSED);
    let outcomes: Vec<OutcomeRecord> = r
        .list_outcomes()
        .map_err(s)?
        .into_iter()
        .filter(|o| workspace.is_none_or(|w| w == o.workspace_id))
        .collect();
    let (mut asks, mut unreadable) = (vec![], BTreeSet::new());
    for (i, o) in outcomes.iter().enumerate() {
        match asks_of(r, i, o, &since) {
            Ok(a) => asks.extend(a),
            Err(_) => {
                unreadable.insert(o.id.to_string());
            }
        }
    }
    // cut before the per-row reads
    asks.sort_by(|a, b| b.keep(&outcomes).cmp(&a.keep(&outcomes)));
    let waiting = asks.iter().filter(|a| a.keep(&outcomes).0).count();
    let omitted = asks.len().saturating_sub(MAX_ROWS);
    asks.truncate(MAX_ROWS);
    let mut plans = BTreeMap::new();
    let mut rows = vec![];
    for ask in &asks {
        let o = &outcomes[ask.o];
        let plan = plans.entry(ask.o).or_insert_with(|| plan_of(r, o));
        match plan
            .as_ref()
            .map_err(Clone::clone)
            .and_then(|p| rate(r, o, p, ask))
        {
            Ok(row) => rows.push(row),
            Err(_) => {
                unreadable.insert(o.id.to_string());
            }
        }
    }
    // highest tier first, then what holds the most work, then the oldest
    rows.sort_by(|a, b| {
        (b.tier, b.weight, &a.created_at, &a.id).cmp(&(a.tier, a.weight, &b.created_at, &b.id))
    });
    let count = |rows: &[&Row], t: Tier| rows.iter().filter(|r| r.tier == t).count();
    let all: Vec<&Row> = rows.iter().collect();
    let mut per: BTreeMap<&str, Vec<&Row>> = BTreeMap::new();
    for r in &all {
        if r.tier != Tier::T0 {
            per.entry(r.body["outcome"].as_str().unwrap_or_default())
                .or_default()
                .push(r);
        }
    }
    let per_outcome: Vec<Value> = per
        .iter()
        .map(|(o, rows)| {
            json!({"outcome": o, "t1": count(rows, Tier::T1), "t2": count(rows, Tier::T2),
                   "t3": count(rows, Tier::T3),
                   "oldest": rows.iter().map(|r| r.created_at.as_str()).min()})
        })
        .collect();
    Ok(json!({
        "classifier_version": CLASSIFIER_VERSION,
        // no mechanism is offered by this build (slices P2 to P4)
        "policy": {"one_tap": false, "batch_max": 0, "undo_seconds": 0},
        // the tiers count the listed rows; `waiting` counts every ask, listed or omitted
        "counts": {"t0": count(&all, Tier::T0), "t1": count(&all, Tier::T1),
                   "t2": count(&all, Tier::T2), "t3": count(&all, Tier::T3),
                   "waiting": waiting, "omitted": omitted},
        "unreadable": unreadable,
        "per_outcome": per_outcome,
        "rows": rows.iter().map(|r| &r.body).collect::<Vec<_>>(),
    }))
}

fn row(
    o: &OutcomeRecord,
    tier: Tier,
    plan: &Plan,
    held: Vec<&WorkItemRecord>,
    created_at: String,
    id: String,
    mut body: Value,
) -> Row {
    let keys: BTreeSet<&str> = held.iter().map(|i| i.key.as_str()).collect();
    let waiting = waiting_on(&keys, &plan.requires);
    body["outcome"] = json!(o.id);
    body["goal"] = json!(capped(&o.goal));
    body["tier"] = json!(tier.as_str());
    body["created_at"] = json!(created_at);
    body["one_tap"] = json!(false);
    body["batchable"] = json!(false);
    body["blocks"] = held
        .iter()
        .take(MAX_BLOCKS)
        .map(|i| json!({"id": i.id, "key": i.key}))
        .collect();
    body["blocks_total"] = json!(held.len());
    body["waiting_on_it"] = json!(waiting);
    Row {
        tier,
        weight: waiting + 1,
        created_at,
        id,
        body,
    }
}

/// The answers, reverts and auto decisions of one outcome after a cursor, from its control
/// session's chained events (payloads are ids, codes and hashes).
pub(crate) fn history(r: &Reader, o: &OutcomeRecord, after: u64) -> Result<Value, String> {
    let (mut records, mut next) = (vec![], after);
    'pages: for _ in 0..SCAN_PAGES {
        let page = r
            .read_events(&o.control_session_id, next, SCAN_PAGE)
            .map_err(s)?;
        if page.is_empty() {
            break;
        }
        for rec in page {
            let v = match rec {
                Record::Known(e) => serde_json::to_value(e),
                Record::Unknown(e) => serde_json::to_value(e),
            }
            .map_err(s)?;
            next = v["seq"].as_u64().unwrap_or(next);
            if matches!(
                v["type"].as_str(),
                Some(ev::DECISION_RESOLVED | ev::APPROVAL_REVERTED | ev::APPROVAL_AUTO_DECIDED)
            ) {
                records.push(
                    json!({"event_id": v["seq"], "ts": v["ts"], "type": v["type"],
                                    "payload": v["payload"]}),
                );
                if records.len() == HISTORY_PAGE {
                    break 'pages;
                }
            }
        }
    }
    Ok(json!({"records": records, "next_event_id": next}))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn deps(edges: &[(&str, &[&str])]) -> BTreeMap<String, Vec<String>> {
        edges
            .iter()
            .map(|(k, d)| (k.to_string(), d.iter().map(|d| d.to_string()).collect()))
            .collect()
    }

    #[test]
    fn waiting_counts_every_transitive_dependent_once() {
        // a diamond: w2 and w3 require w1, w4 requires both
        let d = deps(&[
            ("w1", &[]),
            ("w2", &["w1"]),
            ("w3", &["w1"]),
            ("w4", &["w2", "w3"]),
        ]);
        let n = |k: &[&str]| waiting_on(&k.iter().copied().collect(), &d);
        assert_eq!((n(&["w1"]), n(&["w2"]), n(&["w4"]), n(&[])), (3, 1, 0, 0));
        assert_eq!(n(&["w2", "w3"]), 1);
        // a chain of 1000 items is walked without recursion
        let chain: BTreeMap<String, Vec<String>> = (0..MAX_ITEMS)
            .map(|i| {
                let req = (i > 0).then(|| format!("w{}", i - 1));
                (format!("w{i}"), req.into_iter().collect())
            })
            .collect();
        assert_eq!(waiting_on(&["w0"].into(), &chain), MAX_ITEMS - 1);
    }
}
