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
    /// Hints `next_hint` hands out, then the link "closes".
    hints: Vec<crate::proto::DecisionHint>,
}

impl Transport for Fake {
    fn next_hint(
        &mut self,
        _wait: Duration,
    ) -> Result<Option<crate::proto::DecisionHint>, ClientError> {
        if self.hints.is_empty() {
            return Err(ClientError::Io("the orchestrator closed the link".into()));
        }
        Ok(Some(self.hints.remove(0)))
    }

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
        hints: vec![],
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

// after `request_evidence`: the old card is closed, the new one names it and shows the evidence
#[test]
fn a_rebriefed_card_shows_its_supersedes_link_and_the_old_card_cannot_be_answered() {
    let old = {
        let mut d = decision("invalidated");
        d["revision"] = json!(3);
        d["brief_sha256"] = json!("b".repeat(64));
        d
    };
    let new = {
        let mut d = decision("pending");
        d["id"] = json!("d2");
        d["revision"] = json!(2);
        d["brief_sha256"] = json!("c".repeat(64));
        d["brief"]["supersedes"] = json!("d1");
        d["brief"]["rebriefs_left"] = json!(2);
        d["brief"]["evidence_summary"] = json!({"rows": [{"check": "unit", "result": "failed"}], "source": "stored rows; no model ran"});
        d
    };
    let (mut c, seen) = rig(vec![(
        "decision.list",
        json!({"decisions": [old, new.clone()]}),
    )]);
    let list = OutcomeCmd::Decisions {
        id: "o1".into(),
        decision: None,
        watch: false,
    };
    let (r, out) = run(&mut c, list, false, "", false);
    assert_eq!(r, Ok(0));
    assert!(
        out.contains("Decision d2") && out.contains("pending"),
        "{out}"
    );
    assert!(out.contains("supersedes:\nd1"), "{out}");
    assert!(
        out.contains("rebriefs_left") && out.contains("unit") && out.contains("no model ran"),
        "{out}"
    );
    assert!(
        out.contains("Decision d1") && out.contains("invalidated"),
        "{out}"
    );
    assert_eq!(
        uncovered(&new["brief"], &out),
        Vec::<String>::new(),
        "{out}"
    );
    // the old card refuses an answer and nothing is sent
    let (r, _) = run(
        &mut c,
        resolve(Some("REST"), Some(3), Some("b".repeat(64))),
        false,
        "",
        false,
    );
    assert!(r.unwrap_err().contains("does not offer"), "stale old card");
    assert!(!methods(&seen).contains(&"decision.resolve".to_string()));
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
            watch: false,
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
            watch: false,
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
            watch: false,
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
            watch: false,
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
            watch: false,
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
            watch: false,
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
            watch: false,
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
fn a_closed_gate_refuses_enable_before_the_notice_and_only_enable() {
    let en = OutcomeCmd::Enable { yes: true };
    let why = gate_refusal(false, &en).unwrap();
    assert!(why.contains("the release gate is not passed"));
    assert_eq!(gate_refusal(true, &en), None);
    assert_eq!(
        gate_refusal(false, &OutcomeCmd::Enable { yes: false }),
        Some(why)
    );
    assert_eq!(gate_refusal(false, &OutcomeCmd::List), None);
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

#[cfg(unix)]
#[test]
fn read_json_opens_a_plain_file_only_and_never_blocks_on_a_fifo() {
    let d = tempfile::tempdir().unwrap();
    let ok = d.path().join("ok.json");
    std::fs::write(&ok, "[1]").unwrap();
    assert_eq!(read_json::<Vec<u8>>(&ok, "x").unwrap(), [1]);
    let link = d.path().join("link.json");
    std::os::unix::fs::symlink(&ok, &link).unwrap();
    assert!(read_json::<Vec<u8>>(&link, "x").is_err());
    let fifo = d.path().join("fifo.json");
    assert!(
        std::process::Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .unwrap()
            .success()
    );
    // O_NONBLOCK: opening a FIFO nobody writes returns at once, and the handle is not a file
    let e = read_json::<Vec<u8>>(&fifo, "x").unwrap_err();
    assert!(e.contains("not a regular file"), "{e}");
}

/// Fails the first call as a lost link, answers the second; records every key it saw.
struct Lossy {
    keys: Rc<RefCell<Vec<String>>>,
    calls: u32,
}

impl Transport for Lossy {
    fn call(&mut self, call: Call) -> Result<Value, ClientError> {
        if let Call::OutcomeCreate {
            idempotency_key, ..
        } = call
        {
            self.keys.borrow_mut().push(idempotency_key.unwrap());
        }
        self.calls += 1;
        if self.calls == 1 {
            return Err(ClientError::Io("the orchestrator closed the link".into()));
        }
        Ok(
            json!({"id": "o1", "state": "draft", "contract_version": 1, "contract_hash": "a".repeat(64)}),
        )
    }
}

#[test]
fn a_create_retried_after_a_lost_link_reuses_its_key_and_a_new_one_gets_a_new_key() {
    let keys = Rc::new(RefCell::new(vec![]));
    let mut c = OutcomeClient::new(
        Lossy {
            keys: keys.clone(),
            calls: 0,
        },
        synthesis_session::outcome::Surface::Cli,
    );
    let contract = || {
        serde_json::from_value(json!({"version": 1, "goal": "g", "criteria": [],
        "envelope": {"allowed_paths": [], "denied_paths": [], "capabilities": {}, "budgets": {"max_cost_usd": 1.0}, "quality_gates": []},
        "checks": []})).unwrap()
    };
    assert!(c.create("/w", "abc", contract()).is_err());
    c.create("/w", "abc", contract()).unwrap();
    c.create("/w", "abc", contract()).unwrap();
    let k = keys.borrow();
    assert_eq!(k[0], k[1], "the retry sends the same key");
    assert_ne!(k[1], k[2], "an answered create ends the key");
}

fn hint(outcome: &str) -> crate::proto::DecisionHint {
    crate::proto::DecisionHint {
        outcome: outcome.into(),
        id: "d1".into(),
        revision: 2,
        state: "pending".into(),
    }
}

#[test]
fn watch_refetches_on_a_hint_for_its_outcome_and_shows_a_card_only_when_it_changed() {
    let seen = Seen::default();
    let mut c = OutcomeClient::new(
        Fake {
            seen: seen.clone(),
            answers: vec![("decision.list", json!({"decisions": [decision("pending")]}))],
            hints: vec![hint("other"), hint("o1")],
        },
        Surface::Cli,
    );
    let (r, out) = run(
        &mut c,
        OutcomeCmd::Decisions {
            id: "o1".into(),
            decision: None,
            watch: true,
        },
        false,
        "",
        false,
    );
    // the link closing ends the watch
    assert!(r.unwrap_err().contains("closed"), "{out}");
    // one fetch at the start, one for the hint of o1, none for the hint of another outcome
    assert_eq!(
        methods(&seen)
            .iter()
            .filter(|m| *m == "decision.list")
            .count(),
        2
    );
    // the unchanged card was shown once, from the server's answer
    assert_eq!(out.matches("Revision: 2").count(), 1, "{out}");
}

#[test]
fn the_enable_notice_starts_with_the_integrity_disclosure() {
    assert!(ENABLE_NOTICE.starts_with(crate::outcome_policy::INTEGRITY_DISCLOSURE));
}

#[test]
fn enable_refuses_a_disclosure_that_is_not_this_builds_id_and_text() {
    use crate::outcome_policy::{DISCLOSURE_ID, INTEGRITY_DISCLOSURE};
    let reply = |id: &str, text: &str| json!({"schema": 5, "backups": "b", "disclosure": {"id": id, "text": text}});
    for (id, text) in [
        ("integrity-v0", INTEGRITY_DISCLOSURE),
        (DISCLOSURE_ID, "an older, shorter text"),
        (DISCLOSURE_ID, ""),
    ] {
        let (mut c, _) = rig(vec![("orchestrator.enable", reply(id, text))]);
        let err = c.enable().unwrap_err().to_string();
        assert!(err.contains("integrity disclosure"), "{err}");
    }
    let (mut c, _) = rig(vec![(
        "orchestrator.enable",
        reply(DISCLOSURE_ID, INTEGRITY_DISCLOSURE),
    )]);
    assert!(c.enable().is_ok());
}

fn revise_rig(state: &str) -> (OutcomeClient<Fake>, Seen, tempfile::TempDir, PathBuf) {
    let d = tempfile::tempdir().unwrap();
    let f = d.path().join("c.json");
    std::fs::write(
        &f,
        json!({"goal": "g", "criteria": [{"id": "c1", "text": "t"}],
            "envelope": {"budgets": {"max_cost_usd": 9.0}}})
        .to_string(),
    )
    .unwrap();
    let hash = "c".repeat(64);
    let diff = json!({"base": {"version": 1, "hash": "b".repeat(64), "approved": true},
        "version": 2, "hash": hash, "approved": false, "widens_scope": true,
        "diff": {"changes": [{"area": "budgets", "key": "max_cost_usd", "old": "5", "new": "9"},
                             {"area": "allowed_paths", "key": "docs/**", "old": null, "new": "added\u{1b}[31m"}],
                 "widens": ["budget max_cost_usd is raised or removed"], "weakens": []}});
    let (c, seen) = rig(vec![
        (
            "outcome.get",
            json!({"id": "o1", "goal": "g", "state": "blocked", "revision": 7,
            "needs_you": {"decisions_pending": 0, "blocked": true, "verified_awaiting_ack": false}}),
        ),
        (
            "outcome.contract.revise",
            json!({"kind": "outcome.contract.revise", "state": state,
            "result": {"version": 2, "hash": hash}}),
        ),
        ("outcome.contract.diff", diff),
    ]);
    (c, seen, d, f)
}

fn revise_cmd(f: &std::path::Path, rev: Option<i64>) -> OutcomeCmd {
    OutcomeCmd::Revise {
        id: "o1".into(),
        contract_file: f.to_path_buf(),
        expected_revision: rev,
    }
}

#[test]
fn revise_sends_the_revision_the_person_saw_and_prints_the_servers_diff_then_the_approve_line() {
    let (mut c, seen, _d, f) = revise_rig("applied");
    let (r, out) = run(&mut c, revise_cmd(&f, None), false, "", false);
    assert_eq!(r, Ok(0), "{out}");
    // the outcome revision is read first, the contract goes with it, the diff is the server's
    assert_eq!(
        methods(&seen),
        [
            "outcome.get",
            "outcome.contract.revise",
            "outcome.contract.diff"
        ]
    );
    let sent = seen.borrow()[1]["params"].clone();
    assert_eq!(sent["expected_revision"], 7);
    assert_eq!(sent["contract"]["goal"], "g");
    assert_eq!(seen.borrow()[2]["params"]["version"], 2);
    let hash = "c".repeat(64);
    assert!(
        out.contains(&format!("Revised contract v2 {hash} (not approved).")),
        "{out}"
    );
    assert!(out.contains("budgets max_cost_usd: 5 -> 9"), "{out}");
    assert!(
        out.contains("allowed_paths docs/**: (none) -> added"),
        "{out}"
    );
    assert!(
        out.contains("WARNING: this revision gives attempts MORE room"),
        "{out}"
    );
    assert!(
        out.contains("  budget max_cost_usd is raised or removed"),
        "{out}"
    );
    assert!(!out.contains("lowers the bar"), "{out}");
    assert!(
        out.ends_with(&format!(
            "Approve: synt outcome approve o1 --version 2 --hash {hash}\n"
        )),
        "{out}"
    );
    assert!(!out.chars().any(|c| c.is_control() && c != '\n'), "{out:?}");
    // an explicit revision is used as given and `outcome.get` is not asked
    let (mut c, seen, _d, f) = revise_rig("applied");
    run(&mut c, revise_cmd(&f, Some(3)), false, "", false)
        .0
        .unwrap();
    assert_eq!(seen.borrow()[0]["params"]["expected_revision"], 3);
    assert_eq!(methods(&seen)[0], "outcome.contract.revise");
}

#[test]
fn revise_json_has_a_stable_shape_and_a_lost_race_prints_no_diff_and_exits_1() {
    let (mut c, _seen, _d, f) = revise_rig("applied");
    let (r, out) = run(&mut c, revise_cmd(&f, Some(7)), true, "", false);
    assert_eq!(r, Ok(0));
    let v: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["schema"], 1);
    assert_eq!(v["version"], 2);
    assert_eq!(v["approved"], false);
    assert_eq!(v["widens_scope"], true);
    assert_eq!(v["diff"]["changes"].as_array().unwrap().len(), 2);
    let (mut c, seen, _d, f) = revise_rig("stale");
    let (r, out) = run(&mut c, revise_cmd(&f, Some(7)), false, "", false);
    assert_eq!(r, Ok(1), "{out}");
    assert!(!methods(&seen).contains(&"outcome.contract.diff".to_string()));
    assert!(!out.contains("Approve:"), "{out}");
}

#[test]
fn revise_refuses_a_file_that_is_not_a_contract_and_a_shape_the_server_does_not_make() {
    let (mut c, seen, d, _) = revise_rig("applied");
    let bad = d.path().join("bad.json");
    std::fs::write(&bad, r#"{"goal": "g"}"#).unwrap();
    assert!(
        run(&mut c, revise_cmd(&bad, Some(1)), false, "", false)
            .0
            .is_err()
    );
    assert!(seen.borrow().is_empty(), "nothing is sent for a bad file");
    // a version or hash that is not what the server makes is never printed in a command to paste
    let (_, _, _d, f) = revise_rig("applied");
    let mut c = {
        let seen = Seen::default();
        OutcomeClient::new(
            Fake {
                seen,
                answers: vec![(
                    "outcome.contract.revise",
                    json!({"kind": "outcome.contract.revise", "state": "applied",
                        "result": {"version": 2, "hash": "x; rm -rf"}}),
                )],
                hints: vec![],
            },
            Surface::Cli,
        )
    };
    let (r, out) = run(&mut c, revise_cmd(&f, Some(1)), false, "", false);
    assert!(r.unwrap_err().contains("unexpected shape"));
    assert!(!out.contains("rm -rf"));
    assert_eq!(
        platform_refusal(true, &revise_cmd(&f, None)),
        Some(WSL2_REFUSAL)
    );
}
