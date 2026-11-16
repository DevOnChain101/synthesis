//! `synt outcome ...` through the real binary and the real orchestrator process it starts (plan
//! PR-13). Dispatch, rendering and exit codes only; the lifecycle itself is proven by the protocol
//! tests. This file runs in every build, where the owner process has no model, so the work an
//! attempt does is not driven here: a decision is seeded in the store the way a blocked attempt
//! leaves it, and everything a person does with it goes through the CLI. The same flow with a
//! real worker and a decision it produced is `outcome_e2e.rs` (feature `test-provider`). Unix
//! only (the Windows named pipe is unverified).
#![cfg(unix)]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::{Value, json};
use synthesis_core::WorkspaceContext;
use synthesis_core::outcome::*;
use synthesis_orchestrator::{compile_envelope, decision_binding};
use synthesis_session::outcome::*;
use synthesis_session::{SCHEMA_VERSION_OUTCOMES, Store, StoreOptions};
use time::OffsetDateTime;

const SYNT: &str = env!("CARGO_BIN_EXE_synt");

fn g(dir: &Path, args: &[&str]) -> String {
    let o = Command::new("git")
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap())
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .current_dir(dir)
        .args([
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "-c",
            "init.defaultBranch=main",
        ])
        .args(args)
        .output()
        .unwrap();
    assert!(o.status.success(), "git {args:?}");
    String::from_utf8_lossy(&o.stdout).trim().to_string()
}

struct Rig {
    _t: tempfile::TempDir,
    base: PathBuf,
    home: PathBuf,
    repo: PathBuf,
    head: String,
}

/// Stops the orchestrator a command started, whatever happens to the test.
impl Drop for Rig {
    fn drop(&mut self) {
        let reg = std::fs::read(self.home.join("run/orch.json")).ok();
        let pid = reg
            .and_then(|v| serde_json::from_slice::<Value>(&v).ok())
            .and_then(|v| v["pid"].as_u64());
        if let Some(pid) = pid {
            let _ = Command::new("kill").args(["-9", &pid.to_string()]).status();
        }
    }
}

fn rig(enabled: bool) -> Rig {
    // a short path: a unix socket path is limited to about 100 bytes
    let t = tempfile::Builder::new()
        .prefix("c")
        .tempdir_in("/tmp")
        .unwrap();
    let base = std::fs::canonicalize(t.path()).unwrap();
    let (home, repo) = (base.join("h"), base.join("repo"));
    std::fs::create_dir_all(&home).unwrap();
    std::fs::write(
        home.join("settings.json"),
        json!({"synt": {"orchestrator": {"enabled": enabled}}}).to_string(),
    )
    .unwrap();
    std::fs::create_dir_all(&repo).unwrap();
    g(&repo, &["init", "-q"]);
    std::fs::write(repo.join("a.txt"), "one\n").unwrap();
    g(&repo, &["add", "."]);
    g(&repo, &["commit", "-qm", "init"]);
    let head = g(&repo, &["rev-parse", "HEAD"]);
    Rig {
        _t: t,
        base,
        home,
        repo,
        head,
    }
}

impl Rig {
    /// `(exit code, stdout, stderr)`; the environment is the home and nothing else.
    fn synt(&self, args: &[&str]) -> (i32, String, String) {
        let o = Command::new(SYNT)
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap())
            .env("SYNT_HOME", &self.home)
            .env("HOME", &self.base)
            .arg("outcome")
            .args(args)
            .output()
            .unwrap();
        (
            o.status.code().unwrap(),
            String::from_utf8_lossy(&o.stdout).into_owned(),
            String::from_utf8_lossy(&o.stderr).into_owned(),
        )
    }

    fn json(&self, args: &[&str]) -> Value {
        let mut a = args.to_vec();
        a.push("--json");
        let (code, out, err) = self.synt(&a);
        assert_eq!(code, 0, "{args:?}: {err}");
        serde_json::from_str(out.trim()).unwrap_or_else(|e| panic!("{args:?}: {e}: {out}"))
    }

    fn file(&self, name: &str, v: Value) -> String {
        let p = self.base.join(name);
        std::fs::write(&p, v.to_string()).unwrap();
        p.to_string_lossy().into_owned()
    }
}

#[test]
fn outcomes_off_is_a_polite_refusal_that_starts_and_enables_nothing() {
    let r = rig(false);
    for args in [&["list"][..], &["status", "x"], &["enable", "--yes"]] {
        let (code, out, err) = r.synt(args);
        assert_eq!(code, 2, "{args:?}");
        assert!(out.is_empty(), "{out}");
        assert!(
            err.contains("outcomes are off") && err.contains("synt.orchestrator.enabled"),
            "{err}"
        );
        assert!(err.contains("never turns it on by itself"), "{err}");
    }
    assert!(
        !r.home.join("run/orch.json").exists(),
        "no orchestrator was started"
    );
    let s = std::fs::read_to_string(r.home.join("settings.json")).unwrap();
    assert!(s.contains("false"), "the setting was not touched: {s}");
}

#[test]
fn the_cli_prints_the_same_wsl2_refusal_as_the_orchestrator() {
    use synthesis_orchestrator::AllocError;
    assert_eq!(
        AllocError::NativeWindows.to_string(),
        synthesis_view::outcomes::WSL2_REFUSAL
    );
}

#[test]
fn create_approve_start_status_cancel_through_the_owner_process() {
    let r = rig(true);
    // enable needs the notice read first
    let (code, out, err) = r.synt(&["enable"]);
    assert_eq!(code, 2, "{err}");
    assert!(
        out.contains("schema v5") && err.contains("--yes"),
        "{out} {err}"
    );
    assert_eq!(r.synt(&["enable", "--yes"]).0, 0);
    // the contract: criteria, a frozen check, and an envelope that touches src only
    let criteria = r.file(
        "criteria.json",
        json!([{"id": "c1", "text": "it works", "check_ids": ["t"]}]),
    );
    let checks = r.file("checks.json", json!([{"id": "t", "command": "true"}]));
    let envelope = r.file(
        "envelope.json",
        json!({"allowed_paths": ["src/**"], "capabilities": {"repo.read": "allow", "repo.write": "allow"},
               "budgets": {"max_cost_usd": 1.0}}),
    );
    let repo = r.repo.to_string_lossy().into_owned();
    let made = r.json(&[
        "create",
        "--goal",
        "a small endpoint",
        "--criteria-file",
        &criteria,
        "--checks-file",
        &checks,
        "--envelope-file",
        &envelope,
        "--base-commit",
        &r.head,
        "--cwd",
        &repo,
    ]);
    assert_eq!(made["schema"], 1);
    let (id, hash) = (
        made["id"].as_str().unwrap().to_string(),
        made["contract_hash"].as_str().unwrap().to_string(),
    );
    // show the contract before approving it
    let (code, out, _) = r.synt(&["contract", &id]);
    assert_eq!(code, 0);
    assert!(
        out.contains("a small endpoint") && out.contains("not approved") && out.contains(&hash),
        "{out}"
    );
    // a hash that is not the one shown does not approve
    let (code, out, err) = r.synt(&["approve", &id, "--version", "1", "--hash", &"0".repeat(64)]);
    assert_ne!(code, 0, "{out} {err}");
    let (code, out, err) = r.synt(&["approve", &id, "--version", "1", "--hash", &hash]);
    assert!(
        code == 0 && out.starts_with("outcome.contract.approve: done"),
        "{out} {err}"
    );
    let (code, out, _) = r.synt(&["status", &id]);
    assert_eq!(code, 0);
    assert!(
        out.starts_with("Needs you:") && out.contains("(approved)"),
        "{out}"
    );
    // start: the server plans and applies; the state is whatever the server says
    let (code, out, err) = r.synt(&["start", &id]);
    assert!(
        code == 0 && out.starts_with("outcome.start: done"),
        "{out} {err}"
    );
    let s = r.json(&["status", &id]);
    assert_ne!(s["outcome"]["state"], "contract_pending", "{s}");
    assert!(
        s["work_items"].as_array().is_some_and(|a| a.len() == 3),
        "inspect, implement, test: {s}"
    );
    let (code, out, _) = r.synt(&["list"]);
    assert!(
        code == 0 && out.contains(&id) && out.contains("a small endpoint"),
        "{out}"
    );
    let (code, out, _) = r.synt(&["inspect", &id, "--tab", "workers"]);
    assert!(code == 0 && out.contains("[Workers]"), "{out}");
    let ev = r.json(&["events", &id]);
    assert!(
        ev["events"].as_array().is_some_and(|a| !a.is_empty()),
        "{ev}"
    );
    // cancel is applied by the owner too
    let (code, out, err) = r.synt(&["cancel", &id]);
    assert!(
        code == 0 && out.starts_with("outcome.cancel: done"),
        "{out} {err}"
    );
    assert_eq!(r.json(&["status", &id])["outcome"]["state"], "cancelled");
}

fn seeded(r: &Rig) -> (String, String) {
    let store = Store::open(
        &r.home,
        StoreOptions {
            schema_target: SCHEMA_VERSION_OUTCOMES,
            ..StoreOptions::default()
        },
    )
    .unwrap();
    let ws = WorkspaceContext::open("w", &r.repo, &r.repo, &[], r.base.join("cart"), "pg").unwrap();
    let mut caps = BTreeMap::new();
    caps.insert(Capability::RepoRead, CapabilityMode::Allow);
    let contract = Contract {
        version: 1,
        goal: "decide".into(),
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
    };
    let mut w = store
        .create_outcome(NewOutcome {
            workspace: &ws,
            base_commit: r.head.clone(),
            contract: &contract,
            policy: &OutcomePolicy::new(true, 16, 10, 24),
        })
        .unwrap();
    let none = Effects::default();
    let go = |w: &mut OutcomeWriter, to| {
        let rev = w.get_outcome(w.id()).unwrap().revision;
        w.transition_outcome(rev, to, None, &none).unwrap();
    };
    go(&mut w, OutcomeState::ContractPending);
    let h = w.get_contract(w.id(), 1).unwrap().1;
    w.approve_contract(1, &h, "user").unwrap();
    let plan = Plan {
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
    };
    let items = w.insert_plan(&plan).unwrap();
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
    let o = w.get_outcome(w.id()).unwrap();
    let policy = compile_envelope(&contract.envelope, &[])
        .unwrap()
        .policy_hash();
    let binding = decision_binding(1, o.plan_version.unwrap(), None, &policy, "", 0, &item);
    let fmt = time::macros::format_description!(
        "[year]-[month]-[day]T[hour]:[minute]:[second].[subsecond digits:3]Z"
    );
    let (d, _) = w
        .new_decision(&NewDecision {
            kind: "ambiguity".into(),
            dedupe_key: "k".into(),
            brief: json!({"why": "the worker needs an answer", "delta_from_contract": "Which API shape?",
                "alternatives": [{"id": "REST", "label": "REST"}, {"id": "RPC", "label": "RPC"}],
                "unknowns": ["is the client typed?"], "safe_default": "HOLD"}),
            binding,
            expires_at: (OffsetDateTime::now_utc() + time::Duration::hours(3)).format(&fmt).unwrap(),
            action: None,
        })
        .unwrap();
    w.transition_decision(&d.id, 1, DecisionState::Pending, &d.binding, &none)
        .unwrap();
    // the writer (and its control-session lock) goes away: the owner adopts the outcome
    (w.id().to_string(), d.id.to_string())
}

#[test]
fn a_decision_is_shown_whole_and_answered_only_with_an_explicit_choice_through_the_owner() {
    let r = rig(true);
    assert_eq!(r.synt(&["enable", "--yes"]).0, 0);
    let (oid, did) = seeded(&r);
    let (code, out, _) = r.synt(&["list"]);
    assert!(code == 0 && out.contains("1 decision(s) pending"), "{out}");
    // the card: server order, state, revision and hash to quote, answers without a default
    let (code, out, _) = r.synt(&["decisions", &oid]);
    assert_eq!(code, 0);
    for want in [
        "pending",
        "the worker needs an answer",
        "Which API shape?",
        "is the client typed?",
        "HOLD",
        "1. REST: REST",
        "2. RPC: RPC",
        "3. reject: Reject",
        "none is chosen for you",
    ] {
        assert!(out.contains(want), "{want} in {out}");
    }
    let card = &r.json(&["decisions", &oid])["decisions"][0];
    let (rev, hash) = (
        card["revision"].as_i64().unwrap().to_string(),
        card["brief_sha256"].as_str().unwrap().to_string(),
    );
    // no choice: nothing is sent, not even on a terminal-less default
    let (code, _, err) = r.synt(&["resolve", &oid, &did]);
    assert_eq!(code, 2);
    assert!(err.contains("no answer was chosen for you"), "{err}");
    // a choice that the card does not offer, and a stale revision
    assert_eq!(
        r.synt(&[
            "resolve",
            &oid,
            &did,
            "--choice",
            "MAYBE",
            "--revision",
            &rev,
            "--brief-hash",
            &hash
        ])
        .0,
        2
    );
    assert_eq!(
        r.synt(&[
            "resolve",
            &oid,
            &did,
            "--choice",
            "REST",
            "--revision",
            "99",
            "--brief-hash",
            &hash
        ])
        .0,
        2
    );
    assert_eq!(
        r.json(&["decisions", &oid])["decisions"][0]["state"],
        "pending"
    );
    // the explicit answer, with what the person was shown
    let (code, out, err) = r.synt(&[
        "resolve",
        &oid,
        &did,
        "--choice",
        "REST",
        "--revision",
        &rev,
        "--brief-hash",
        &hash,
    ]);
    assert!(
        code == 0 && out.starts_with("decision.resolve: done"),
        "{out} {err}"
    );
    let after = &r.json(&["decisions", &oid])["decisions"][0];
    assert_ne!(after["state"], "pending", "{after}");
    // a second answer to the same card is not applied: exit 1, never a silent success
    let (code, out, err) = r.synt(&[
        "resolve",
        &oid,
        &did,
        "--choice",
        "RPC",
        "--revision",
        &rev,
        "--brief-hash",
        &hash,
    ]);
    assert_ne!(code, 0, "{out} {err}");
}
