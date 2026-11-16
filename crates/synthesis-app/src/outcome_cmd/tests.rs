//! Dispatch and rendering only: behaviour is proven by the protocol tests (`outcome_proto.rs`).
//! A scripted server answers by method name; the calls it saw are recorded.

use std::cell::RefCell;
use std::io::Cursor;
use std::rc::Rc;

use serde_json::{Value, json};
use synthesis_view::outcomes::uncovered;

use super::*;
use crate::outcome_client::ClientError;
use crate::proto::Call;

type Seen = Rc<RefCell<Vec<Value>>>;

struct Fake {
    seen: Seen,
    answers: Vec<(&'static str, Value)>,
}

impl Transport for Fake {
    fn call(&mut self, call: Call) -> Result<Value, ClientError> {
        let v = serde_json::to_value(&call).unwrap();
        let m = v["method"].as_str().unwrap().to_string();
        let key = v["params"]["idempotency_key"].clone();
        self.seen.borrow_mut().push(v);
        let mut a = self
            .answers
            .iter()
            .find(|(k, _)| *k == m)
            .map(|(_, a)| a.clone())
            .ok_or_else(|| ClientError::Refused(format!("no answer for {m}")))?;
        // a command reply names the key it answers, as the server does
        if a.get("kind").is_some() && !key.is_null() {
            a["idempotency_key"] = key;
        }
        Ok(a)
    }
}

fn brief() -> Value {
    json!({"why": "the worker needs an answer", "delta_from_contract": "Which API shape?",
        "alternatives": [{"id": "REST", "label": "REST", "tradeoffs": "simple, chatty"},
                         {"id": "RPC", "label": "RPC", "tradeoffs": "fast, coupled"}],
        "evidence": {"work_item": "one", "attempt_ordinal": 3, "refused_calls": ["Bash: rm x"]},
        "unknowns": ["is the client typed?"], "defer_consequence": "the item stays on hold",
        "safe_default": "HOLD", "future_field": "shown though this build does not know it"})
}

fn decision(state: &str) -> Value {
    json!({"id": "d1", "kind": "ambiguity", "state": state, "revision": 2, "brief": brief(),
        "brief_sha256": "a".repeat(64), "expires_at": "2030-01-01T00:00:00.000Z"})
}

fn outcome() -> Value {
    json!({"id": "o1", "goal": "ship it", "state": "running", "revision": 4,
        "needs_you": {"decisions_pending": 1, "blocked": false, "verified_awaiting_ack": false}})
}

/// Echoes whatever key the client generated, as the server does.
fn rig(answers: Vec<(&'static str, Value)>) -> (OutcomeClient<Fake>, Seen) {
    let seen = Seen::default();
    let t = Fake {
        seen: seen.clone(),
        answers,
    };
    (OutcomeClient::new(t, Surface::Cli), seen)
}

fn run(
    c: &mut OutcomeClient<Fake>,
    cmd: OutcomeCmd,
    json: bool,
    input: &str,
    interactive: bool,
) -> (Result<u8, String>, String) {
    let mut out = vec![];
    let mut inp = Cursor::new(input.as_bytes().to_vec());
    let r = dispatch(
        c,
        cmd,
        json,
        &mut Io {
            out: &mut out,
            input: &mut inp,
            interactive,
        },
        Duration::from_secs(2),
    );
    (r, String::from_utf8(out).unwrap())
}

fn methods(seen: &Seen) -> Vec<String> {
    seen.borrow()
        .iter()
        .map(|v| v["method"].as_str().unwrap().to_string())
        .collect()
}

fn resolve(choice: Option<&str>, rev: Option<i64>, hash: Option<String>) -> OutcomeCmd {
    OutcomeCmd::Resolve {
        outcome: "o1".into(),
        decision: "d1".into(),
        choice: choice.map(Into::into),
        revision: rev,
        brief_hash: hash,
        note: None,
    }
}

fn decisions_rig() -> (OutcomeClient<Fake>, Seen) {
    rig(vec![(
        "decision.list",
        json!({"decisions": [decision("pending")]}),
    )])
}

// surface-brief-all-fields-shown: the CLI's own layout drops no field of the brief
#[test]
fn every_non_empty_brief_field_is_printed_in_server_order_with_the_revision_and_hash() {
    let (mut c, _) = decisions_rig();
    let (r, out) = run(
        &mut c,
        OutcomeCmd::Decisions {
            id: "o1".into(),
            decision: None,
        },
        false,
        "",
        false,
    );
    assert_eq!(r, Ok(0));
    assert_eq!(uncovered(&brief(), &out), Vec::<String>::new(), "{out}");
    assert!(out.contains("pending") && out.contains("2030-01-01"));
    assert!(out.contains("Revision: 2") && out.contains(&"a".repeat(64)));
    // alternatives keep the server's order
    let (a, b) = (out.find("REST").unwrap(), out.find("RPC").unwrap());
    assert!(a < b);
    // each brief label shows once per field, in the order the server sent them
    let at = |s: &str| out.find(s).unwrap_or_else(|| panic!("{s} in {out}"));
    assert!(
        at("Why you:") < at("Change from the contract:")
            && at("Change from the contract:") < at("Alternatives:")
    );
    assert!(at("future_field:") > at("If you do nothing:"));
}

// surface-no-preselection
#[test]
fn nothing_is_preselected_and_the_answers_are_numbered_without_a_marker() {
    let (mut c, _) = decisions_rig();
    let (_, out) = run(
        &mut c,
        OutcomeCmd::Decisions {
            id: "o1".into(),
            decision: None,
        },
        false,
        "",
        false,
    );
    let answers: Vec<_> = out
        .lines()
        .skip_while(|l| !l.starts_with("Answers"))
        .collect();
    assert_eq!(answers[0], "Answers (none is chosen for you):");
    assert_eq!(
        &answers[1..6],
        [
            "  1. REST: REST",
            "  2. RPC: RPC",
            "  3. reject: Reject",
            "  4. request_evidence: Request evidence",
            "  5. edit_scope: Edit scope"
        ]
    );
    let (_, j) = run(
        &mut c,
        OutcomeCmd::Decisions {
            id: "o1".into(),
            decision: None,
        },
        true,
        "",
        false,
    );
    for bad in ["selected", "recommended", "\"default", "preselected"] {
        assert!(!j.contains(bad), "{bad} in {j}");
    }
    // a brief that names a preferred option is refused whole
    let mut d = decision("pending");
    d["brief"]["recommended"] = json!("RPC");
    let (mut c, _) = rig(vec![("decision.list", json!({"decisions": [d]}))]);
    let (r, out) = run(
        &mut c,
        OutcomeCmd::Decisions {
            id: "o1".into(),
            decision: None,
        },
        false,
        "",
        false,
    );
    assert!(r.unwrap_err().contains("recommended") && out.is_empty());
}

#[test]
fn resolve_sends_nothing_without_an_explicit_choice() {
    for interactive in [false, true] {
        let (mut c, seen) = decisions_rig();
        // non-interactive: no prompt; interactive: an empty or unknown answer is no answer
        let (r, out) = run(&mut c, resolve(None, None, None), false, "\n", interactive);
        assert!(r.is_err(), "{interactive}");
        assert!(
            out.contains("Which API shape?"),
            "the card is shown first: {out}"
        );
        assert!(!methods(&seen).contains(&"decision.resolve".to_string()));
    }
    let (mut c, seen) = decisions_rig();
    let (r, _) = run(&mut c, resolve(None, None, None), false, "99\n", true);
    assert!(r.is_err() && !methods(&seen).contains(&"decision.resolve".to_string()));
    // a choice without the revision and brief hash is not enough
    let (r, _) = run(&mut c, resolve(Some("REST"), None, None), false, "", false);
    assert!(r.unwrap_err().contains("--revision"));
    assert!(!methods(&seen).contains(&"decision.resolve".to_string()));
}

#[test]
fn resolve_refuses_a_stale_revision_or_hash_and_an_answer_the_card_does_not_offer() {
    let (mut c, seen) = decisions_rig();
    let (r, out) = run(
        &mut c,
        resolve(Some("REST"), Some(1), Some("a".repeat(64))),
        false,
        "",
        false,
    );
    assert!(r.unwrap_err().contains("stale"));
    assert!(
        out.contains("Revision: 2"),
        "the current card is shown: {out}"
    );
    let (r, _) = run(
        &mut c,
        resolve(Some("REST"), Some(2), Some("b".repeat(64))),
        false,
        "",
        false,
    );
    assert!(r.unwrap_err().contains("stale"));
    let (r, _) = run(
        &mut c,
        resolve(Some("MAYBE"), Some(2), Some("a".repeat(64))),
        false,
        "",
        false,
    );
    assert!(r.unwrap_err().contains("does not offer"));
    assert!(!methods(&seen).contains(&"decision.resolve".to_string()));
}

#[test]
fn resolve_sends_the_choice_with_the_revision_and_hash_of_the_card_shown() {
    for (choice, input, interactive) in [(Some("RPC"), "", false), (None, "2\n", true)] {
        let (mut c, seen) = rig(vec![
            ("decision.list", json!({"decisions": [decision("pending")]})),
            (
                "decision.resolve",
                json!({"kind": "decision.resolve", "state": "applied"}),
            ),
        ]);
        // the scripted server echoes the key it was sent
        let (rev, hash) = choice.map_or((None, None), |_| (Some(2), Some("a".repeat(64))));
        let (r, out) = run(
            &mut c,
            resolve(choice, rev, hash),
            false,
            input,
            interactive,
        );
        assert_eq!(r, Ok(0), "{out}");
        let sent = seen
            .borrow()
            .iter()
            .find(|v| v["method"] == "decision.resolve")
            .cloned()
            .unwrap();
        let p = &sent["params"];
        assert_eq!(
            (p["choice"].as_str(), p["expected_revision"].as_i64()),
            (Some("RPC"), Some(2))
        );
        assert_eq!(p["brief_sha256"], "a".repeat(64));
        assert_eq!(p["surface"], "cli");
        assert!(out.contains("decision.resolve: done"), "{out}");
    }
}

#[test]
fn a_decision_that_is_not_pending_cannot_be_answered() {
    let (mut c, seen) = rig(vec![(
        "decision.list",
        json!({"decisions": [decision("expired")]}),
    )]);
    let (_, out) = run(
        &mut c,
        OutcomeCmd::Decisions {
            id: "o1".into(),
            decision: None,
        },
        false,
        "",
        false,
    );
    assert!(
        out.contains("cannot be answered (expired)") && !out.contains("1. REST"),
        "{out}"
    );
    let (r, _) = run(
        &mut c,
        resolve(Some("REST"), Some(2), Some("a".repeat(64))),
        false,
        "",
        false,
    );
    assert!(r.unwrap_err().contains("does not offer"));
    assert!(!methods(&seen).contains(&"decision.resolve".to_string()));
}

fn status_rig() -> (OutcomeClient<Fake>, Seen) {
    rig(vec![
        ("outcome.get", outcome()),
        (
            "outcome.contract.get",
            json!({"version": 1, "hash": "h1", "approved": true, "contract": {"goal": "ship it"}}),
        ),
        (
            "plan.get",
            json!({"plan_version": 1, "items": [
            {"id": "w1", "key": "implement", "kind": "agent", "state": "waiting_decision", "retries_used": 1}]}),
        ),
        (
            "evidence.get",
            json!({"evidence": [{"check_id": "tests", "result": "pass",
            "trust_class": "independent_check", "subject_commit": "abc", "criteria_ids": ["c1"]}]}),
        ),
        ("decision.list", json!({"decisions": [decision("pending")]})),
    ])
}

#[test]
fn status_text_starts_with_needs_you_and_shows_the_servers_state() {
    let (mut c, _) = status_rig();
    let (r, out) = run(
        &mut c,
        OutcomeCmd::Status { id: "o1".into() },
        false,
        "",
        false,
    );
    assert_eq!(r, Ok(0));
    let want = "Needs you: 1 decision(s) pending
Outcome o1: running (revision 4)
Goal: ship it
Contract: vm1 h1 (approved)
Work items:
  implement [agent] waiting_decision, retries used 1
Evidence:
  tests pass (independent_check) commit abc criteria [\"c1\"]
Decisions: 1 total, 1 pending (synt outcome decisions o1)
";
    // the contract version is printed as the server sent it
    assert_eq!(out.replace("vm1", "v1"), want.replace("vm1", "v1"), "{out}");
}

#[test]
fn status_json_schema_is_stable() {
    let (mut c, _) = status_rig();
    let (r, out) = run(
        &mut c,
        OutcomeCmd::Status { id: "o1".into() },
        true,
        "",
        false,
    );
    assert_eq!(r, Ok(0));
    let v: Value = serde_json::from_str(out.trim()).unwrap();
    let keys = |v: &Value| -> Vec<String> {
        let mut k: Vec<String> = v.as_object().unwrap().keys().cloned().collect();
        k.sort();
        k
    };
    assert_eq!(
        keys(&v),
        [
            "contract",
            "decisions",
            "evidence",
            "outcome",
            "plan_version",
            "schema",
            "work_items"
        ]
    );
    assert_eq!(v["schema"], 1);
    assert_eq!(
        keys(&v["outcome"]),
        ["goal", "id", "needs_you", "revision", "state"]
    );
    assert_eq!(
        keys(&v["outcome"]["needs_you"]),
        ["blocked", "decisions_pending", "verified_awaiting_ack"]
    );
    assert_eq!(keys(&v["contract"]), ["approved", "hash", "version"]);
    let d = &v["decisions"][0];
    assert_eq!(
        keys(d),
        [
            "brief_sha256",
            "expires_at",
            "id",
            "kind",
            "options",
            "revision",
            "rows",
            "state"
        ]
    );
    assert_eq!(d["options"][0], json!({"id": "REST", "label": "REST"}));
    assert_eq!(
        d["rows"][0],
        json!({"label": "Why you", "text": "the worker needs an answer"})
    );
    assert!(out.lines().count() == 1, "one object per call");
}

#[test]
fn control_text_from_the_server_is_made_printable() {
    let dirty = "a\u{1b}[31mred\u{1b}]52;c;AAAA\u{7}";
    let mut o = outcome();
    o["goal"] = json!(dirty);
    let (mut c, _) = rig(vec![("outcome.list", json!({"outcomes": [o]}))]);
    let (_, out) = run(&mut c, OutcomeCmd::List, false, "", false);
    assert!(!out.contains('\u{1b}'), "{out:?}");
    // every field of a decision card, in every command that prints one
    let mut d = decision("pending");
    d["kind"] = json!(dirty);
    d["expires_at"] = json!(dirty);
    d["brief"]["why"] = json!(dirty);
    d["brief"]["alternatives"][0]["label"] = json!(dirty);
    d["brief"]["alternatives"][0]["tradeoffs"] = json!(dirty);
    d["brief"]["future_field"] = json!({dirty: dirty});
    let (mut c, _) = rig(vec![("decision.list", json!({"decisions": [d]}))]);
    for cmd in [
        OutcomeCmd::Decisions {
            id: "o1".into(),
            decision: None,
        },
        resolve(None, None, None),
        resolve(Some("REST"), Some(1), Some("x".into())),
    ] {
        let (_, out) = run(&mut c, cmd, false, "\n", true);
        assert!(!out.chars().any(|c| c.is_control() && c != '\n'), "{out:?}");
    }
}

#[test]
fn a_command_is_followed_until_the_owner_applied_it_and_a_lost_race_exits_1() {
    let reply = |state: &str| json!({"kind": "outcome.start", "state": state});
    let (mut c, _) = rig(vec![
        ("outcome.start", reply("queued")),
        ("command.status", reply("applied")),
    ]);
    let cmd = |id: &str| {
        OutcomeCmd::Start(Cmd {
            id: id.into(),
            expected_revision: None,
        })
    };
    let (r, out) = run(&mut c, cmd("o1"), false, "", false);
    assert_eq!((r, out.as_str()), (Ok(0), "outcome.start: done\n"));
    // a lost race is not a failure of the CLI: exit 1 and the server's words
    let lost = json!({"kind": "outcome.start", "state": "stale"});
    let (mut c, _) = rig(vec![("outcome.start", lost)]);
    let (r, out) = run(&mut c, cmd("o1"), false, "", false);
    assert_eq!(r, Ok(1));
    assert!(out.contains("stale") && out.contains("reload"), "{out}");
}

#[test]
fn platform_refusal_is_the_wsl2_text_for_commands_that_run_attempts_only() {
    let start = OutcomeCmd::Start(Cmd {
        id: "o".into(),
        expected_revision: None,
    });
    assert_eq!(platform_refusal(true, &start), Some(WSL2_REFUSAL));
    assert!(WSL2_REFUSAL.contains("WSL2") && WSL2_REFUSAL.contains("native Windows"));
    assert_eq!(platform_refusal(false, &start), None);
    for cmd in [
        OutcomeCmd::List,
        OutcomeCmd::Status { id: "o".into() },
        OutcomeCmd::Decisions {
            id: "o".into(),
            decision: None,
        },
    ] {
        assert_eq!(platform_refusal(true, &cmd), None);
    }
}

#[test]
fn enable_shows_the_notice_and_changes_nothing_without_yes() {
    let (mut c, seen) = rig(vec![]);
    let (r, out) = run(&mut c, OutcomeCmd::Enable { yes: false }, false, "", false);
    assert!(r.unwrap_err().contains("--yes"));
    assert!(out.contains("schema v5") && out.contains("same user outside the sandbox"));
    assert!(seen.borrow().is_empty());
}

#[test]
fn inspect_names_the_tabs_and_the_diff_tab_says_it_is_not_there() {
    let (mut c, _) = rig(vec![]);
    let (r, out) = run(
        &mut c,
        OutcomeCmd::Inspect {
            id: "o1".into(),
            tab: "diff".into(),
            attempt: None,
        },
        false,
        "",
        false,
    );
    assert_eq!(r, Ok(0));
    assert!(out.contains("not available"), "{out}");
    let (r, _) = run(
        &mut c,
        OutcomeCmd::Inspect {
            id: "o1".into(),
            tab: "nope".into(),
            attempt: None,
        },
        false,
        "",
        false,
    );
    assert!(r.unwrap_err().contains("overview, workers"));
}
