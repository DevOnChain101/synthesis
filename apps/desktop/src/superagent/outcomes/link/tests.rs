use std::cell::RefCell;
use std::rc::Rc;
use std::sync::mpsc::channel;

use futures::channel::mpsc::unbounded;
use serde_json::{Value, json};
use synthesis_app::outcome_client::{ClientError, OutcomeClient, Transport};
use synthesis_app::proto::Call;
use synthesis_session::outcome::Surface;

use super::*;
use crate::superagent::outcomes::sample;

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
    /// The orchestrator's memory of `outcome.create` keys: `(key, params)`, one draft per key.
    drafts: Rc<RefCell<Vec<(String, Value)>>>,
    /// Creates whose reply is lost (the link fails after the draft was made).
    lose: Rc<RefCell<u32>>,
    /// What `outcome.contract.revise` answers: `applied` (default), `stale`, `denied` or `queued`.
    revise: Rc<RefCell<&'static str>>,
    /// What `outcome.contract.approve` answers (default `applied`).
    approve: Rc<RefCell<&'static str>>,
    /// Every revise and approve call, as sent.
    sent: Rc<RefCell<Vec<(String, Value)>>>,
    /// The contract of `outcome.contract.get` is not approved yet.
    draft: bool,
    /// `outcome.contract.get` of the current contract is an unapproved version 2 (a revision made elsewhere).
    draft_v2: bool,
    /// `outcome.contract.diff` is refused.
    diff_fails: bool,
    /// `outcome.contract.get` for version 2 reports another hash than the revise reply did.
    wrong_hash: bool,
    /// What `attempt.message` answers: `live`, `queued`, `denied` (a rate limit) or `pending` (never applied).
    msg: Rc<RefCell<&'static str>>,
    /// Every `attempt.message` call, as sent.
    msgs: Rc<RefCell<Vec<Value>>>,
    /// The control session's inbox events (`events.subscribe` adds them) and the acked `seq` of attempt `at4`.
    inbox: bool,
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
            "outcome.contract.get" => {
                let mut v = match p["version"].as_u64() {
                    Some(2) => sample::contract_v2(),
                    None if self.draft_v2 => sample::contract_v2(),
                    _ => sample::contract(),
                };
                if self.draft {
                    v["approved"] = json!(false);
                }
                if self.wrong_hash && p["version"] == 2 {
                    v["hash"] = json!("f".repeat(64));
                }
                v
            }
            "outcome.create" => {
                let key = p["idempotency_key"].as_str().unwrap().to_string();
                if !self.drafts.borrow().iter().any(|(k, _)| *k == key) {
                    self.drafts.borrow_mut().push((key, p.clone()));
                }
                if std::mem::take(&mut *self.lose.borrow_mut()) > 0 {
                    return Err(ClientError::Io("the orchestrator closed the link".into()));
                }
                let s = sample::contract();
                json!({"id": "o_new", "state": "contract_pending", "contract_version": 1, "contract_hash": s["hash"]})
            }
            "outcome.contract.revise" | "outcome.contract.approve" => {
                self.sent.borrow_mut().push((m.clone(), p.clone()));
                let (state, result) = if m.ends_with("revise") {
                    (
                        *self.revise.borrow(),
                        json!({"version": 2, "hash": sample::HASH_V2}),
                    )
                } else {
                    (*self.approve.borrow(), json!({}))
                };
                let state = if state.is_empty() { "applied" } else { state };
                json!({"idempotency_key": p["idempotency_key"], "kind": m, "state": state, "result": result})
            }
            "command.status" => {
                let (a, r) = (*self.approve.borrow(), *self.revise.borrow());
                let state = [a, r]
                    .into_iter()
                    .find(|s| !s.is_empty())
                    .unwrap_or("applied");
                let state = if *self.msg.borrow() == "pending" {
                    "queued"
                } else {
                    state
                };
                json!({"idempotency_key": p["idempotency_key"], "kind": "outcome.contract.approve", "state": state, "result": {}})
            }
            "outcome.contract.diff" if self.diff_fails => {
                return Err(ClientError::Refused("no such version".into()));
            }
            "outcome.contract.diff" => sample::diff_v2(),
            "decision.list" => {
                let mut cards: Vec<Value> = sample::cards_json();
                cards[0]["revision"] = json!(*self.revision.borrow());
                if self.hostile {
                    cards[0]["brief"]["alternatives"][0]["recommended"] = json!(true);
                }
                json!({"decisions": cards})
            }
            "attempt.message" => {
                self.msgs.borrow_mut().push(p.clone());
                let (state, result) = match *self.msg.borrow() {
                    "queued" => (
                        "applied",
                        json!({"delivery": "queued", "seq": 9, "rebriefed": 0}),
                    ),
                    "denied" => (
                        "denied",
                        json!({"reason": "at most 5 messages to one attempt in 10 minutes; wait, or the worker is flooded"}),
                    ),
                    "pending" => ("queued", json!({})),
                    _ => (
                        "applied",
                        json!({"delivery": "live", "attempt": "at4-1111-2222", "seq": 9, "rebriefed": 0}),
                    ),
                };
                json!({"idempotency_key": p["idempotency_key"], "kind": "attempt.message", "state": state, "result": result})
            }
            "events.subscribe" => {
                let mut evs = sample::events();
                if self.inbox {
                    evs.push(json!({"seq": 6, "type": "attempt.message", "payload": {"seq": 6, "attempt": "at4-1111-2222", "text": "use REST\u{1b}[31m", "sha256": "a"}}));
                    evs.push(json!({"seq": 7, "type": "attempt.message.queued", "payload": {"seq": 7, "work_item": "wi4", "text": "later", "sha256": "b"}}));
                }
                json!({"events": evs, "next_event_id": evs.len()})
            }
            "plan.get" => sample::plan(),
            "work_item.get" => {
                let id = p["id"].as_str().unwrap();
                let mut w = sample::work_items()
                    .into_iter()
                    .find(|w| w["id"] == id)
                    .unwrap();
                if self.inbox && id == "wi3" {
                    w["attempts"][0]["messages_delivered"] = json!([6]);
                }
                w
            }
            "approvals.queue" => json!({"rows": []}),
            "approvals.history" => {
                let first = p["outcome"] == "o_auth" && p["after_event_id"] == 0;
                json!({"records": if first { vec![json!({"event_id": 3, "ts": "2026-11-17T07:00:00Z",
                    "type": "decision.resolved", "payload": {"choice": "retry"}})] } else { vec![] },
                    "next_event_id": if p["outcome"] == "o_auth" { 3 } else { 0 }})
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
    let r = serve(&mut client(f), &rx, &out, Duration::ZERO, false, &ok_head);
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
        false,
        &ok_head,
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
    serve(&mut client(&f), &rx, &out, Duration::ZERO, false, &ok_head).unwrap();
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
    assert_eq!(found(&user.join("app")), None);
    // beside the real app it is; a symlink named synt there is not
    std::fs::write(real.join("elsewhere"), "x").unwrap();
    std::os::unix::fs::symlink(real.join("elsewhere"), real.join("synt")).unwrap();
    assert_eq!(found(&user.join("app")), None);
    std::fs::remove_file(real.join("synt")).unwrap();
    std::fs::write(real.join("synt"), "ok").unwrap();
    // not executable: refused; executable: used
    assert_eq!(found(&user.join("app")), None);
    use std::os::unix::fs::PermissionsExt as _;
    for (p, m) in [(real.join("synt"), 0o755), (real.clone(), 0o755)] {
        std::fs::set_permissions(p, std::fs::Permissions::from_mode(m)).unwrap();
    }
    assert_eq!(found(&user.join("app")), Some(real.join("synt")));
}

/// The usable `synt` beside `exe`, if any.
#[cfg(unix)]
fn found(exe: &std::path::Path) -> Option<std::path::PathBuf> {
    match synt_beside(exe) {
        Sibling::Usable(p) => Some(p),
        _ => None,
    }
}

fn ok_head(_: &str) -> Result<String, String> {
    Ok(BASE.into())
}

const BASE: &str = "9433c678e4b1d2f0a7c35e8b61d9f4a02c7b5e13";

fn contract() -> synthesis_core::outcome::Contract {
    serde_json::from_value(sample::contract()["contract"].clone()).unwrap()
}

fn create(key: &str) -> Req {
    Req::Submit(Submit::Create {
        workspace: "/work/app".into(),
        base_commit: BASE.into(),
        contract: Box::new(contract()),
        key: key.into(),
    })
}

fn revise(key: &str) -> Req {
    Req::Submit(Submit::Revise {
        outcome: "o_auth".into(),
        revision: 12,
        contract: Box::new(contract()),
        key: key.into(),
    })
}

fn forms(m: &[Msg]) -> Vec<&FormMsg> {
    m.iter()
        .filter_map(|m| match m {
            Msg::Form(f) => Some(f),
            _ => None,
        })
        .collect()
}

#[test]
fn a_created_outcome_is_stored_for_review_and_nothing_is_approved() {
    let f = fake();
    let msgs = run(&f, vec![create("create-k1")]);
    let drafts = f.drafts.borrow();
    assert_eq!(drafts.len(), 1);
    let (key, p) = &drafts[0];
    assert_eq!(key, "create-k1");
    assert_eq!(
        (
            p["workspace"].as_str(),
            p["base_commit"].as_str(),
            p["surface"].as_str()
        ),
        (Some("/work/app"), Some(BASE), Some("superagent"))
    );
    let fm = forms(&msgs);
    let [FormMsg::Review(r)] = fm.as_slice() else {
        panic!("{:?}", forms(&msgs).len())
    };
    // the first version has nothing to compare with: no diff call, and the server's hash is the one to approve
    assert_eq!(
        (r.outcome.as_str(), r.version, r.diff.is_none()),
        ("o_new", 1, true)
    );
    assert_eq!(r.hash, sample::review(false).hash);
    assert!(r.contract.is_ok() && r.approvable());
    let log = f.log.borrow();
    assert!(
        !log.iter()
            .any(|m| m.contains("approve") || m.contains("diff")),
        "{log:?}"
    );
    // the list is looked at again, so the new outcome is the one shown
    assert!(matches!(msgs.last(), Some(Msg::Snapshot { .. })));
}

#[test]
fn a_create_whose_reply_was_lost_is_retried_with_the_same_key_and_makes_one_draft() {
    let f = fake();
    *f.lose.borrow_mut() = 1;
    // first try: the draft is made, the link fails before the answer
    let (tx, rx) = channel();
    tx.send(create("create-k1")).unwrap();
    drop(tx);
    let (out, mut orx) = unbounded();
    let r = serve(&mut client(&f), &rx, &out, Duration::ZERO, false, &ok_head);
    assert!(r.is_err(), "the link failure ends the worker with its text");
    drop(out);
    let first: Vec<Msg> = std::iter::from_fn(|| orx.try_recv().ok()).collect();
    assert!(matches!(
        forms(&first)[..],
        [FormMsg::Failed { spent: false, .. }]
    ));
    // the retry (the form sends the same key): the orchestrator returns the first draft
    let msgs = run(&f, vec![create("create-k1")]);
    assert_eq!(
        f.drafts.borrow().len(),
        1,
        "a retry never makes a second draft"
    );
    assert!(matches!(forms(&msgs)[..], [FormMsg::Review(_)]));
}

#[test]
fn a_refused_create_is_a_spent_key_and_the_workers_keep_going() {
    let f = fake();
    let bad = Req::Submit(Submit::Create {
        workspace: "/work/app".into(),
        base_commit: BASE.into(),
        contract: Box::new(contract()),
        key: "create-k9".into(),
    });
    // the fake refuses an unknown method like a server refuses a request: a text, not a dead link
    struct Refuse(Fake);
    impl Transport for Refuse {
        fn call(&mut self, c: Call) -> Result<Value, ClientError> {
            if matches!(c, Call::OutcomeCreate { .. }) {
                return Err(ClientError::Refused(
                    "base_commit does not exist in the repository".into(),
                ));
            }
            self.0.call(c)
        }
    }
    let (tx, rx) = channel();
    tx.send(bad).unwrap();
    drop(tx);
    let (out, mut orx) = unbounded();
    assert_eq!(
        serve(
            &mut OutcomeClient::new(Refuse(f), Surface::Superagent),
            &rx,
            &out,
            Duration::ZERO,
            false,
            &ok_head,
        ),
        Ok(())
    );
    drop(out);
    let msgs: Vec<Msg> = std::iter::from_fn(|| orx.try_recv().ok()).collect();
    let fm = forms(&msgs);
    let [FormMsg::Failed { text, spent: true }] = fm.as_slice() else {
        panic!()
    };
    assert!(text.contains("does not exist"));
}

#[test]
fn a_revision_is_stored_then_the_servers_diff_is_read_and_nothing_is_approved() {
    let f = fake();
    let msgs = run(&f, vec![revise("revise-k1")]);
    let sent = f.sent.borrow();
    assert_eq!(sent.len(), 1, "only the revise call was made, no approve");
    assert_eq!(sent[0].0, "outcome.contract.revise");
    assert_eq!(sent[0].1["expected_revision"], 12);
    assert_eq!(sent[0].1["idempotency_key"], "revise-k1");
    let fm = forms(&msgs);
    let [FormMsg::Review(r)] = fm.as_slice() else {
        panic!()
    };
    // the version and hash to approve are the ones the server reported
    assert_eq!((r.version, r.hash.as_str()), (2, sample::HASH_V2));
    let d = r.diff.as_ref().unwrap().as_ref().unwrap();
    assert_eq!(d.widens.len(), 1);
    assert_eq!(d.weakens.len(), 1);
    assert!(r.approvable());
    assert!(
        f.log
            .borrow()
            .contains(&"outcome.contract.diff".to_string())
    );
}

#[test]
fn a_stale_revision_says_to_reload_and_a_denied_one_says_when_a_contract_can_be_revised() {
    let f = fake();
    *f.revise.borrow_mut() = "stale";
    let msgs = run(&f, vec![revise("revise-k1")]);
    let fm = forms(&msgs);
    let [FormMsg::Failed { text, spent: true }] = fm.as_slice() else {
        panic!()
    };
    assert_eq!(text, STALE);
    assert!(text.contains("reload") && text.contains("Your text is kept"));
    let f = fake();
    *f.revise.borrow_mut() = "denied";
    let msgs = run(&f, vec![revise("revise-k1")]);
    let fm = forms(&msgs);
    let [FormMsg::Failed { text, spent: true }] = fm.as_slice() else {
        panic!()
    };
    assert!(text.contains("blocked with no live attempt"), "{text}");
    // a queued one is not a failure of the request: the same key looks again
    let f = fake();
    *f.revise.borrow_mut() = "queued";
    let msgs = run(&f, vec![revise("revise-k1")]);
    assert!(matches!(
        forms(&msgs)[..],
        [FormMsg::Failed { spent: false, .. }]
    ));
}

#[test]
fn a_stored_contract_that_is_not_the_reported_hash_cannot_be_approved() {
    let f = Fake {
        wrong_hash: true,
        ..fake()
    };
    let msgs = run(&f, vec![revise("revise-k1")]);
    let fm = forms(&msgs);
    let [FormMsg::Review(r)] = fm.as_slice() else {
        panic!()
    };
    assert!(
        r.contract
            .as_ref()
            .unwrap_err()
            .contains("cannot be approved")
    );
    assert!(!r.approvable());
}

#[test]
fn an_approval_sends_the_reviewed_version_hash_and_key_and_a_refusal_is_not_a_success() {
    let f = fake();
    let req = |key: &str| Req::Approve {
        outcome: "o_auth".into(),
        version: 2,
        hash: sample::HASH_V2.into(),
        key: key.into(),
    };
    let msgs = run(&f, vec![req("approve-k1")]);
    let sent = f.sent.borrow();
    assert_eq!(sent[0].0, "outcome.contract.approve");
    assert_eq!(
        (
            sent[0].1["version"].as_u64(),
            sent[0].1["hash"].as_str(),
            sent[0].1["idempotency_key"].as_str()
        ),
        (Some(2), Some(sample::HASH_V2), Some("approve-k1"))
    );
    assert!(matches!(forms(&msgs)[..], [FormMsg::Approved(n)] if n.contains("done")));
    drop(sent);
    *f.approve.borrow_mut() = "stale";
    let msgs = run(&f, vec![req("approve-k2")]);
    assert!(matches!(
        forms(&msgs)[..],
        [FormMsg::Failed { spent: true, .. }]
    ));
}

#[test]
fn a_draft_is_read_for_review_only_while_its_contract_is_not_approved() {
    let f = Fake {
        draft: true,
        ..fake()
    };
    let msgs = run(&f, vec![Req::Review("o_auth".into())]);
    let fm = forms(&msgs);
    let [FormMsg::Review(r)] = fm.as_slice() else {
        panic!()
    };
    assert_eq!((r.version, r.diff.is_none()), (1, true));
    let msgs = run(&fake(), vec![Req::Review("o_auth".into())]);
    assert!(matches!(forms(&msgs)[..], [FormMsg::Failed { .. }]));
}

#[test]
fn an_unapproved_later_version_is_reviewed_with_the_servers_diff_and_warnings() {
    let f = Fake {
        draft_v2: true,
        ..fake()
    };
    let msgs = run(&f, vec![Req::Review("o_auth".into())]);
    let fm = forms(&msgs);
    let [FormMsg::Review(r)] = fm.as_slice() else {
        panic!()
    };
    assert_eq!(r.version, 2);
    let d = r.diff.as_ref().unwrap().as_ref().unwrap();
    assert!(!d.widens.is_empty() && !d.weakens.is_empty());
    assert!(
        f.log
            .borrow()
            .contains(&"outcome.contract.diff".to_string())
    );
    assert!(r.approvable());
}

#[test]
fn a_later_version_whose_diff_cannot_be_read_cannot_be_approved() {
    let f = Fake {
        draft_v2: true,
        diff_fails: true,
        ..fake()
    };
    let msgs = run(&f, vec![Req::Review("o_auth".into())]);
    let fm = forms(&msgs);
    let [FormMsg::Review(r)] = fm.as_slice() else {
        panic!()
    };
    assert!(matches!(r.diff, Some(Err(_))) && !r.approvable());
}

#[test]
fn a_base_commit_that_moved_stops_the_create_and_shows_the_new_one() {
    let f = fake();
    let (tx, rx) = channel();
    tx.send(create("create-k1")).unwrap();
    drop(tx);
    let (out, mut orx) = unbounded();
    let moved = |_: &str| Ok("b".repeat(40));
    assert_eq!(
        serve(&mut client(&f), &rx, &out, Duration::ZERO, false, &moved),
        Ok(())
    );
    drop(out);
    let msgs: Vec<Msg> = std::iter::from_fn(|| orx.try_recv().ok()).collect();
    assert!(
        f.drafts.borrow().is_empty(),
        "nothing was created on a stale base"
    );
    let fm = forms(&msgs);
    let [FormMsg::BaseMoved { commit, text, .. }] = fm.as_slice() else {
        panic!()
    };
    assert_eq!(commit, &"b".repeat(40));
    assert!(text.contains("Nothing was created"));
    // a folder git cannot read is an error in the form, not a create
    let (tx, rx) = channel();
    tx.send(create("create-k2")).unwrap();
    drop(tx);
    let (out, mut orx) = unbounded();
    let bad = |_: &str| Err("is not the top folder".to_string());
    serve(&mut client(&f), &rx, &out, Duration::ZERO, false, &bad).unwrap();
    drop(out);
    let msgs: Vec<Msg> = std::iter::from_fn(|| orx.try_recv().ok()).collect();
    assert!(f.drafts.borrow().is_empty());
    assert!(matches!(forms(&msgs)[..], [FormMsg::Failed { .. }]));
}

#[test]
fn a_queued_approval_is_not_reported_as_approved() {
    let f = fake();
    *f.approve.borrow_mut() = "queued";
    let msgs = run(
        &f,
        vec![Req::Approve {
            outcome: "o_auth".into(),
            version: 2,
            hash: sample::HASH_V2.into(),
            key: "approve-k1".into(),
        }],
    );
    let fm = forms(&msgs);
    let [FormMsg::Failed { text, spent: false }] = fm.as_slice() else {
        panic!()
    };
    assert!(text.contains("not approved yet"), "{text}");
}

// ------------------------------------------------------------------------ the migration step

/// An orchestrator whose store is not migrated: only `orchestrator.enable` is answered.
struct Unmigrated {
    calls: Rc<RefCell<Vec<String>>>,
    reply: Option<Result<Value, ClientError>>,
}

impl Transport for Unmigrated {
    fn next_hint(
        &mut self,
        _: Duration,
    ) -> Result<Option<synthesis_app::proto::DecisionHint>, ClientError> {
        Ok(None)
    }
    fn call(&mut self, call: Call) -> Result<Value, ClientError> {
        let v = serde_json::to_value(&call).unwrap();
        self.calls
            .borrow_mut()
            .push(v["method"].as_str().unwrap().into());
        self.reply
            .take()
            .unwrap_or_else(|| Err(ClientError::Io("no reply scripted".into())))
    }
}

fn enable_run(
    reply: Result<Value, ClientError>,
    reqs: Vec<Req>,
) -> (Vec<String>, Vec<Result<(), String>>, bool) {
    let calls = Rc::new(RefCell::new(vec![]));
    let mut c = OutcomeClient::new(
        Unmigrated {
            calls: calls.clone(),
            reply: Some(reply),
        },
        Surface::Superagent,
    );
    let (tx, rx) = channel();
    for r in reqs {
        tx.send(r).unwrap();
    }
    drop(tx);
    let (out, mut orx) = unbounded();
    let reconnect = await_enable(&mut c, &rx, &out);
    drop(out);
    let res = std::iter::from_fn(|| orx.try_recv().ok())
        .filter_map(|m| match m {
            Msg::Enabled(_, r) => Some(r),
            _ => None,
        })
        .collect();
    let calls = calls.borrow().clone();
    (calls, res, reconnect)
}

fn good() -> Value {
    json!({"schema": 5, "backups": "backups/pre-v5-*", "disclosure": {
        "id": synthesis_app::outcome_policy::DISCLOSURE_ID,
        "text": synthesis_app::outcome_policy::INTEGRITY_DISCLOSURE}})
}

#[test]
fn the_store_is_migrated_only_on_the_explicit_request_then_the_worker_reconnects() {
    // nothing asked: nothing sent (a closed window ends the wait)
    let (calls, res, reconnect) = enable_run(Ok(good()), vec![]);
    assert!((calls.is_empty(), res.is_empty(), reconnect) == (true, true, false));
    // other requests are ignored; a retry only looks again
    let (calls, _, reconnect) = enable_run(Ok(good()), vec![Req::Refresh, Req::Retry]);
    assert!(calls.is_empty() && reconnect);
    let (calls, res, reconnect) = enable_run(Ok(good()), vec![Req::Enable(1)]);
    assert_eq!(calls, ["orchestrator.enable"]);
    assert_eq!(res, vec![Ok(())]);
    assert!(reconnect);
}

#[test]
fn a_disclosure_that_is_not_this_builds_is_refused_and_reported() {
    for bad in [
        json!({"schema": 5}),
        json!({"disclosure": {"id": "integrity-v0", "text": synthesis_app::outcome_policy::INTEGRITY_DISCLOSURE}}),
        json!({"disclosure": {"id": synthesis_app::outcome_policy::DISCLOSURE_ID, "text": "another wording"}}),
    ] {
        let (calls, res, reconnect) = enable_run(Ok(bad), vec![Req::Enable(1)]);
        assert_eq!(calls.len(), 1);
        let [Err(why)] = res.as_slice() else {
            panic!("{res:?}")
        };
        assert!(why.contains("integrity disclosure"), "{why}");
        assert!(
            !reconnect,
            "the worker keeps waiting; it never claims the store is enabled"
        );
    }
}

#[test]
fn a_refusal_is_reported_and_a_lost_link_reconnects() {
    let (_, res, reconnect) = enable_run(
        Err(ClientError::Refused("set the setting first".into())),
        vec![Req::Enable(1)],
    );
    assert!(
        matches!(res.as_slice(), [Err(w)] if w.contains("set the setting first")) && !reconnect
    );
    let (_, res, reconnect) =
        enable_run(Err(ClientError::Io("closed".into())), vec![Req::Enable(1)]);
    assert!(matches!(res.as_slice(), [Err(_)]) && reconnect);
}

// ------------------------------------------------------------------------ the synt beside the app

#[cfg(unix)]
mod beside {
    use super::{Sibling, found};
    use std::os::unix::fs::{PermissionsExt, symlink};

    /// `<tmp>/Synthesis.app/Contents/MacOS/{Synthesis,synt}` with the given mode on synt (None: no synt).
    fn bundle(mode: Option<u32>) -> (tempfile::TempDir, std::path::PathBuf) {
        let t = tempfile::tempdir().unwrap();
        let macos = std::fs::canonicalize(t.path())
            .unwrap()
            .join("Synthesis.app/Contents/MacOS");
        std::fs::create_dir_all(&macos).unwrap();
        std::fs::set_permissions(&macos, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::write(macos.join("Synthesis"), b"x").unwrap();
        if let Some(m) = mode {
            std::fs::write(macos.join("synt"), b"x").unwrap();
            std::fs::set_permissions(macos.join("synt"), std::fs::Permissions::from_mode(m))
                .unwrap();
        }
        (t, macos)
    }

    #[test]
    fn the_synt_inside_the_app_bundle_is_found_from_the_app_executable() {
        let (_t, macos) = bundle(Some(0o755));
        assert_eq!(found(&macos.join("Synthesis")), Some(macos.join("synt")));
    }

    #[test]
    fn a_missing_non_executable_or_writable_by_others_synt_is_refused() {
        let (_t, macos) = bundle(None);
        assert_eq!(found(&macos.join("Synthesis")), None);
        for mode in [0o644, 0o775, 0o757] {
            let (_t, macos) = bundle(Some(mode));
            assert_eq!(found(&macos.join("Synthesis")), None, "{mode:o}");
        }
        // a folder others can write to: the file could have been swapped
        let (_t, macos) = bundle(Some(0o755));
        std::fs::set_permissions(&macos, std::fs::Permissions::from_mode(0o777)).unwrap();
        assert_eq!(found(&macos.join("Synthesis")), None);
    }

    #[test]
    fn a_refused_synt_is_told_apart_from_none_and_a_writable_ancestor_in_the_bundle_refuses() {
        let (_t, macos) = bundle(None);
        assert_eq!(
            super::super::synt_beside(&macos.join("Synthesis")),
            Sibling::Absent
        );
        let (_t, macos) = bundle(Some(0o666));
        assert_eq!(
            super::super::synt_beside(&macos.join("Synthesis")),
            Sibling::Refused
        );
        // Contents is writable by others: the folder could have been swapped from above
        let (_t, macos) = bundle(Some(0o755));
        let contents = macos.parent().unwrap();
        std::fs::set_permissions(contents, std::fs::Permissions::from_mode(0o777)).unwrap();
        assert_eq!(
            super::super::synt_beside(&macos.join("Synthesis")),
            Sibling::Refused
        );
        std::fs::set_permissions(contents, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(matches!(
            super::super::synt_beside(&macos.join("Synthesis")),
            Sibling::Usable(_)
        ));
    }

    #[test]
    fn a_symlinked_synt_is_refused_and_a_symlinked_app_looks_where_it_really_is() {
        let (t, macos) = bundle(None);
        let elsewhere = t.path().join("elsewhere");
        std::fs::write(&elsewhere, b"x").unwrap();
        std::fs::set_permissions(&elsewhere, std::fs::Permissions::from_mode(0o755)).unwrap();
        symlink(&elsewhere, macos.join("synt")).unwrap();
        assert_eq!(found(&macos.join("Synthesis")), None, "never a symlink");
        // a link to the app from another folder does not pick the synt that sits beside the link
        let (_t2, real) = bundle(None);
        let dir = t.path().join("links");
        std::fs::create_dir(&dir).unwrap();
        std::fs::write(dir.join("synt"), b"x").unwrap();
        std::fs::set_permissions(dir.join("synt"), std::fs::Permissions::from_mode(0o755)).unwrap();
        symlink(real.join("Synthesis"), dir.join("Synthesis")).unwrap();
        assert_eq!(found(&dir.join("Synthesis")), None);
    }
}

#[test]
fn a_migration_confirm_that_comes_late_is_answered_not_dropped() {
    let msgs = run(&fake(), vec![Req::Enable(1)]);
    assert!(
        msgs.iter()
            .any(|m| matches!(m, Msg::Enabled(_, Err(e)) if e.contains("already migrated")))
    );
}

#[test]
fn a_worker_that_ended_makes_send_report_it() {
    let (tx, rx) = channel();
    let (out, _orx) = unbounded();
    let l = Link {
        tx,
        out,
        env: Env::default(),
    };
    assert!(l.try_send(Req::Refresh));
    drop(rx);
    assert!(!l.try_send(Req::Enable(1)));
}

#[test]
fn a_read_only_create_reads_no_head_and_sends_an_empty_base_commit() {
    let f = fake();
    let ro = synthesis_core::outcome::Contract::read_only(
        "Review the auth module".into(),
        &[],
        synthesis_core::outcome::Budgets::new(2.0),
    );
    let (tx, rx) = channel();
    tx.send(Req::Submit(Submit::Create {
        workspace: "/work/notes".into(),
        base_commit: String::new(),
        contract: Box::new(ro),
        key: "create-ro".into(),
    }))
    .unwrap();
    drop(tx);
    let (out, mut orx) = unbounded();
    // the folder is not a git repository: reading HEAD would fail, and must not be asked
    let no_git = |_: &str| -> Result<String, String> { Err("not a git repository".into()) };
    assert_eq!(
        serve(&mut client(&f), &rx, &out, Duration::ZERO, false, &no_git),
        Ok(())
    );
    drop(out);
    let msgs: Vec<Msg> = std::iter::from_fn(|| orx.try_recv().ok()).collect();
    let drafts = f.drafts.borrow();
    assert_eq!(drafts.len(), 1);
    assert_eq!(drafts[0].1["base_commit"].as_str(), Some(""));
    assert!(matches!(forms(&msgs)[..], [FormMsg::Review(_)]));
}

fn say(key: &str, text: &str) -> Req {
    Req::Message {
        outcome: "o_auth".into(),
        item: "wi3".into(),
        text: text.into(),
        key: key.into(),
    }
}

fn sent(m: &[Msg]) -> Vec<(&str, &Result<Delivery, String>)> {
    m.iter()
        .filter_map(|m| match m {
            Msg::Sent { key, result } => Some((key.as_str(), result)),
            _ => None,
        })
        .collect()
}

#[cfg(not(windows))]
#[test]
fn a_worker_message_is_sent_once_with_its_text_and_key_and_the_delivery_is_reported() {
    let f = fake();
    let msgs = run(&f, vec![say("k1", "  use REST\n")]);
    let calls = f.msgs.borrow();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0]["text"], "  use REST\n", "the exact text");
    assert_eq!(calls[0]["idempotency_key"], "k1");
    assert_eq!(
        (calls[0]["outcome"].as_str(), calls[0]["work_item"].as_str()),
        (Some("o_auth"), Some("wi3"))
    );
    assert_eq!(
        calls[0]["surface"], "superagent",
        "the person's surface, not a card"
    );
    assert!(
        calls[0].get("proposal_sha256").is_none_or(Value::is_null),
        "no card: {:?}",
        calls[0]
    );
    assert_eq!(
        sent(&msgs),
        [("k1", &Ok(Delivery::Live(Some("at4-1111".into()))))]
    );
    // the list is looked at again afterwards
    assert!(snapshots(&msgs).len() >= 2);
}

#[cfg(not(windows))]
#[test]
fn queued_denied_and_never_applied_are_told_in_words() {
    for (mode, want) in [
        ("queued", Ok(Delivery::Queued)),
        (
            "denied",
            Err(
                "at most 5 messages to one attempt in 10 minutes; wait, or the worker is flooded"
                    .to_string(),
            ),
        ),
    ] {
        let f = fake();
        *f.msg.borrow_mut() = mode;
        let msgs = run(&f, vec![say("k", "hi")]);
        assert_eq!(sent(&msgs), [("k", &want)], "{mode}");
    }
    // the owner has not applied it within the wait: not claimed as sent
    let f = fake();
    *f.msg.borrow_mut() = "pending";
    let msgs = run(&f, vec![say("k", "hi")]);
    assert!(
        matches!(sent(&msgs)[0].1, Err(w) if w.contains("Not confirmed yet")),
        "{:?}",
        sent(&msgs)
    );
}

#[cfg(not(windows))]
#[test]
fn hidden_characters_and_an_empty_or_long_text_never_reach_the_server() {
    let f = fake();
    let long = "x".repeat(4097);
    let msgs = run(
        &f,
        vec![say("a", "a\u{202e}b"), say("b", "  "), say("c", &long)],
    );
    assert!(f.msgs.borrow().is_empty());
    let s = sent(&msgs);
    assert!(matches!(s[0].1, Err(w) if w.contains("hidden, bidi or control")));
    assert!(matches!(s[1].1, Err(w) if w.contains("empty")));
    assert!(matches!(s[2].1, Err(w) if w.contains("longer than 4096")));
}

#[cfg(windows)]
#[test]
fn on_native_windows_nothing_is_sent_and_the_words_name_wsl() {
    let f = fake();
    let msgs = run(&f, vec![say("k", "hi")]);
    assert!(f.msgs.borrow().is_empty());
    assert!(matches!(sent(&msgs)[0].1, Err(w) if w.contains("WSL2")));
}

#[test]
fn the_snapshot_carries_the_workers_and_the_message_rows_with_their_acks() {
    let f = fake();
    let f = Fake { inbox: true, ..f };
    let msgs = run(&f, vec![]);
    let d = snapshots(&msgs)[0].1.clone().unwrap();
    assert!(matches!(d.workers, InspectorContent::Workers(_)));
    let got: Vec<_> = d.messages.iter().map(|m| (m.seq, m.label())).collect();
    assert_eq!(
        got,
        [
            (6, "delivered to at4-1111".to_string()),
            (7, "queued for the next attempt".to_string())
        ]
    );
    assert!(d.messages.iter().all(|m| !m.text.contains('\u{1b}')));
    assert_eq!(d.messages[1].work_item.as_deref(), Some("wi4"));
}

#[test]
fn the_engines_features_say_whether_a_worker_may_be_messaged() {
    let has = |s: &Value, f: &str| {
        s["features"]
            .as_array()
            .is_some_and(|a| a.iter().any(|x| x == f))
    };
    assert!(has(
        &json!({"features": ["read_only_goal", "attempt_message"]}),
        "attempt_message"
    ));
    assert!(!has(
        &json!({"features": ["read_only_goal", "report_review"]}),
        "attempt_message"
    ));
}

#[test]
fn the_queue_is_read_only_when_the_engine_has_it_and_history_resumes_at_its_cursor() {
    let f = fake();
    run(&f, vec![]);
    assert!(!f.log.borrow().iter().any(|m| m.starts_with("approvals.")));
    let (tx, rx) = channel();
    tx.send(Req::Refresh).unwrap();
    drop(tx);
    let (out, mut orx) = unbounded();
    serve(&mut client(&f), &rx, &out, Duration::ZERO, true, &ok_head).unwrap();
    drop(out);
    let reads: Vec<_> = std::iter::from_fn(|| orx.try_recv().ok())
        .filter_map(|m| match m {
            Msg::Queue(q) => q,
            _ => None,
        })
        .collect();
    assert_eq!(reads.len(), 2, "the first look and the refresh");
    // the second read asked after the cursor and still holds the record of the first
    assert_eq!(reads[1].answered.len(), 1);
    assert_eq!(reads[1].answered[0].0, "o_auth");
}
