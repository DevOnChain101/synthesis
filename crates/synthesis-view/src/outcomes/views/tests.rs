use super::*;
use crate::outcomes::uncovered;
use serde_json::json;

fn plan() -> Value {
    json!({"plan_version": 2, "items": [
        {"id": "wi_c", "key": "c", "kind": "task", "state": "ready", "revision": 3, "retries_used": 0, "requires": ["a", "b"], "future": {"x": "seen"}},
        {"id": "wi_a", "key": "a", "kind": "task", "state": "succeeded", "revision": 1, "retries_used": 1, "requires": []},
        {"id": "wi_b", "key": "b", "kind": "check", "state": "running", "revision": 2, "retries_used": 0, "requires": ["a"]}]})
}

fn contract() -> Value {
    json!({"version": 1, "hash": "h".repeat(8), "approved": true, "contract": {"version": 1, "goal": "g",
        "criteria": [{"id": "k1", "text": "tests pass", "mandatory": true, "manual": false, "required_trust": "independent_check", "check_ids": ["t"]}],
        "checks": [{"id": "t", "command": "cargo test"}],
        "envelope": {"budgets": {"max_cost_usd": 5.0, "attempt_max_wall_secs": 600}}}})
}

fn attempt() -> Value {
    json!({"id": "at_1", "ordinal": 1, "session_id": "s1", "state": "finished", "task_status": "done",
           "subject_commit": "abc", "retained_reason": null, "started_at": "2030-01-01", "ended_at": "2030-01-02"})
}

fn evidence() -> Value {
    json!({"id": "ev_1", "work_item_id": "wi_a", "trust_class": "independent_check", "producer": "verifier",
           "subject_commit": "abc", "check_id": "t", "criteria_ids": ["k1"], "result": "pass", "exit_code": 0,
           "flags": ["tests_weakened"], "captured_at": "2030-01-01"})
}

#[test]
fn plan_layers_follow_the_servers_edges_and_the_servers_order() {
    let p = PlanView::from_json(&plan()).unwrap();
    assert!(!p.cycle && p.unknown_requires.is_empty());
    let l: Vec<Vec<&str>> = p
        .layers()
        .iter()
        .map(|l| l.iter().map(|w| w.key.as_str()).collect())
        .collect();
    assert_eq!(l, [vec!["a"], vec!["b"], vec!["c"]]);
    // the states are the server's, the order of `items` is the server's
    assert_eq!(p.items[0].key, "c");
    assert_eq!(p.items[0].state, "ready");
    assert_eq!(p.items[0].extra[0].label, "server extra: future");
    assert_eq!(uncovered(&plan(), &p.render_text()), Vec::<String>::new());
}

#[test]
fn a_cycle_shows_no_layers_and_bad_plans_are_refused() {
    let mut v = plan();
    v["items"][1]["requires"] = json!(["c"]);
    let p = PlanView::from_json(&v).unwrap();
    assert!(p.cycle && p.layers().is_empty() && p.items.iter().all(|w| w.layer.is_none()));
    assert!(p.render_text().contains("cycle"));
    let mut v = plan();
    v["items"][0]["requires"] = json!(["c"]); // self edge
    assert!(PlanView::from_json(&v).unwrap().cycle);
    // dangling requirement stays visible, not followed
    let mut v = plan();
    v["items"][1]["requires"] = json!(["ghost"]);
    let p = PlanView::from_json(&v).unwrap();
    assert!(p.items.iter().all(|w| w.layer.is_none()));
    assert!(
        p.render_text().contains("a requires ghost") && p.render_text().contains("layer unknown")
    );
    assert!(!p.cycle && p.unknown_requires == [(1, "ghost".to_string())]);
    // duplicate id, duplicate key, bad id, missing state, no list, too many
    let mut v = plan();
    v["items"][1]["id"] = json!("wi_c");
    assert!(PlanView::from_json(&v).unwrap_err().contains("twice"));
    let mut v = plan();
    v["items"][1]["key"] = json!("c");
    assert!(PlanView::from_json(&v).unwrap_err().contains("twice"));
    for bad in ["a\u{1b}[31m", "a b", ""] {
        let mut v = plan();
        v["items"][1]["id"] = json!(bad);
        assert!(PlanView::from_json(&v).is_err(), "{bad:?}");
    }
    let mut v = plan();
    v["items"][1].as_object_mut().unwrap().remove("state");
    assert!(PlanView::from_json(&v).is_err());
    assert!(PlanView::from_json(&json!({})).is_err());
    let many: Vec<Value> = (0..=MAX_ROWS)
        .map(|i| json!({"id": format!("w{i}"), "state": "ready"}))
        .collect();
    assert!(PlanView::from_json(&json!({"items": many})).is_err());
}

#[test]
fn hostile_text_is_printable_and_bounded_and_nothing_is_preselected() {
    let mut v = plan();
    v["items"][0]["state"] = json!("run\u{1b}[2Jning\u{202e}");
    v["items"][0]["kind"] = json!("x".repeat(100_000));
    v["items"][0]["recommended"] = json!("wi_a"); // an unknown field is shown, never acted on
    let p = PlanView::from_json(&v).unwrap();
    let w = &p.items[0];
    assert!(!w.state.contains('\u{1b}') && !w.state.contains('\u{202e}'));
    assert!(w.kind.len() < 5000 && w.kind.ends_with("[truncated]"));
    assert!(
        w.extra
            .iter()
            .any(|r| r.label.contains("ignored") && r.label.contains("recommended"))
    );
    // no cursor, no selection, no verdict field exists on any view
    let s = format!("{p:?}");
    assert!(!s.contains("selected") && !s.contains("verdict"));
}

#[test]
fn criteria_golden_and_coverage() {
    let c = CriteriaView::from_json(&contract()).unwrap();
    assert_eq!(c.criteria[0].id, "k1");
    assert_eq!((c.version, c.approved), (Some(1), Some(true)));
    assert_eq!(c.checks, ["t: cargo test"]);
    let shown = c.render_text();
    assert!(shown.contains("tests pass") && shown.contains("independent_check"));
    // the envelope is not criteria: only the fields this view owns are required to appear
    let mut v = contract();
    v["contract"].as_object_mut().unwrap().remove("envelope");
    assert_eq!(
        uncovered(&v, &CriteriaView::from_json(&v).unwrap().render_text()),
        Vec::<String>::new()
    );
    // a field this build does not know stays visible
    let mut v = contract();
    v["contract"]["criteria"][0]["waived"] = json!("by someone");
    assert!(
        CriteriaView::from_json(&v)
            .unwrap()
            .render_text()
            .contains("by someone")
    );
    // duplicate or bad ids, missing contract
    let mut v = contract();
    v["contract"]["criteria"] = json!([{"id": "k"}, {"id": "k"}]);
    assert!(CriteriaView::from_json(&v).is_err());
    assert!(CriteriaView::from_json(&json!({"version": 1})).is_err());
}

#[test]
fn attempt_sandbox_is_unknown_unless_the_server_says() {
    let a = AttemptView::from_json(&attempt(), "wi_a").unwrap();
    assert_eq!(
        (a.sandbox.backend.as_str(), a.sandbox.guarantee.as_str()),
        ("unknown", "unknown")
    );
    assert_eq!(a.sandbox.source, SandboxSource::NotProvided);
    assert_eq!(a.state, "finished");
    assert_eq!(
        uncovered(&attempt(), &a.render_text()),
        Vec::<String>::new()
    );
    let mut v = attempt();
    v["sandbox"] = json!({"backend": "seatbelt", "guarantee": "enforcing"});
    let a = AttemptView::from_json(&v, "wi_a").unwrap();
    assert_eq!(uncovered(&v, &a.render_text()), Vec::<String>::new());
    let inspect = json!({"work_item_id": "wi_a", "attempt": attempt()});
    assert_eq!(
        AttemptView::from_inspect(&inspect).unwrap().work_item_id,
        "wi_a"
    );
    let wi = json!({"id": "wi_a", "attempts": [attempt(), attempt()]});
    assert_eq!(AttemptView::list_from_work_item(&wi).unwrap().len(), 2);
    assert!(AttemptView::from_json(&json!({"id": "a b", "state": "x"}), "w").is_err());
    assert!(AttemptView::from_json(&json!({"id": "a"}), "w").is_err());
}

#[test]
fn evidence_flag_is_the_servers_and_never_computed() {
    let e = EvidenceView::from_json(&evidence()).unwrap();
    assert_eq!(e.event_backed, None);
    assert!(
        e.render_text().contains("event-backed unknown")
            && e.render_text().contains("tests_weakened")
    );
    assert_eq!(
        uncovered(&evidence(), &e.render_text()),
        Vec::<String>::new()
    );
    for b in [true, false] {
        let mut v = evidence();
        v["event_backed"] = json!(b);
        let e = EvidenceView::from_json(&v).unwrap();
        assert_eq!(e.event_backed, Some(b));
        assert_eq!(uncovered(&v, &e.render_text()), Vec::<String>::new());
    }
    // a non-boolean claim is not a flag; it is kept visible as it was sent... as unknown
    let mut v = evidence();
    v["event_backed"] = json!("yes");
    assert_eq!(EvidenceView::from_json(&v).unwrap().event_backed, None);
    // result is shown as recorded, whatever it is
    let mut v = evidence();
    v["result"] = json!("pass\u{7}");
    assert!(
        !EvidenceView::from_json(&v)
            .unwrap()
            .result
            .contains('\u{7}')
    );
    assert!(EvidenceView::from_json(&json!({"id": "e"})).is_err());
}

#[test]
fn budget_is_as_given_and_remaining_is_never_computed() {
    let b = BudgetView::from_json(
        &json!({"cost_usd": {"spent": 1.5, "limit": 5.0}, "tokens": {"spent": 10}, "wall_secs": {"limit": -1, "remaining": 3}}),
    );
    assert_eq!(
        b.cost_usd,
        Amount {
            spent: Some(1.5),
            limit: Some(5.0),
            remaining: None
        }
    );
    assert_eq!(b.tokens.limit, None);
    assert_eq!(
        b.wall_secs,
        Amount {
            spent: None,
            limit: None,
            remaining: Some(3.0)
        }
    );
    let s = b.render_text();
    assert!(
        s.contains("cost_usd spent 1.5 limit 5 remaining unknown"),
        "{s}"
    );
    let c = BudgetView::from_contract(&contract());
    assert_eq!(
        (c.cost_usd.limit, c.wall_secs.limit, c.cost_usd.spent),
        (Some(5.0), Some(600.0), None)
    );
    assert_eq!(BudgetView::from_json(&json!("x")), BudgetView::default());
}

#[test]
fn inspector_tabs_carry_content_or_an_explicit_reason() {
    let outcome = json!({"id": "o1", "goal": "g", "state": "running", "revision": 4,
        "needs_you": {"decisions_pending": 0, "blocked": false, "verified_awaiting_ack": false}});
    let o = InspectorContent::overview(&outcome, &contract()).unwrap();
    assert_eq!(o.tab(), InspectorTab::Overview);
    assert!(o.render_text().contains("tests pass") && o.render_text().contains("o1"));
    let wi = json!({"id": "wi_a", "key": "a", "state": "succeeded", "attempts": [attempt()]});
    let w = InspectorContent::workers(&plan(), &[wi]).unwrap();
    assert!(w.render_text().contains("at_1") && w.render_text().contains("wi_c"));
    let a = InspectorContent::activity(&[json!({"seq": 7, "type": "x\u{1b}[0m"})], 8);
    assert!(a.render_text().contains("7  x") && !a.render_text().contains('\u{1b}'));
    let e = InspectorContent::evidence(&[evidence()]).unwrap();
    assert!(e.render_text().contains("ev_1"));
    let d = InspectorContent::diff();
    assert_eq!(d.tab(), InspectorTab::Diff);
    assert!(
        d.render_text().contains("not available") && d.render_text().contains("no diff method")
    );
    // a failed read is a reason, never an empty page
    let u = InspectorContent::or_unavailable(
        InspectorTab::Workers,
        InspectorContent::workers(&json!({}), &[]),
    );
    assert_eq!(u.tab(), InspectorTab::Workers);
    assert!(
        matches!(u, InspectorContent::Unavailable { .. })
            && u.render_text().contains("no items list")
    );
    assert!(InspectorContent::evidence(&vec![json!({}); MAX_ROWS + 1]).is_err());
}

#[test]
fn amplification_input_is_bounded() {
    let mut v = plan();
    v["items"][0]["key"] = json!("k".repeat(MAX_KEY + 1));
    assert!(PlanView::from_json(&v).unwrap_err().contains("longer"));
    let req: Vec<String> = (0..10)
        .map(|i| format!("{}{i}", "r".repeat(MAX_KEY - 2)))
        .collect();
    let items: Vec<Value> = (0..1000)
        .map(|i| json!({"id": format!("w{i}"), "key": format!("{}{i}", "k".repeat(200)), "state": "ready", "requires": req}))
        .collect();
    let t0 = std::time::Instant::now();
    let p = PlanView::from_json(&json!({"items": items})).unwrap();
    assert_eq!(
        (p.unknown_requires.len(), p.unknown_requires_more),
        (MAX_ROWS, 9000)
    );
    let out = p.render_text();
    assert!(
        out.contains("9000 more unknown") && out.len() < 5_000_000,
        "{}",
        out.len()
    );
    assert!(t0.elapsed().as_secs() < 5);
    let items: Vec<Value> = (0..1000)
        .map(|i| json!({"id": format!("w{i}"), "key": format!("k{i}"), "state": "x", "requires": vec!["k0"; 11]}))
        .collect();
    assert!(
        PlanView::from_json(&json!({"items": items}))
            .unwrap_err()
            .contains("more than")
    );
}

#[test]
fn a_long_chain_lays_out_right() {
    let items: Vec<Value> = (0..1000)
        .map(|i| {
            json!({"id": format!("w{i}"), "key": format!("k{i}"), "state": "x",
            "requires": if i == 0 { vec![] } else { vec![format!("k{}", i - 1)] }})
        })
        .collect();
    let p = PlanView::from_json(&json!({"items": items})).unwrap();
    assert!(!p.cycle && p.items[999].layer == Some(999) && p.layers().len() == 1000);
}

#[test]
fn extra_rows_are_capped_and_have_no_heading_authority() {
    let mut o = serde_json::Map::new();
    for i in 0..300 {
        o.insert(format!("f{i}"), json!("v".repeat(100_000)));
    }
    let rows = extra(&Value::Object(o), &[]);
    assert_eq!(rows.len(), MAX_EXTRA + 1);
    assert!(rows.iter().all(|r| r.text.len() < MAX_TEXT + 20));
    assert!(rows.last().unwrap().text.contains("more fields"));
    let rows = extra(
        &json!({"safe_default": "HOLD", "evidence": "e", "why": "w"}),
        &[],
    );
    let labels: Vec<_> = rows.iter().map(|r| r.label.as_str()).collect();
    assert_eq!(
        labels,
        [
            "server extra: safe_default",
            "server extra: evidence",
            "server extra: why"
        ]
    );
}

#[test]
fn sandbox_is_marked_server_reported_and_a_bad_one_is_unreadable() {
    let mut v = attempt();
    v["sandbox"] = json!({"backend": "seatbelt", "guarantee": "enforcing", "profile": "p1"});
    let a = AttemptView::from_json(&v, "wi_a").unwrap();
    assert_eq!(a.sandbox.source, SandboxSource::ServerReported);
    let shown = a.render_text();
    assert!(
        shown.contains("server-reported, not verified by this surface") && shown.contains("p1"),
        "{shown}"
    );
    v["sandbox"] = json!("enforcing");
    let a = AttemptView::from_json(&v, "wi_a").unwrap();
    assert_eq!(a.sandbox.source, SandboxSource::Unreadable);
    assert!(a.render_text().contains("unreadable"));
    let mut v = attempt();
    v["work_item_id"] = json!("wi_other");
    assert!(AttemptView::from_json(&v, "wi_a").is_err());
}

#[test]
fn goal_attempts_and_inconsistent_remaining_are_shown() {
    let c = CriteriaView::from_json(&contract()).unwrap();
    assert_eq!(c.goal, "g");
    assert!(c.render_text().contains("Goal: g"));
    let mut v = plan();
    v["items"][1]["attempts"] = json!([{"id": "at_1"}, {"id": "at_2"}]);
    let p = PlanView::from_json(&v).unwrap();
    assert_eq!(p.items[1].attempts_listed, Some(2));
    assert!(p.render_text().contains("attempts 2"));
    let b =
        BudgetView::from_json(&json!({"cost_usd": {"spent": 1.0, "limit": 5.0, "remaining": 9.0}}));
    assert_eq!(b.cost_usd.remaining, Some(9.0));
    assert!(b.cost_usd.remaining_disagrees() && b.render_text().contains("does not match"));
    let b =
        BudgetView::from_json(&json!({"cost_usd": {"spent": 1.0, "limit": 5.0, "remaining": 4.0}}));
    assert!(!b.cost_usd.remaining_disagrees());
}
