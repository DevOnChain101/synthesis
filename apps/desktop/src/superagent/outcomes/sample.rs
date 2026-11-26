//! Sample data of the Super Agent view (`--sample`, the offscreen shots, the tests): protocol replies, run
//! through the same `synthesis_view::outcomes` parsers a live reply goes through, so a sample can only show what
//! the real models can show. Nothing here is sent anywhere.

use serde_json::{Value, json};
use synthesis_view::outcomes::{
    BudgetView, CriteriaView, DecisionCard, InspectorContent, InspectorTab, OutcomeRow,
    OverviewTab, Queue, Record,
};

use super::form::{DiffView, Review};
use super::model::{Detail, Item, QueueData, Screen, State};

/// The sample's workspace: an absolute path on the OS it runs on.
pub fn workspace() -> String {
    if cfg!(windows) {
        r"C:\dev\auth-service".into()
    } else {
        "/home/dev/auth-service".into()
    }
}

pub const COMMIT: &str = "9433c678e4b1d2f0a7c35e8b61d9f4a02c7b5e13";
const HASH: &str = "3c9a41be7d02f58a6e1c90d4b7a3582f1e60c9d47b2a8f35e0614cd9a7b3e5e1";

fn row(
    id: &str,
    goal: &str,
    state: &str,
    rev: i64,
    pending: u32,
    blocked: bool,
    ack: bool,
) -> Value {
    json!({"id": id, "goal": goal, "state": state, "revision": rev,
           "needs_you": {"decisions_pending": pending, "blocked": blocked, "verified_awaiting_ack": ack}})
}

pub(super) fn rows() -> Vec<Value> {
    vec![
        row(
            "o_auth",
            "Migrate auth to OIDC and keep the old API answering",
            "running",
            12,
            2,
            false,
            false,
        ),
        row(
            "o_ci",
            "Speed up the CI cache",
            "running",
            7,
            0,
            false,
            false,
        ),
        row("o_docs", "Docs link checker", "verified", 9, 0, false, true),
        row("o_cfg", "Rename config keys", "blocked", 5, 0, true, false),
    ]
}

pub(super) fn budget(id: &str) -> Value {
    match id {
        "o_auth" => {
            json!({"cost_usd": {"spent": 2.10, "limit": 5.00}, "wall_secs": {"spent": 1900}, "reserved_usd": 1.40})
        }
        "o_ci" => json!({"cost_usd": {"spent": 0.42, "limit": 3.00}, "reserved_usd": 0.0}),
        "o_cfg" => json!({"cost_usd": {"spent": 3.00, "limit": 3.00}, "reserved_usd": 0.0}),
        _ => json!({"cost_usd": {"spent": 0.88, "limit": 2.00}, "reserved_usd": 0.0}),
    }
}

pub fn contract() -> Value {
    json!({"version": 1, "hash": "7be2c4d19a3f06e85b71d2c4a98f3e105d6b7c82a4e91f03b5d7c6a8e2f14d4d", "approved": true,
        "contract": {"version": 1,
            "goal": "Move login to OIDC and keep the old API answering the same requests.",
            "criteria": [
                {"id": "c1", "text": "Login through OIDC works with the test IdP", "mandatory": true, "manual": false,
                 "required_trust": "independent_check", "check_ids": ["t1"]},
                {"id": "c2", "text": "The old API still answers 200 for the recorded requests", "mandatory": true, "manual": false,
                 "required_trust": "independent_check", "check_ids": ["t1", "t2"]},
                {"id": "c3", "text": "Integration tests pass against OIDC", "mandatory": false, "manual": true,
                 "required_trust": "human_review", "check_ids": []}],
            "checks": [{"id": "t1", "command": "cargo test -p auth"},
                       {"id": "t2", "command": "cargo clippy -p auth -- -D warnings"}],
            "envelope": {"allowed_paths": ["auth/**", "tests/**"],
                         "budgets": {"max_cost_usd": 5.0, "attempt_max_wall_secs": 1200}}}})
}

pub(super) const HASH_V2: &str = "a41f0c9e27b3d86510fe4a7c2d9b3e80651f7a4c9d02be3586f1a0c7e94d2b63";

/// The sample's revision (version 2): the scope is wider and a criterion is gone.
pub(super) fn contract_v2() -> Value {
    let mut v = contract();
    v["version"] = json!(2);
    v["hash"] = json!(HASH_V2);
    v["approved"] = json!(false);
    v["contract"]["version"] = json!(2);
    v["contract"]["envelope"]["allowed_paths"] = json!(["auth/**", "tests/**", "billing/**"]);
    v["contract"]["criteria"].as_array_mut().map(|a| a.pop());
    v
}

pub(super) fn diff_v2() -> Value {
    json!({"base": {"version": 1, "hash": "7be2c4d19a3f06e85b71d2c4a98f3e105d6b7c82a4e91f03b5d7c6a8e2f14d4d", "approved": true},
        "version": 2, "hash": HASH_V2, "approved": false,
        "diff": {"changes": [
            {"area": "envelope", "key": "allowed_paths", "old": "auth/**, tests/**", "new": "auth/**, tests/**, billing/**"},
            {"area": "criteria", "key": "c3", "old": "Integration tests pass against OIDC", "new": null}],
          "widens": ["allowed_paths: billing/**"],
          "weakens": ["criterion c3 removed"]},
        "widens_scope": true})
}

/// What the form's review stage shows after a create (`revision` false: the first version, no diff) or a
/// revise (the server's diff, with its widening and weakening warnings).
pub fn review(revision: bool) -> Review {
    let (v, diff) = if revision {
        (
            contract_v2(),
            Some(DiffView::from_json(&diff_v2(), 2, HASH_V2)),
        )
    } else {
        (contract(), None)
    };
    Review {
        outcome: "o_auth".into(),
        version: if revision { 2 } else { 1 },
        hash: v["hash"].as_str().unwrap_or_default().into(),
        contract: CriteriaView::from_json(&v),
        diff,
        key: "approve-sample".into(),
    }
}

fn brief_scope() -> Value {
    json!({
        "why_human": "The worker needs your answer before it can plan the last step.",
        "delta_from_contract": "Add work item w5: run the OIDC integration tests",
        "alternatives": [
            {"id": "approve_with_w5", "label": "approve_with_w5", "tradeoffs": "covers c3, about $0.40 more"},
            {"id": "approve_without_c3", "label": "approve_without_c3", "tradeoffs": "no c3 check; c3 stays unmet"}],
        "evidence_refs": {"work_item": "w3", "attempt_ordinal": 3, "refused_calls": ["Bash: curl http://idp.test"]},
        "unknowns": ["is the test IdP reachable from the sandbox?"],
        "defer_consequence": "The outcome stays blocked until you decide or the decision expires.",
        "safe_default": "HOLD",
        "risk_note": "shown as the server sent it"})
}

fn brief_contract() -> Value {
    json!({
        "why_human": "A revised contract needs your approval before it replaces v1.",
        "delta_from_contract": "Raise the timeout of the test check from 10 to 30 minutes",
        "alternatives": [
            {"id": "approve_v2", "label": "approve_v2", "tradeoffs": "longer runs, fewer false failures"},
            {"id": "keep_v1", "label": "keep_v1", "tradeoffs": "flaky failures stay"}],
        "unknowns": [],
        "defer_consequence": "v1 stays in force."})
}

pub(super) fn card(id: &str, kind: &str, state: &str, rev: i64, brief: Value) -> Value {
    json!({"id": id, "kind": kind, "state": state, "revision": rev, "brief_sha256": HASH,
           "expires_at": "2026-11-18T09:30:00.000Z", "brief": brief})
}

pub(super) fn cards_json() -> Vec<Value> {
    vec![
        card("d_7f3a", "plan_change", "pending", 4, brief_scope()),
        card("d_91bc", "contract_review", "pending", 2, brief_contract()),
        card(
            "d_44e0",
            "ambiguity",
            "approved",
            3,
            json!({"why_human": "Which API shape?", "alternatives": [{"id": "REST", "label": "REST"}]}),
        ),
    ]
}

pub fn cards() -> Vec<DecisionCard> {
    cards_json()
        .iter()
        .map(|c| DecisionCard::from_json(c).expect("sample card"))
        .collect()
}

/// An `approvals.queue` reply and a recorded answer, for the home's shot and tests. It is not part of [`state`]:
/// the other views' samples keep one row per outcome.
pub fn queue() -> QueueData {
    let row = |outcome: &str,
               goal: &str,
               source: &str,
               id: &str,
               kind: &str,
               tier: &str,
               at: &str,
               held: &[&str],
               waiting: u64| {
        json!({"outcome": outcome, "goal": goal, "source": source, "decision_id": id, "kind": kind,
            "state": if source == "lapsed" { "expired" } else { "pending" }, "tier": tier, "revision": 2,
            "brief_sha256": HASH_V2, "hash": HASH_V2, "version": 2, "created_at": at,
            "expires_at": "2023-11-15T21:00:00.000Z", "one_tap": false, "batchable": false,
            "blocks": held.iter().map(|k| json!({"id": k, "key": k})).collect::<Vec<_>>(),
            "waiting_on_it": waiting})
    };
    let auth = "Migrate auth to OIDC and keep the old API answering";
    // the sample clock (`SAMPLE_NOW`) is 2023-11-14T22:13:20Z
    let rows = json!({"rows": [
        row("o_cfg", "Rename config keys", "contract", "", "contract", "T3", "2023-11-14T21:10:00.000Z", &[], 0),
        row("o_auth", auth, "decision", "d_7f3a", "approval_needed", "T2", "2023-11-14T22:04:20.000Z", &["w3"], 2),
        row("o_auth", auth, "decision", "d_91bc", "blocked", "T1", "2023-11-14T22:01:20.000Z", &["w5"], 0),
        row("o_ci", "Speed up the CI cache", "lapsed", "d_0a11", "ambiguity", "T0", "2023-11-13T09:00:00.000Z", &[], 0),
    ]});
    let record = json!({"event_id": 9, "ts": "2023-11-14T20:13:20.000Z", "type": "decision.resolved",
        "payload": {"decision_id": "d_44e0", "choice": "retry", "approver": "user:cli",
                    "audit": {"mechanism": "cli", "tier": "T1"}}});
    QueueData {
        queue: Queue::from_json(&rows).expect("sample queue"),
        answered: Record::from_json(&record)
            .into_iter()
            .map(|r| ("o_auth".to_string(), r))
            .collect(),
    }
}

pub(super) fn plan() -> Value {
    let item = |id: &str, key: &str, state: &str, requires: &[&str]| json!({"id": id, "key": key, "kind": "code", "state": state, "revision": 3, "retries_used": 0, "requires": requires});
    json!({"plan_version": 2, "items": [
        item("wi1", "w1", "succeeded", &[]),
        item("wi2", "w2", "succeeded", &["w1"]),
        item("wi3", "w3", "running", &["w2"]),
        item("wi4", "w4", "created", &["w3"]),
        item("wi5", "w5", "waiting_decision", &["w3"])]})
}

pub(super) fn work_items() -> Vec<Value> {
    let at = |id: &str, n: i64, state: &str, ended: &str| {
        json!({"id": id, "ordinal": n, "session_id": format!("s_91c{n}"), "state": state, "task_status": "ok",
               "subject_commit": COMMIT, "started_at": "2026-11-17T07:12:00Z", "ended_at": ended,
               "sandbox": {"backend": "seatbelt", "guarantee": "weak"}})
    };
    let mut v: Vec<Value> = plan()["items"].as_array().cloned().unwrap_or_default();
    v[0]["attempts"] = json!([at("at1", 1, "finished", "2026-11-17T07:20:00Z")]);
    v[1]["attempts"] = json!([
        at("at2", 1, "lost", "2026-11-17T07:31:00Z"),
        at("at3", 2, "finished", "2026-11-17T07:44:00Z")
    ]);
    v[3]["attempts"] = json!([]);
    v[4]["attempts"] = json!([]);
    v[2]["attempts"] = json!([{"id": "at4", "ordinal": 1, "session_id": "s_91c2", "state": "running", "task_status": "working",
                                "started_at": "2026-11-17T08:05:00Z"}]);
    v
}

pub(super) fn evidence() -> Vec<Value> {
    vec![
        json!({"id": "ev1", "work_item_id": "wi1", "trust_class": "independent_check", "producer": "verifier", "subject_commit": COMMIT,
               "check_id": "t1", "criteria_ids": ["c1"], "result": "pass", "exit_code": 0, "flags": [], "captured_at": "2026-11-17T07:21:00Z"}),
        json!({"id": "ev2", "work_item_id": "wi2", "trust_class": "independent_check", "producer": "verifier", "subject_commit": COMMIT,
               "check_id": "t2", "criteria_ids": ["c2"], "result": "fail", "exit_code": 101, "flags": ["stale_checkout"], "captured_at": "2026-11-17T07:45:00Z"}),
    ]
}

pub(super) fn diff() -> Value {
    json!({"attempt": "at3", "base_commit": "5d1f0a9c2b7e4386a1d0f5c7e92b4a6803f1d7c5", "commit": COMMIT, "total_files": 3, "truncated": false,
        "files": [
            {"path": "auth/refresh.rs", "status": "M", "binary": false, "secret": false, "added": 6, "removed": 2,
             "patch": "@@ -10,7 +10,11 @@ fn refresh(token: &Token) -> Result<Token> {\n     let now = clock.now();\n-    if token.expired(now) {\n-        return Err(Error::Expired);\n+    if token.expires_within(now, SKEW) {\n+        let fresh = idp.refresh(&token.refresh)?;\n+        store.put(&fresh)?;\n+        return Ok(fresh);\n     }\n     Ok(token.clone())\n }", "patch_truncated": false},
            {"path": ".env", "status": "A", "binary": false, "secret": true, "added": 1, "removed": 0},
            {"path": "assets/logo.bin", "status": "A", "binary": true, "secret": false, "added": 0, "removed": 0}]})
}

pub(super) fn events() -> Vec<Value> {
    [
        "outcome.created",
        "contract.approved",
        "plan.installed",
        "attempt.started",
        "decision.requested",
    ]
    .iter()
    .enumerate()
    .map(
        |(i, t)| json!({"seq": i + 1, "type": t, "at": format!("2026-11-17T07:{:02}:00Z", 10 + i)}),
    )
    .collect()
}

pub fn inspector(tab: InspectorTab, item: &Item) -> InspectorContent {
    let r = match tab {
        InspectorTab::Overview => CriteriaView::from_json(&contract()).map(|criteria| {
            InspectorContent::Overview(Box::new(OverviewTab {
                outcome: item.row.clone(),
                criteria,
                budget: item.budget,
            }))
        }),
        InspectorTab::Workers => InspectorContent::workers(&plan(), &work_items()),
        InspectorTab::Activity => Ok(InspectorContent::activity(&events(), 5)),
        InspectorTab::Evidence => InspectorContent::evidence(&evidence()),
        InspectorTab::Diff => InspectorContent::diff_from(&diff()),
    };
    InspectorContent::or_unavailable(tab, r)
}

pub fn detail(item: &Item) -> Detail {
    let main = item.row.id == "o_auth";
    Detail {
        outcome: item.row.id.clone(),
        cards: Ok(if main { cards() } else { vec![] }),
        current: serde_json::from_value(contract()["contract"].clone()).ok(),
        contract: CriteriaView::from_json(&contract()),
        history: InspectorContent::activity(&events(), 5),
        workers: InspectorContent::or_unavailable(
            InspectorTab::Workers,
            InspectorContent::workers(&plan(), &work_items()),
        ),
        messages: vec![],
    }
}

/// The Ready state of the sample: four outcomes, the first selected with its decisions.
pub fn state() -> State {
    let items: Vec<Item> = rows()
        .iter()
        .map(|r| Item {
            row: OutcomeRow::from_json(r).expect("sample row"),
            budget: BudgetView::from_json(&budget(r["id"].as_str().unwrap_or_default())),
        })
        .collect();
    let mut s = State::new(Screen::Ready);
    s.workspace = workspace();
    s.selected = Some(items[0].row.id.clone());
    let (d, i) = (
        detail(&items[0]),
        inspector(InspectorTab::Overview, &items[0]),
    );
    s.apply_snapshot(items, None, Some(d), Some(i), 0);
    s
}

/// The sample outcome `id` selected (its own detail, the inspector tab asked for).
pub fn show(s: &mut State, id: &str, tab: InspectorTab) {
    if let Some(item) = s.items.iter().find(|i| i.row.id == id).cloned() {
        s.selected = Some(id.into());
        s.itab = tab;
        s.detail = Some(detail(&item));
        s.inspector = Some(inspector(tab, &item));
        s.active = 0;
        s.apply_snapshot(
            s.items.clone(),
            None,
            s.detail.clone(),
            s.inspector.clone(),
            0,
        );
    }
}
