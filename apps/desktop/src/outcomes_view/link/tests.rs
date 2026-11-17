use std::cell::RefCell;
use std::rc::Rc;
use std::sync::mpsc::channel;

use futures::channel::mpsc::unbounded;
use serde_json::{Value, json};
use synthesis_app::outcome_client::{ClientError, OutcomeClient, Transport};
use synthesis_app::proto::Call;
use synthesis_session::outcome::Surface;

use super::*;
use crate::outcomes_view::sample;

/// A scripted orchestrator: the sample's replies, a log of the methods called, and a decision that can move.
#[derive(Clone, Default)]
struct Fake {
    log: Rc<RefCell<Vec<String>>>,
    /// The revision `decision.list` reports for `d_7f3a`.
    revision: Rc<RefCell<i64>>,
    resolved: Rc<RefCell<Vec<Value>>>,
    hostile: bool,
    /// Hints still to give, and the request sender dropped when they are used up (ends the loop).
    hints: Rc<RefCell<u32>>,
    end: Rc<RefCell<Option<std::sync::mpsc::Sender<Req>>>>,
}

impl Transport for Fake {
    fn next_hint(
        &mut self,
        _: Duration,
    ) -> Result<Option<synthesis_app::proto::DecisionHint>, ClientError> {
        let mut n = self.hints.borrow_mut();
        if *n == 0 {
            self.end.borrow_mut().take();
            return Ok(None);
        }
        *n -= 1;
        Ok(Some(synthesis_app::proto::DecisionHint {
            outcome: "o_auth".into(),
            id: "d_7f3a".into(),
            revision: 5,
            state: "pending".into(),
        }))
    }

    fn call(&mut self, call: Call) -> Result<Value, ClientError> {
        let v = serde_json::to_value(&call).unwrap();
        let m = v["method"].as_str().unwrap().to_string();
        let p = v["params"].clone();
        self.log.borrow_mut().push(m.clone());
        Ok(match m.as_str() {
            "outcome.list" => json!({"outcomes": sample::rows()}),
            "outcome.budget" => sample::budget(p["id"].as_str().unwrap()),
            "outcome.contract.get" => sample::contract(),
            "decision.list" => {
                let mut cards: Vec<Value> = sample::cards_json();
                cards[0]["revision"] = json!(*self.revision.borrow());
                if self.hostile {
                    cards[0]["brief"]["alternatives"][0]["recommended"] = json!(true);
                }
                json!({"decisions": cards})
            }
            "events.subscribe" => {
                let evs = sample::events();
                json!({"events": evs, "next_event_id": evs.len()})
            }
            "plan.get" => sample::plan(),
            "work_item.get" => {
                let id = p["id"].as_str().unwrap();
                sample::work_items()
                    .into_iter()
                    .find(|w| w["id"] == id)
                    .unwrap()
            }
            "evidence.get" => json!({"evidence": sample::evidence()}),
            "attempt.diff" => sample::diff(),
            "decision.resolve" => {
                self.resolved.borrow_mut().push(p.clone());
                json!({"idempotency_key": p["idempotency_key"], "kind": "decision.resolve", "state": "applied"})
            }
            other => return Err(ClientError::Refused(format!("no {other}"))),
        })
    }
}

fn client(f: &Fake) -> OutcomeClient<Fake> {
    OutcomeClient::new(f.clone(), Surface::Superagent)
}

fn fake() -> Fake {
    let f = Fake::default();
    *f.revision.borrow_mut() = 4;
    f
}

/// Runs `reqs` through the loop and returns what it said (the loop ends when the requests do).
fn run(f: &Fake, reqs: Vec<Req>) -> Vec<Msg> {
    let (tx, rx) = channel();
    for r in reqs {
        tx.send(r).unwrap();
    }
    drop(tx);
    let (out, mut orx) = unbounded();
    let r = serve(&mut client(f), &rx, &out, Duration::ZERO);
    assert_eq!(r, Ok(()));
    drop(out);
    std::iter::from_fn(|| orx.try_recv().ok()).collect()
}

type Snap<'a> = (
    &'a Vec<Item>,
    &'a Option<Box<Detail>>,
    &'a Option<InspectorContent>,
);

fn snapshots(m: &[Msg]) -> Vec<Snap<'_>> {
    m.iter()
        .filter_map(|m| match m {
            Msg::Snapshot {
                items,
                detail,
                inspector,
                ..
            } => Some((items, detail, inspector)),
            _ => None,
        })
        .collect()
}

#[test]
fn the_first_snapshot_has_the_list_the_first_outcome_and_the_overview() {
    let f = fake();
    let msgs = run(&f, vec![]);
    let s = snapshots(&msgs);
    assert_eq!(s.len(), 1);
    let (items, detail, inspector) = &s[0];
    assert_eq!(items.len(), 4);
    assert_eq!(items[0].budget.cost_usd.limit, Some(5.0));
    let d = detail.as_ref().unwrap();
    assert_eq!(
        (d.outcome.as_str(), d.cards.as_ref().unwrap().len()),
        ("o_auth", 3)
    );
    assert!(d.contract.is_ok() && matches!(d.history, InspectorContent::Activity(_)));
    assert!(matches!(inspector, Some(InspectorContent::Overview(_))));
    // only reads were made: the answer is never sent by a refresh
    assert!(!f.log.borrow().iter().any(|m| m.contains("resolve")));
}

#[test]
fn every_inspector_tab_is_built_from_the_replies_and_diff_reads_the_newest_attempt() {
    let f = fake();
    let tabs = InspectorTab::ALL.map(Req::Inspect).into_iter().collect();
    let msgs = run(&f, tabs);
    let got: Vec<_> = snapshots(&msgs)
        .iter()
        .skip(1)
        .map(|s| s.2.clone().unwrap())
        .collect();
    let kinds: Vec<_> = got.iter().map(InspectorContent::tab).collect();
    assert_eq!(kinds, InspectorTab::ALL);
    assert!(
        got.iter()
            .all(|c| !matches!(c, InspectorContent::Unavailable { .. })),
        "{got:?}"
    );
    let InspectorContent::Diff(d) = &got[3] else {
        panic!()
    };
    assert_eq!(d.files.len(), 3);
    // `attempt.diff` was asked for an attempt with a candidate commit (at3 is the newest that has one)
    assert!(f.log.borrow().contains(&"attempt.diff".to_string()));
}

#[test]
fn an_answer_is_sent_with_the_cards_revision_and_hash_after_a_fresh_look() {
    let f = fake();
    let card = sample::cards().remove(0);
    let req = Req::Resolve {
        outcome: "o_auth".into(),
        card: Box::new(card.clone()),
        choice: "approve_with_w5".into(),
    };
    let msgs = run(&f, vec![req]);
    let sent = f.resolved.borrow();
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0]["expected_revision"], 4);
    assert_eq!(sent[0]["brief_sha256"], card.brief_sha256.as_str());
    assert_eq!(
        (sent[0]["choice"].as_str(), sent[0]["surface"].as_str()),
        (Some("approve_with_w5"), Some("superagent"))
    );
    assert!(
        sent[0]["idempotency_key"]
            .as_str()
            .is_some_and(|k| k.len() >= 32)
    );
    assert!(
        msgs.iter()
            .any(|m| matches!(m, Msg::Answered(n) if n.contains("done"))),
        "the server's word"
    );
    // and a snapshot follows, so the person sees the server's state, not the click
    assert!(matches!(msgs.last(), Some(Msg::Snapshot { .. })));
}

#[test]
fn a_card_that_moved_is_not_answered_and_a_choice_that_is_not_offered_is_not_sent() {
    let f = fake();
    *f.revision.borrow_mut() = 5; // the server has a newer revision than the card the person saw
    let card = sample::cards().remove(0);
    let req = Req::Resolve {
        outcome: "o_auth".into(),
        card: Box::new(card.clone()),
        choice: "approve_with_w5".into(),
    };
    let msgs = run(&f, vec![req]);
    assert!(f.resolved.borrow().is_empty());
    assert!(
        msgs.iter()
            .any(|m| matches!(m, Msg::Answered(n) if n.contains("changed since you saw it")))
    );
    // a choice the card does not offer never leaves the client
    let f = fake();
    let req = Req::Resolve {
        outcome: "o_auth".into(),
        card: Box::new(card),
        choice: "approve_everything".into(),
    };
    let msgs = run(&f, vec![req]);
    assert!(f.resolved.borrow().is_empty());
    assert!(
        msgs.iter()
            .any(|m| matches!(m, Msg::Answered(n) if n.starts_with("Not sent")))
    );
}

#[test]
fn a_hostile_brief_is_the_decision_tabs_error_not_a_card() {
    let f = Fake {
        hostile: true,
        ..fake()
    };
    let msgs = run(&f, vec![]);
    let s = snapshots(&msgs);
    let d = s[0].1.as_ref().unwrap();
    assert!(
        d.cards.as_ref().unwrap_err().contains("recommended"),
        "{:?}",
        d.cards
    );
    assert!(d.contract.is_ok(), "the other parts are still shown");
}

#[test]
fn a_failed_link_ends_the_loop_with_its_text() {
    struct Dead;
    impl Transport for Dead {
        fn call(&mut self, _: Call) -> Result<Value, ClientError> {
            Err(ClientError::Io("the orchestrator closed the link".into()))
        }
    }
    let (_tx, rx) = channel();
    let (out, _orx) = unbounded();
    let r = serve(
        &mut OutcomeClient::new(Dead, Surface::Superagent),
        &rx,
        &out,
        Duration::ZERO,
    );
    assert_eq!(r, Err("the orchestrator closed the link".into()));
}

#[test]
fn a_flood_of_hints_is_one_refetch_not_one_per_hint() {
    let f = fake();
    *f.hints.borrow_mut() = 500;
    let (tx, rx) = channel::<Req>();
    *f.end.borrow_mut() = Some(tx);
    let (out, _orx) = unbounded();
    serve(&mut client(&f), &rx, &out, Duration::ZERO).unwrap();
    let lists = f
        .log
        .borrow()
        .iter()
        .filter(|m| *m == "outcome.list")
        .count();
    assert!(lists <= 2, "{lists} refetches for 500 hints");
}

#[cfg(unix)]
#[test]
fn the_synt_next_to_the_app_is_found_by_the_real_path_not_by_a_symlink() {
    let dir = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(dir.path()).unwrap();
    let (real, user) = (root.join("real"), root.join("user"));
    std::fs::create_dir_all(&real).unwrap();
    std::fs::create_dir_all(&user).unwrap();
    std::fs::write(real.join("app"), "x").unwrap();
    std::os::unix::fs::symlink(real.join("app"), user.join("app")).unwrap();
    // a `synt` planted beside the symlink, in the folder the user can write, is not used
    std::fs::write(user.join("synt"), "evil").unwrap();
    assert_eq!(synt_beside(&user.join("app")), None);
    // beside the real app it is; a symlink named synt there is not
    std::fs::write(real.join("elsewhere"), "x").unwrap();
    std::os::unix::fs::symlink(real.join("elsewhere"), real.join("synt")).unwrap();
    assert_eq!(synt_beside(&user.join("app")), None);
    std::fs::remove_file(real.join("synt")).unwrap();
    std::fs::write(real.join("synt"), "ok").unwrap();
    assert_eq!(synt_beside(&user.join("app")), Some(real.join("synt")));
}
