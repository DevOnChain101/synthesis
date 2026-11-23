//! The person's line to a worker, end to end without a model (S1 4.6, P2c): a surface queues
//! `attempt.message` through the client, the owner applies it, the worker's `deliver_inbox` hands it
//! to the harness and acks it in its own session, and what the worker never took goes back to the
//! next attempt. The harness is a recording closure here; its own tests cover the `owner_message` turn.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};
use synthesis_app::outcome_client::{InProc, OutcomeClient};
use synthesis_app::outcome_service::Service;
use synthesis_app::proto::FEATURES;
use synthesis_app::sessions::{AckFn, SendFn, deliver_inbox, start_cursor};
use synthesis_core::WorkspaceContext;
use synthesis_core::outcome::*;
use synthesis_orchestrator::apply_queued;
use synthesis_session::outcome::*;
use synthesis_session::{NewSession, SCHEMA_VERSION_OUTCOMES, SessionWriter, Store, StoreOptions};
use time::OffsetDateTime;

const ATT: &str = "att-1";

struct Rig {
    _tmp: tempfile::TempDir,
    home: std::path::PathBuf,
    store: Store,
    w: OutcomeWriter,
    outcome: String,
    item: WorkItemId,
    attempt: AttemptId,
}

fn contract() -> Contract {
    let mut caps = BTreeMap::new();
    caps.insert(Capability::RepoRead, CapabilityMode::Allow);
    Contract {
        kind: synthesis_core::outcome::ContractKind::Write,
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
    let items = w
        .insert_plan(&Plan {
            version: 1,
            contract_version: 1,
            items: vec![
                PlanItem {
                    key: "one".into(),
                    kind: WorkItemKind::Agent,
                    requires: vec![],
                    capabilities: vec![],
                    covers: vec![],
                    title: String::new(),
                    instructions: String::new(),
                },
                PlanItem {
                    key: "test".into(),
                    kind: WorkItemKind::Verify,
                    requires: vec!["one".into()],
                    capabilities: vec![],
                    covers: vec!["c1".into()],
                    title: String::new(),
                    instructions: String::new(),
                },
            ],
        })
        .unwrap();
    go(&mut w, OutcomeState::Ready);
    go(&mut w, OutcomeState::Running);
    let item = items.iter().find(|(k, _)| k == "one").unwrap().1.clone();
    let to = |w: &mut OutcomeWriter, s| {
        let r = w.get_work_item(&item).unwrap().revision;
        w.transition_work_item(&item, r, s, &none).unwrap();
    };
    to(&mut w, WorkItemState::Ready);
    to(&mut w, WorkItemState::Leased);
    drop(
        store
            .create_session(NewSession {
                id: Some(ATT.into()),
                workspace: &ws,
                model_config_id: "m".into(),
                permission_mode: synthesis_core::PermissionMode::Default,
                permission_policy_id: "default".into(),
                budget: Default::default(),
                parent: Some(synthesis_core::ParentRef {
                    session_id: w.control_session_id().into(),
                    tool_call_id: String::new(),
                }),
                title: None,
            })
            .unwrap(),
    );
    let (a, _) = w.new_attempt(&item, 1, ATT, "local-inproc", &none).unwrap();
    to(&mut w, WorkItemState::Running);
    w.transition_attempt(
        &a.id,
        a.revision,
        AttemptState::Running,
        &AttemptEnd::default(),
        &none,
    )
    .unwrap();
    Rig {
        _tmp: tmp,
        home,
        store,
        outcome: w.id().to_string(),
        w,
        item,
        attempt: a.id,
    }
}

impl Rig {
    fn client(&self) -> OutcomeClient<InProc> {
        OutcomeClient::new(InProc(Service::new(&self.home, true)), Surface::Tui)
    }

    /// The surface asks, the owner ticks, the surface reads the settled command.
    fn say(
        &mut self,
        key: &str,
        text: &str,
        brief_change: bool,
    ) -> synthesis_view::outcomes::CommandReply {
        let mut c = self.client();
        let queued = c
            .send_worker_message(
                &self.outcome,
                self.item.as_str(),
                text,
                brief_change,
                None,
                key,
            )
            .unwrap();
        assert_eq!(queued.state, CommandState::Queued);
        apply_queued(&mut self.w, OffsetDateTime::now_utc()).unwrap();
        c.command_status(&self.outcome, key).unwrap()
    }

    fn worker_session(&self) -> Arc<Mutex<SessionWriter>> {
        Arc::new(Mutex::new(
            self.store.open_attempt_session(&self.w, ATT).unwrap(),
        ))
    }
}

/// A harness that records what it was handed; `fail` makes it refuse.
fn harness(log: &Arc<Mutex<Vec<(u64, String)>>>, fail: Option<&'static str>) -> SendFn {
    let log = log.clone();
    Arc::new(move |seq, text| {
        let log = log.clone();
        Box::pin(async move {
            if let Some(e) = fail {
                return Err(e.to_string());
            }
            log.lock().unwrap().push((seq, text));
            Ok(())
        })
    })
}

fn ack_into(ws: &Arc<Mutex<SessionWriter>>) -> AckFn {
    let ws = ws.clone();
    Arc::new(move |seq, sha| {
        ws.lock()
            .unwrap()
            .append_owner_delivered(seq, sha)
            .map_err(|e| e.to_string())
    })
}

#[tokio::test]
async fn live_path_from_the_client_to_the_harness_and_back_as_an_ack_exactly_once() {
    let mut r = rig();
    let reply = r.say("k1", "use the REST shape", false);
    assert_eq!(reply.state, CommandState::Applied, "{reply:?}");
    assert_eq!(reply.result["delivery"], "live");
    let seq = reply.result["seq"].as_u64().unwrap();
    let (log, ws) = (Arc::new(Mutex::new(vec![])), r.worker_session());
    let (send, ack) = (harness(&log, None), ack_into(&ws));
    let c = deliver_inbox(&r.home, ATT, 0, &send, &ack).await.unwrap();
    assert_eq!(c, seq);
    assert_eq!(
        *log.lock().unwrap(),
        vec![(seq, "use the REST shape".to_string())]
    );
    // the next poll sees nothing above the cursor, and a restarted watcher (a cursor made of the
    // acks) delivers nothing either: kill -9 after the ack does not deliver twice
    deliver_inbox(&r.home, ATT, c, &send, &ack).await.unwrap();
    let restarted = r.store.reader().unwrap().owner_deliveries(ATT).unwrap();
    assert_eq!(restarted, vec![seq]);
    deliver_inbox(&r.home, ATT, restarted[0], &send, &ack)
        .await
        .unwrap();
    assert_eq!(log.lock().unwrap().len(), 1);
    // the attempt's listing says the worker took it (the surface's "delivered")
    let a = r.client().attempt(&r.outcome, r.attempt.as_str()).unwrap();
    assert_eq!(a["attempt"]["messages_delivered"], json!([seq]));
    // nothing is left to requeue once the attempt ends
    let a = r.w.list_attempts(&r.item).unwrap().remove(0);
    r.w.transition_attempt(
        &a.id,
        a.revision,
        AttemptState::Lost,
        &AttemptEnd::default(),
        &Effects::default(),
    )
    .unwrap();
    assert_eq!(r.w.requeue_unacked(&r.item).unwrap(), 0);
}

#[tokio::test]
async fn killed_before_the_ack_the_message_is_taken_once_by_the_next_attempt() {
    let mut r = rig();
    r.say("k1", "mind the cache", false);
    // the harness took it, then the worker died before it wrote the ack (the ack never ran)
    let log = Arc::new(Mutex::new(vec![]));
    let broken: AckFn = Arc::new(|_, _| Err("killed".into()));
    deliver_inbox(&r.home, ATT, 0, &harness(&log, None), &broken)
        .await
        .unwrap();
    assert_eq!(log.lock().unwrap().len(), 1);
    // the owner finds the attempt lost and the message unacked: queued again, then taken by attempt 2
    let a = r.w.list_attempts(&r.item).unwrap().remove(0);
    r.w.transition_attempt(
        &a.id,
        a.revision,
        AttemptState::Lost,
        &AttemptEnd::default(),
        &Effects::default(),
    )
    .unwrap();
    assert_eq!(r.w.requeue_unacked(&r.item).unwrap(), 1);
    let q = r.w.queued_messages(&r.item).unwrap();
    assert_eq!(q.len(), 1);
    assert_eq!(q[0].text, "mind the cache");
    r.w.mark_messages_consumed(&r.item, &r.attempt, &[q[0].seq])
        .unwrap();
    assert!(r.w.queued_messages(&r.item).unwrap().is_empty());
    assert_eq!(r.w.requeue_unacked(&r.item).unwrap(), 0);
}

#[tokio::test]
async fn a_task_that_has_not_started_is_retried_and_a_refusal_is_left_unacked() {
    let mut r = rig();
    r.say("k1", "first", false);
    let (log, ws) = (Arc::new(Mutex::new(vec![])), r.worker_session());
    let ack = ack_into(&ws);
    // no task yet: the cursor stays and nothing is acked
    let c = deliver_inbox(
        &r.home,
        ATT,
        0,
        &harness(&log, Some(synthesis_harness::NO_TASK_RUNNING)),
        &ack,
    )
    .await
    .unwrap();
    assert_eq!(c, 0);
    assert!(
        r.store
            .reader()
            .unwrap()
            .owner_deliveries(ATT)
            .unwrap()
            .is_empty()
    );
    // a hook refused it: said, not acked, and not retried for ever
    let c = deliver_inbox(
        &r.home,
        ATT,
        0,
        &harness(&log, Some("blocked by a UserPromptSubmit hook")),
        &ack,
    )
    .await
    .unwrap();
    assert!(c > 0);
    assert!(
        r.store
            .reader()
            .unwrap()
            .owner_deliveries(ATT)
            .unwrap()
            .is_empty()
    );
    assert!(log.lock().unwrap().is_empty());
    // the worker ends without it: the owner queues it again for the next attempt
    let a = r.w.list_attempts(&r.item).unwrap().remove(0);
    r.w.transition_attempt(
        &a.id,
        a.revision,
        AttemptState::Finished,
        &AttemptEnd::default(),
        &Effects::default(),
    )
    .unwrap();
    assert_eq!(r.w.requeue_unacked(&r.item).unwrap(), 1);
}

#[tokio::test]
async fn an_older_worker_never_reads_the_inbox_and_the_message_comes_back_queued() {
    let mut r = rig();
    let reply = r.say("k1", "hello old worker", false);
    assert_eq!(reply.result["delivery"], "live");
    // no deliver_inbox at all: the binary has no watcher
    let a = r.w.list_attempts(&r.item).unwrap().remove(0);
    r.w.transition_attempt(
        &a.id,
        a.revision,
        AttemptState::Finished,
        &AttemptEnd::default(),
        &Effects::default(),
    )
    .unwrap();
    r.w.requeue_unacked(&r.item).unwrap();
    let events = r.client().events(&r.outcome, 0).unwrap().0;
    let rows = synthesis_view::outcomes::MessageRow::from_events(&events, &[]);
    assert_eq!(rows.len(), 1);
    assert_eq!(
        rows[0].label(),
        "not delivered, queued for the next attempt"
    );
}

#[test]
fn denied_and_refused_paths_reach_the_surface_in_words() {
    let mut r = rig();
    // the client refuses hidden text before sending anything
    let mut c = r.client();
    assert!(
        c.send_worker_message(
            &r.outcome,
            r.item.as_str(),
            "a\u{202e}b",
            false,
            None,
            "bad"
        )
        .is_err()
    );
    // a stale outcome revision is `stale`, not sent
    let q = c
        .send_worker_message(&r.outcome, r.item.as_str(), "hi", false, Some(1), "stale")
        .unwrap();
    assert_eq!(q.state, CommandState::Queued);
    apply_queued(&mut r.w, OffsetDateTime::now_utc()).unwrap();
    assert_eq!(
        c.command_status(&r.outcome, "stale").unwrap().state,
        CommandState::Stale
    );
}

#[test]
fn features_say_attempt_message_and_old_servers_do_not() {
    assert!(FEATURES.contains(&"attempt_message"));
    let r = rig();
    let st = r.client().status().unwrap();
    let has = |v: &Value| {
        v["features"]
            .as_array()
            .is_some_and(|a| a.iter().any(|f| f == "attempt_message"))
    };
    assert_eq!(has(&st), !cfg!(windows));
    // an older engine's status has no such feature: a surface shows no message control
    assert!(!has(
        &json!({"features": ["read_only_goal", "report_review"]})
    ));
    assert!(!has(&json!({"enabled": true})));
}

#[test]
fn only_the_orchestrator_endpoint_queues_and_the_stdio_engine_is_read_only() {
    let r = rig();
    let ro = Service::read_only(&r.home, true);
    let mut c = OutcomeClient::new(InProc(ro), Surface::Cli);
    let e = c
        .send_worker_message(&r.outcome, r.item.as_str(), "hi", false, None, "k")
        .unwrap_err();
    assert!(e.to_string().contains("read-only"), "{e}");
}

#[tokio::test]
async fn the_start_cursor_is_the_newest_ack_and_an_unreadable_store_is_an_error_never_zero() {
    let mut r = rig();
    r.say("k1", "one", false);
    let (log, ws) = (Arc::new(Mutex::new(vec![])), r.worker_session());
    let c = deliver_inbox(&r.home, ATT, 0, &harness(&log, None), &ack_into(&ws))
        .await
        .unwrap();
    assert_eq!(start_cursor(&r.home, ATT).unwrap(), c);
    assert!(start_cursor(&r.home.join("no-such-home"), ATT).is_err());
}

#[test]
fn msg_hidden_chars_union_of_both_lists() {
    for c in [
        '\u{fffc}',
        '\u{2800}',
        '\u{1d173}',
        '\u{1d17a}',
        '\u{a0}',
        '\u{202e}',
    ] {
        assert!(check_message_text(&format!("a{c}b")).is_err(), "{c:?}");
    }
    assert!(check_message_text("plain\ttext\nok").is_ok());
}
