//! The read-only budget, evidence `event_backed` and attempt diff reads (ADR-021 notes, threat
//! T-34, T-37): off by default, the same on the read-only engine service and the owner's, bounded,
//! and no secret in what is sent. The server is in this process (`InProc`/`Service`).

use std::collections::BTreeMap;
use std::path::Path;
use std::process::Command;

use rusqlite::Connection;
use serde_json::json;
use synthesis_app::outcome_client::{ClientError, InProc, OutcomeClient};
use synthesis_app::outcome_service::Service;
use synthesis_app::proto::Call;
use synthesis_core::WorkspaceContext;
use synthesis_core::outcome::*;
use synthesis_session::outcome::*;
use synthesis_session::{NewSession, SCHEMA_VERSION_OUTCOMES, Store, StoreOptions};

fn git(dir: &Path, args: &[&str]) -> String {
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
            "commit.gpgsign=false",
        ])
        .args(args)
        .output()
        .unwrap();
    assert!(o.status.success(), "git {args:?}");
    String::from_utf8_lossy(&o.stdout).trim().to_string()
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

struct Rig {
    _tmp: tempfile::TempDir,
    home: std::path::PathBuf,
    outcome: String,
    attempt: String,
    commit: String,
}

/// An outcome over a real repository with one finished attempt whose commit changed three files.
fn rig() -> Rig {
    let tmp = tempfile::tempdir().unwrap();
    let base_dir = synthesis_core::canonicalize(tmp.path()).unwrap();
    let repo = base_dir.join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    git(&repo, &["init", "-q"]);
    std::fs::write(repo.join("a.txt"), "one\n").unwrap();
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-qm", "base"]);
    let base = git(&repo, &["rev-parse", "HEAD"]);
    std::fs::write(repo.join("a.txt"), "two\n").unwrap();
    std::fs::write(repo.join(".env"), "TOKEN=hunter2\n").unwrap();
    std::fs::write(repo.join("pic.bin"), b"\0\x01\x02").unwrap();
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-qm", "work"]);
    let commit = git(&repo, &["rev-parse", "HEAD"]);

    let home = base_dir.join("home");
    let store = Store::open(
        &home,
        StoreOptions {
            schema_target: SCHEMA_VERSION_OUTCOMES,
            ..StoreOptions::default()
        },
    )
    .unwrap();
    let ws = WorkspaceContext::open("w", &repo, &repo, &[], base_dir.join("cart"), "pg").unwrap();
    let mut w = store
        .create_outcome(NewOutcome {
            workspace: &ws,
            base_commit: base,
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
                id: Some("att-1".into()),
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
    let (a, _) = w
        .new_attempt(&item, 1, "att-1", "local-inproc", &none)
        .unwrap();
    to(&mut w, WorkItemState::Running);
    let r = w
        .transition_attempt(
            &a.id,
            a.revision,
            AttemptState::Running,
            &AttemptEnd::default(),
            &none,
        )
        .unwrap()
        .revision;
    w.transition_attempt(
        &a.id,
        r,
        AttemptState::Finished,
        &AttemptEnd {
            task_status: Some(synthesis_core::TaskStatus::Completed),
            subject_commit: Some(commit.clone()),
            retained_reason: None,
        },
        &none,
    )
    .unwrap();
    w.record_evidence(&NewEvidence {
        work_item_id: item.clone(),
        attempt_id: Some(a.id.clone()),
        trust_class: TrustClass::IndependentCheck,
        producer: "independent_runner".into(),
        subject_commit: commit.clone(),
        check_id: "unit".into(),
        command_hash: None,
        criteria_ids: vec!["c1".into()],
        result: "passed".into(),
        exit_code: Some(0),
        env_fingerprint: "fp".into(),
        artifact: None,
        fence: a.fence,
        flags: vec![],
    })
    .unwrap();
    let outcome = w.id().to_string();
    drop(w);
    // what the attempt's session recorded, and a reservation for it
    let db = Connection::open(home.join("synt.db")).unwrap();
    db.execute(
        "UPDATE sessions SET usage=?1, status='completed' WHERE id='att-1'",
        [json!({"turns": 3, "tool_calls": 4, "cost_usd": 0.25, "wall_ms": 12500, "verify_attempts": 0}).to_string()],
    )
    .unwrap();
    db.execute("UPDATE attempts SET reserved_usd=2.0", [])
        .unwrap();
    Rig {
        _tmp: tmp,
        home,
        outcome,
        attempt: a.id.to_string(),
        commit,
    }
}

fn client(r: &Rig, svc: fn(&Path, bool) -> Service) -> OutcomeClient<InProc> {
    OutcomeClient::new(InProc(svc(&r.home, true)), Surface::Cli)
}

#[test]
fn off_by_default_the_new_reads_are_refused_and_nothing_is_opened() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    for svc in [Service::new(&home, false), Service::read_only(&home, false)] {
        for call in [
            Call::OutcomeBudget { id: "o".into() },
            Call::AttemptDiff {
                outcome: "o".into(),
                id: "a".into(),
            },
        ] {
            assert!(svc.handle(call).unwrap_err().contains("off"));
        }
    }
    assert!(!home.exists());
}

#[test]
fn budget_reports_what_the_store_has_and_reservations_apart() {
    let r = rig();
    for svc in [
        Service::new as fn(&Path, bool) -> Service,
        Service::read_only,
    ] {
        let b = client(&r, svc).budget(&r.outcome).unwrap();
        assert_eq!(b["cost_usd"], json!({"spent": 0.25, "limit": 5.0}));
        assert_eq!(b["wall_secs"], json!({"spent": 12.5}));
        // the attempt has a final usage, so its reservation is no longer outstanding
        assert_eq!(b["reserved_usd"], 0.0);
        assert!(b.get("tokens").is_none() && b["cost_usd"].get("remaining").is_none());
        let view = synthesis_view::outcomes::BudgetView::from_json(&b);
        assert_eq!(view.cost_usd.spent, Some(0.25));
        assert_eq!(view.cost_usd.remaining, None);
    }
    // an unfinished usage keeps the reservation outstanding and reported apart from the spend
    Connection::open(r.home.join("synt.db"))
        .unwrap()
        .execute(
            "UPDATE sessions SET usage='{}', status='running' WHERE id='att-1'",
            [],
        )
        .unwrap();
    let b = client(&r, Service::read_only).budget(&r.outcome).unwrap();
    assert_eq!(
        (b["cost_usd"]["spent"].clone(), b["reserved_usd"].clone()),
        (json!(0.0), json!(2.0))
    );
    assert!(client(&r, Service::read_only).budget("not-an-id").is_err());
}

#[test]
fn evidence_says_whether_the_store_found_its_chained_event() {
    let r = rig();
    let rows = client(&r, Service::read_only).evidence(&r.outcome).unwrap();
    assert_eq!(rows[0]["event_backed"], true);
    let v = synthesis_view::outcomes::EvidenceView::from_json(&rows[0]).unwrap();
    assert_eq!(v.event_backed, Some(true));
    // the event is gone (a writer outside the owner): the row is still listed, and says so
    let db = Connection::open(r.home.join("synt.db")).unwrap();
    db.execute("DELETE FROM events WHERE type='evidence.recorded'", [])
        .unwrap();
    let rows = client(&r, Service::read_only).evidence(&r.outcome).unwrap();
    assert_eq!(rows[0]["event_backed"], false);
}

#[test]
fn the_diff_is_the_attempts_commit_against_the_base_and_withholds_secrets() {
    let r = rig();
    for svc in [
        Service::new as fn(&Path, bool) -> Service,
        Service::read_only,
    ] {
        let d = client(&r, svc).diff(&r.outcome, &r.attempt).unwrap();
        assert_eq!(d["commit"], r.commit.as_str());
        assert_eq!(
            (d["total_files"].clone(), d["truncated"].clone()),
            (json!(3), json!(false))
        );
        let f = |p: &str| {
            d["files"]
                .as_array()
                .unwrap()
                .iter()
                .find(|f| f["path"] == p)
                .unwrap()
                .clone()
        };
        assert!(f("a.txt")["patch"].as_str().unwrap().contains("+two"));
        assert_eq!(f("pic.bin")["binary"], true);
        assert!(f("pic.bin")["patch"].is_null());
        assert_eq!(f(".env")["secret"], true);
        assert!(f(".env")["patch"].is_null());
        assert!(
            !d.to_string().contains("hunter2"),
            "a secret reached the wire"
        );
        // the view accepts the wire shape as sent
        let tab = synthesis_view::outcomes::InspectorContent::diff_from(&d).unwrap();
        assert!(tab.render_text().contains("+two") && !tab.render_text().contains("hunter2"));
    }
}

#[test]
fn the_diff_refuses_an_unknown_attempt_and_a_bad_id() {
    let r = rig();
    let mut c = client(&r, Service::read_only);
    assert!(
        matches!(c.diff(&r.outcome, "at_nope"), Err(ClientError::Refused(e)) if e.contains("no attempt"))
    );
    assert!(c.diff("nope", &r.attempt).is_err());
}
