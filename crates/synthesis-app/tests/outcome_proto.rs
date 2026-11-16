//! Protocol v4 outcome methods through the client library (plan PR-11, threat T-24, T-31, T-35):
//! the server in the same process (`InProc`, tests only), the owner step run by hand where a real
//! orchestrator process will run it (PR-12), and a scripted hostile engine over `Stdio`.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::io::{Cursor, Write};
use std::rc::Rc;

use serde_json::{Value, json};
use synthesis_app::outcome_client::{ClientError, InProc, OutcomeClient, Stdio, Transport};
use synthesis_app::outcome_service::Service;
use synthesis_app::proto::{Call, PROTOCOL, VERSION};
use synthesis_core::WorkspaceContext;
use synthesis_core::outcome::*;
use synthesis_orchestrator::{apply_queued, compile_envelope, decision_binding};
use synthesis_session::outcome::*;
use synthesis_session::{SCHEMA_VERSION_OUTCOMES, Store, StoreOptions};
use time::OffsetDateTime;

struct Rig {
    _tmp: tempfile::TempDir,
    home: std::path::PathBuf,
    store: Store,
    w: Option<OutcomeWriter>,
    outcome: String,
    item: WorkItemId,
}

fn contract() -> Contract {
    let mut caps = BTreeMap::new();
    caps.insert(Capability::RepoRead, CapabilityMode::Allow);
    caps.insert(Capability::RepoWrite, CapabilityMode::Allow);
    Contract {
        version: 1,
        goal: "goal".into(),
        criteria: vec![ContractCriterion {
            id: "c1".into(),
            text: "c1".into(),
            mandatory: true,
            manual: false,
            required_trust: TrustClass::IndependentCheck,
            check_ids: vec![],
        }],
        envelope: Envelope {
            allowed_paths: vec!["src/**".into()],
            denied_paths: vec![],
            capabilities: caps,
            budgets: Budgets::new(5.0),
            quality_gates: vec![],
        },
        checks: vec![],
    }
}

fn plan() -> Plan {
    Plan {
        version: 1,
        contract_version: 1,
        items: vec![
            PlanItem {
                key: "one".into(),
                kind: WorkItemKind::Agent,
                requires: vec![],
                capabilities: vec![],
                covers: vec![],
            },
            PlanItem {
                key: "test".into(),
                kind: WorkItemKind::Verify,
                requires: vec!["one".into()],
                capabilities: vec![],
                covers: vec!["c1".into()],
            },
        ],
    }
}

fn stamp(hours: i64) -> String {
    let fmt = time::macros::format_description!(
        "[year]-[month]-[day]T[hour]:[minute]:[second].[subsecond digits:3]Z"
    );
    (OffsetDateTime::now_utc() + time::Duration::hours(hours))
        .format(&fmt)
        .unwrap()
}

fn later() -> OffsetDateTime {
    OffsetDateTime::now_utc() + time::Duration::hours(3)
}

/// A running outcome whose only work item waits for a decision.
fn rig() -> Rig {
    let tmp = tempfile::tempdir().unwrap();
    let base = synthesis_core::canonicalize(tmp.path()).unwrap();
    let repo = base.join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    let home = base.join("home");
    let store = Store::open(
        &home,
        StoreOptions {
            schema_target: SCHEMA_VERSION_OUTCOMES,
            ..StoreOptions::default()
        },
    )
    .unwrap();
    let ws = WorkspaceContext::open("w", &repo, &repo, &[], base.join("cart"), "pg").unwrap();
    let mut w = store
        .create_outcome(NewOutcome {
            workspace: &ws,
            base_commit: "c0ffee".into(),
            contract: &contract(),
            policy: &OutcomePolicy::new(true, 16, 10, 24),
        })
        .unwrap();
    let none = Effects::default();
    let go = |w: &mut OutcomeWriter, to| {
        let r = w.get_outcome(w.id()).unwrap().revision;
        w.transition_outcome(r, to, None, &none).unwrap();
    };
    go(&mut w, OutcomeState::ContractPending);
    let h = w.get_contract(w.id(), 1).unwrap().1;
    w.approve_contract(1, &h, "user").unwrap();
    let items = w.insert_plan(&plan()).unwrap();
    go(&mut w, OutcomeState::Ready);
    go(&mut w, OutcomeState::Running);
    let item = items.iter().find(|(k, _)| k == "one").unwrap().1.clone();
    for to in [
        WorkItemState::Ready,
        WorkItemState::Leased,
        WorkItemState::Running,
        WorkItemState::WaitingDecision,
    ] {
        let rev = w.get_work_item(&item).unwrap().revision;
        w.transition_work_item(&item, rev, to, &none).unwrap();
    }
    Rig {
        _tmp: tmp,
        home,
        store,
        outcome: w.id().to_string(),
        w: Some(w),
        item,
    }
}

impl Rig {
    fn w(&self) -> &OutcomeWriter {
        self.w.as_ref().unwrap()
    }

    fn wm(&mut self) -> &mut OutcomeWriter {
        self.w.as_mut().unwrap()
    }

    /// The owner's clock moves (the writer is reopened: one writer per outcome).
    fn set_clock(&mut self, clock: fn() -> OffsetDateTime) {
        let id = self.w().id().clone();
        self.w = None;
        let w = self
            .store
            .open_outcome(&id, &OutcomePolicy::new(true, 16, 10, 24))
            .unwrap();
        self.w = Some(w.with_clock(clock));
    }

    fn client(&self, surface: Surface) -> OutcomeClient<InProc> {
        OutcomeClient::new(InProc(Service::new(&self.home, true)), surface)
    }

    /// A pending decision; `tweak` moves its binding (a binding that no longer matches is invalidated).
    fn decide(&mut self, key: &str, hours: i64, tweak: impl Fn(&mut Value)) {
        let o = self.w().get_outcome(self.w().id()).unwrap();
        let (c, ..) = self.w().get_contract(self.w().id(), 1).unwrap();
        let policy = compile_envelope(&c.envelope, &[]).unwrap().policy_hash();
        let mut b = decision_binding(1, o.plan_version.unwrap(), None, &policy, "", 0, &self.item);
        tweak(&mut b);
        let (d, _) = self
            .wm()
            .new_decision(&NewDecision {
                kind: "ambiguity".into(),
                dedupe_key: key.into(),
                brief: json!({"why": "the worker needs an answer", "delta_from_contract": "Which API shape?",
                    "alternatives": [{"id": "REST", "label": "REST"}, {"id": "RPC", "label": "RPC"}],
                    "unknowns": [], "safe_default": "HOLD"}),
                binding: b,
                expires_at: stamp(hours),
                action: None,
            })
            .unwrap();
        self.wm()
            .transition_decision(
                &d.id,
                1,
                DecisionState::Pending,
                &d.binding,
                &Effects::default(),
            )
            .unwrap();
    }

    /// What the orchestrator process does on its tick (PR-12): apply the queued commands.
    fn owner(&mut self) -> Vec<CommandRecord> {
        apply_queued(self.wm(), OffsetDateTime::now_utc()).unwrap()
    }

    fn card(&self, c: &mut OutcomeClient<InProc>) -> synthesis_view::outcomes::DecisionCard {
        c.decisions(&self.outcome).unwrap().remove(0)
    }
}

#[test]
fn off_by_default_every_method_but_status_is_refused_and_nothing_is_created() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let mut c = OutcomeClient::new(InProc(Service::new(&home, false)), Surface::Cli);
    let st = c.status().unwrap();
    assert_eq!(
        (st["enabled"].clone(), st["ready"].clone()),
        (json!(false), json!(false))
    );
    assert_eq!(st["protocol"], PROTOCOL);
    assert!(matches!(c.outcomes(None), Err(ClientError::Refused(e)) if e.contains("off")));
    assert!(
        c.command("start", "11111111-1111-4111-8111-111111111111", None, "k")
            .is_err()
    );
    assert!(!home.exists(), "a disabled service never touches the store");
}

#[test]
fn enabled_on_a_v4_store_refuses_and_reports_not_ready() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    Store::open(&home, StoreOptions::default()).unwrap();
    let mut c = OutcomeClient::new(InProc(Service::new(&home, true)), Surface::Cli);
    assert_eq!(c.status().unwrap()["ready"], false);
    assert!(c.outcomes(None).is_err());
}

#[test]
fn reads_show_the_outcome_with_needs_you_and_the_decision_as_the_server_states_it() {
    let mut r = rig();
    r.decide("d", 2, |_| {});
    let mut c = r.client(Surface::Cli);
    let rows = c.outcomes(None).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].id, r.outcome);
    assert_eq!(rows[0].needs_you.decisions_pending, 1);
    assert_eq!(c.outcome(&r.outcome).unwrap().state, "running");
    let card = r.card(&mut c);
    assert!(card.resolvable() && card.alternatives.len() == 2);
    let (events, next) = c.events(&r.outcome, 0).unwrap();
    assert!(!events.is_empty() && next > 0);
    // resuming from the cursor returns only what is new
    assert!(c.events(&r.outcome, next).unwrap().0.is_empty());
}

// proto-resolve-race
#[test]
fn proto_resolve_race_two_surfaces_one_effect_one_already_resolved() {
    let mut r = rig();
    r.decide("d", 2, |_| {});
    let (mut cli, mut tui) = (r.client(Surface::Cli), r.client(Surface::Tui));
    let card = r.card(&mut cli);
    let a = cli
        .resolve(&r.outcome, &card, "REST", "cli-1", None)
        .unwrap();
    let b = tui
        .resolve(&r.outcome, &card, "RPC", "tui-1", None)
        .unwrap();
    assert_eq!(
        (a.state, b.state),
        (CommandState::Queued, CommandState::Queued)
    );
    assert_eq!(r.owner().len(), 2);
    let a = cli.command_status(&r.outcome, "cli-1").unwrap();
    let b = tui.command_status(&r.outcome, "tui-1").unwrap();
    assert_eq!(a.state, CommandState::Applied);
    assert_eq!(b.state, CommandState::AlreadyResolved, "{b:?}");
    let d = &r.w().list_decisions(r.w().id()).unwrap()[0];
    assert_eq!(
        r.w().decision_approvals(&d.id).unwrap().len(),
        1,
        "one approval row"
    );
    assert_eq!(r.w().decision_approvals(&d.id).unwrap()[0].2, "REST");
    assert_eq!(
        r.w().verified_resolution(&d.id).unwrap().unwrap().approver,
        "user:cli"
    );
    // the loser refetches and sees the server's state; it cannot answer again
    let card = r.card(&mut tui);
    assert_eq!(card.state, "approved");
    assert!(
        tui.resolve(&r.outcome, &card, "RPC", "tui-2", None)
            .is_err()
    );
}

// proto-resolve-vs-invalidate
#[test]
fn proto_resolve_vs_invalidate_is_stale_and_nothing_is_approved() {
    let mut r = rig();
    // the plan moved since the decision was made: its binding no longer matches
    r.decide("d", 2, |b| b["plan_version"] = json!(9));
    let mut c = r.client(Surface::Cli);
    let card = r.card(&mut c);
    assert_eq!(card.state, "pending");
    c.resolve(&r.outcome, &card, "REST", "k", None).unwrap();
    r.owner();
    let s = c.command_status(&r.outcome, "k").unwrap();
    assert_eq!(s.state, CommandState::Stale, "{s:?}");
    let d = &r.w().list_decisions(r.w().id()).unwrap()[0];
    assert!(r.w().decision_approvals(&d.id).unwrap().is_empty());
    assert_eq!(r.card(&mut c).state, "invalidated");
}

// proto-resolve-vs-expiry
#[test]
fn proto_resolve_vs_expiry_an_expired_decision_is_never_approved() {
    let mut r = rig();
    r.decide("d", 1, |_| {});
    let mut c = r.client(Surface::Cli);
    let card = r.card(&mut c);
    c.resolve(&r.outcome, &card, "REST", "k", None).unwrap();
    // the owner's clock is past the expiry when it gets to the command
    r.set_clock(later);
    let done = apply_queued(r.wm(), later()).unwrap();
    assert_eq!(done.len(), 1);
    assert_eq!(
        c.command_status(&r.outcome, "k").unwrap().state,
        CommandState::Stale
    );
    let d = &r.w().list_decisions(r.w().id()).unwrap()[0];
    assert!(r.w().decision_approvals(&d.id).unwrap().is_empty());
    assert_eq!(r.card(&mut c).state, "expired");
    assert_eq!(
        r.w().get_work_item(&r.item).unwrap().state,
        WorkItemState::WaitingDecision,
        "HOLD, never an approval"
    );
}

// proto-command-idempotent
#[test]
fn proto_command_idempotent_same_key_same_stored_result_one_effect() {
    let mut r = rig();
    r.decide("d", 2, |_| {});
    let mut c = r.client(Surface::Cli);
    let card = r.card(&mut c);
    let first = c
        .resolve(&r.outcome, &card, "REST", "k", Some("ok"))
        .unwrap();
    let again = c
        .resolve(&r.outcome, &card, "REST", "k", Some("ok"))
        .unwrap();
    assert_eq!(first, again);
    assert_eq!(r.owner().len(), 1);
    // a retry after it was applied returns the stored result and does not answer twice
    let retry = c
        .resolve(&r.outcome, &card, "REST", "k", Some("ok"))
        .unwrap();
    assert_eq!(retry.state, CommandState::Applied);
    assert!(r.owner().is_empty());
    let d = &r.w().list_decisions(r.w().id()).unwrap()[0];
    assert_eq!(r.w().decision_approvals(&d.id).unwrap().len(), 1);
    // the same key for another request is refused, not merged
    let err = c
        .resolve(&r.outcome, &card, "RPC", "k", Some("ok"))
        .unwrap_err();
    assert!(err.to_string().contains("different request"), "{err}");
    assert_eq!(
        c.command_status(&r.outcome, "k").unwrap().state,
        CommandState::Applied
    );
}

#[test]
fn the_client_sends_only_what_the_person_was_shown() {
    struct Rec(Rc<RefCell<Vec<String>>>);
    impl Transport for Rec {
        fn call(&mut self, call: Call) -> Result<Value, ClientError> {
            self.0
                .borrow_mut()
                .push(serde_json::to_string(&call).unwrap());
            Err(ClientError::Refused("recorded".into()))
        }
    }
    let mut r = rig();
    r.decide("d", 2, |_| {});
    let mut real = r.client(Surface::Cli);
    let card = r.card(&mut real);
    let log = Rc::new(RefCell::new(vec![]));
    let mut c = OutcomeClient::new(Rec(log.clone()), Surface::Tui);
    // no default and no invented choice: nothing is sent
    for bad in ["", "grant_once", "REST ", "recommended"] {
        assert!(
            c.resolve(&r.outcome, &card, bad, "k", None).is_err(),
            "{bad:?}"
        );
    }
    let mut old = card.clone();
    old.state = "expired".into();
    assert!(c.resolve(&r.outcome, &old, "REST", "k", None).is_err());
    assert!(c.command("explode", &r.outcome, None, "k").is_err());
    assert_eq!(log.borrow().len(), 0);
    // an offered choice goes out with the revision and the hash of the card, and the surface
    let _ = c.resolve(&r.outcome, &card, "reject", "k2", None);
    let line = log.borrow()[0].clone();
    assert!(line.contains(&format!("\"expected_revision\":{}", card.revision)));
    assert!(
        line.contains(&card.brief_sha256) && line.contains("\"surface\":\"tui\""),
        "{line}"
    );
}

// proto-unsolicited-decision-resolve (T-24): the engine cannot answer a decision or fake the answer to one
fn hello() -> String {
    format!("{{\"msg\":\"hello\",\"version\":\"{VERSION}\",\"protocol\":{PROTOCOL}}}\n")
}

#[derive(Clone, Default)]
struct Sink(Rc<RefCell<Vec<u8>>>);
impl Write for Sink {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        self.0.borrow_mut().extend_from_slice(b);
        Ok(b.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn stdio_into(engine_says: &str, out: Sink) -> Stdio<Cursor<Vec<u8>>, Sink> {
    Stdio::connect(
        Cursor::new(format!("{}{engine_says}", hello()).into_bytes()),
        out,
    )
    .unwrap()
}

fn stdio(engine_says: &str) -> Stdio<Cursor<Vec<u8>>, Sink> {
    stdio_into(engine_says, Sink::default())
}

fn resolved_call() -> Call {
    serde_json::from_value(json!({"method": "decision.resolve", "params": {
        "outcome": "o", "id": "d", "expected_revision": 2, "choice": "REST",
        "brief_sha256": "a".repeat(64), "idempotency_key": "k", "surface": "cli"}}))
    .unwrap()
}

#[test]
fn proto_unsolicited_decision_resolve_a_result_nobody_asked_for_kills_the_link() {
    // an engine that answers before it was asked, then tries to answer the real request too
    let mut t = stdio(
        "{\"msg\":\"result\",\"id\":77,\"ok\":{\"state\":\"applied\",\"kind\":\"decision.resolve\"}}\n\
         {\"msg\":\"result\",\"id\":1,\"ok\":{}}\n",
    );
    let e = t.call(resolved_call()).unwrap_err();
    assert!(matches!(e, ClientError::Hostile(_)), "{e:?}");
    // poisoned: the true answer that follows is not believed either, and no later call gets through
    assert!(matches!(
        t.call(Call::OrchestratorStatus),
        Err(ClientError::Hostile(_))
    ));
}

#[test]
fn a_hostile_engine_is_refused_for_every_message_that_is_not_the_result() {
    for line in [
        r#"{"msg":"restarted"}"#,
        r#"{"msg":"task_done","error":null}"#,
        r#"{"msg":"withdraw","id":"a"}"#,
        r#"{"msg":"result","id":1}"#,
        r#"{"msg":"result","id":1,"ok":{},"error":"both"}"#,
    ] {
        let mut t = stdio(&format!("{line}\n"));
        assert!(
            matches!(
                t.call(Call::OrchestratorStatus),
                Err(ClientError::Hostile(_))
            ),
            "{line}"
        );
    }
    // events and notices between are skipped; the result is returned; an error is a refusal
    let ev = r#"{"v":1,"seq":1,"ts":"2026-01-01T00:00:00Z","session_id":"s","task_id":null,"turn_id":null,"agent_id":null,"type":"notice","payload":{"level":"info","text":"hi"}}"#;
    let out = Sink::default();
    let mut t = stdio_into(
        &format!(
            "{ev}\n{{\"msg\":\"notice\",\"text\":\"n\"}}\n{{\"msg\":\"result\",\"id\":1,\"ok\":{{\"enabled\":false}}}}\n\
         {{\"msg\":\"result\",\"id\":2,\"error\":\"outcomes are off\"}}\n"
        ),
        out.clone(),
    );
    assert_eq!(t.call(Call::OrchestratorStatus).unwrap()["enabled"], false);
    assert!(
        matches!(t.call(Call::OrchestratorStatus), Err(ClientError::Refused(e)) if e.contains("off"))
    );
    // the bytes this client wrote are the requests, one per line
    let sent = String::from_utf8(out.0.borrow().clone()).unwrap();
    assert_eq!(sent.lines().count(), 2);
}

#[test]
fn a_reply_that_is_about_another_command_or_lies_about_order_is_not_believed() {
    let ok = |kind: &str, key: &str| {
        format!(
            "{{\"msg\":\"result\",\"id\":1,\"ok\":{{\"idempotency_key\":\"{key}\",\"kind\":\"{kind}\",\"state\":\"applied\"}}}}\n"
        )
    };
    let mut r = rig();
    r.decide("d", 2, |_| {});
    let card = {
        let mut c = r.client(Surface::Cli);
        r.card(&mut c)
    };
    // the engine says "applied" for a different key, or for a different kind
    for engine in [ok("decision.resolve", "other"), ok("outcome.cancel", "k")] {
        let mut c = OutcomeClient::new(stdio(&engine), Surface::Cli);
        let e = c.resolve(&r.outcome, &card, "REST", "k", None).unwrap_err();
        assert!(matches!(e, ClientError::Hostile(_)), "{engine} {e:?}");
    }
    // events that repeat or go back
    let page = |seqs: &[u64], next: u64| {
        let evs: Vec<Value> = seqs.iter().map(|s| json!({"seq": s})).collect();
        format!(
            "{{\"msg\":\"result\",\"id\":1,\"ok\":{}}}\n",
            json!({"events": evs, "next_event_id": next})
        )
    };
    for (seqs, next) in [
        (vec![3u64, 3], 3u64),
        (vec![2, 1], 2),
        (vec![5], 9),
        (vec![4], 4),
    ] {
        let mut c = OutcomeClient::new(stdio(&page(&seqs, next)), Surface::Cli);
        // `after` is 4 for the last case: seq 4 is not after 4
        let after = if seqs == vec![4] { 4 } else { 0 };
        assert!(c.events("o", after).is_err(), "{seqs:?} {next}");
    }
}

// version skew (T-35): the migration cases themselves are in the session tests
// (`a_default_open_stays_at_v4_...`, `a_binary_that_supports_v4_refuses_v5_...`)
#[test]
fn an_engine_of_another_protocol_is_refused_at_connect() {
    for p in [3, 5] {
        let hello = format!("{{\"msg\":\"hello\",\"version\":\"{VERSION}\",\"protocol\":{p}}}\n");
        let e = Stdio::connect(Cursor::new(hello.into_bytes()), Sink::default())
            .err()
            .unwrap();
        assert!(e.to_string().contains(&format!("protocol {p}")), "{e}");
    }
    assert!(
        Stdio::connect(
            Cursor::new(b"{\"msg\":\"restarted\"}\n".to_vec()),
            Sink::default()
        )
        .is_err()
    );
}

#[test]
fn the_contract_is_approved_by_a_command_the_owner_applies() {
    let tmp = tempfile::tempdir().unwrap();
    let base = synthesis_core::canonicalize(tmp.path()).unwrap();
    let repo = base.join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    let store = Store::open(
        &base.join("home"),
        StoreOptions {
            schema_target: SCHEMA_VERSION_OUTCOMES,
            ..StoreOptions::default()
        },
    )
    .unwrap();
    let ws = WorkspaceContext::open("w", &repo, &repo, &[], base.join("cart"), "pg").unwrap();
    let mut w = store
        .create_outcome(NewOutcome {
            workspace: &ws,
            base_commit: "c0ffee".into(),
            contract: &contract(),
            policy: &OutcomePolicy::new(true, 16, 10, 24),
        })
        .unwrap();
    let rev = w.get_outcome(w.id()).unwrap().revision;
    w.transition_outcome(
        rev,
        OutcomeState::ContractPending,
        None,
        &Effects::default(),
    )
    .unwrap();
    let id = w.id().to_string();
    let mut c = OutcomeClient::new(InProc(Service::new(&base.join("home"), true)), Surface::Cli);
    let hash = w.get_contract(w.id(), 1).unwrap().1;
    // a wrong hash is denied, the right one applied; a surface cannot approve by any other route
    c.approve_contract(&id, 1, &"0".repeat(64), "bad").unwrap();
    c.approve_contract(&id, 1, &hash, "good").unwrap();
    let done = apply_queued(&mut w, OffsetDateTime::now_utc()).unwrap();
    assert_eq!(done.len(), 2);
    assert_eq!(
        c.command_status(&id, "bad").unwrap().state,
        CommandState::Denied
    );
    assert_eq!(
        c.command_status(&id, "good").unwrap().state,
        CommandState::Applied
    );
    assert!(
        w.get_contract(w.id(), 1).unwrap().2,
        "the approval verifies"
    );
    // start, pause, resume, cancel and ack wait for the orchestrator process
    let s = c.command("start", &id, None, "s").unwrap();
    assert_eq!(s.state, CommandState::Queued);
    assert!(
        apply_queued(&mut w, OffsetDateTime::now_utc())
            .unwrap()
            .is_empty()
    );
}
