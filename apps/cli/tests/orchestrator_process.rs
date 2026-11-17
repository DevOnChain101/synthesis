//! The standalone orchestrator process (plan PR-12): a real `synt orchestrator --serve`, killed
//! and restarted; two owners racing for one outcome; recovery on adoption. Unix only: the Windows
//! named pipe is unverified.
#![cfg(unix)]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use serde_json::json;
use synthesis_app::orch_endpoint::{self as ep, LocalSocket};
use synthesis_app::orch_owner::{Owner, due_notices};
use synthesis_app::outcome_client::OutcomeClient;
use synthesis_config::Env;
use synthesis_core::WorkspaceContext;
use synthesis_core::outcome::*;
use synthesis_orchestrator::{AllocSpec, Allocator, Platform, SyntSpawn};
use synthesis_session::outcome::*;
use synthesis_session::{SCHEMA_VERSION_OUTCOMES, Store, StoreOptions};
use synthesis_workspace::{WorktreeManager, WorktreeOptions};
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

fn settings(home: &Path, enabled: bool) {
    std::fs::create_dir_all(home).unwrap();
    std::fs::write(
        home.join("settings.json"),
        json!({"synt": {"orchestrator": {"enabled": enabled}}}).to_string(),
    )
    .unwrap();
}

/// A short path (a unix socket path is limited to about 100 bytes).
fn tmp() -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix("o")
        .tempdir_in("/tmp")
        .unwrap()
}

fn registry_pid(home: &Path) -> u32 {
    let v: serde_json::Value = serde_json::from_slice(
        &std::fs::read(synthesis_peers::run_dir(home).join("orch.json")).unwrap(),
    )
    .unwrap();
    v["pid"].as_u64().unwrap() as u32
}

/// Kills the orchestrator a test started, whatever happens to the test.
struct Reap(PathBuf);
impl Drop for Reap {
    fn drop(&mut self) {
        if ep::read_registry(&self.0).is_some() {
            let _ = Command::new("kill")
                .args(["-9", &registry_pid(&self.0).to_string()])
                .status();
        }
    }
}

#[test]
fn a_disabled_install_does_not_start_the_process_and_publishes_nothing() {
    let t = tmp();
    let home = std::fs::canonicalize(t.path()).unwrap().join("h");
    settings(&home, false);
    let o = Command::new(SYNT)
        .args(["orchestrator", "--serve"])
        .env("SYNT_HOME", &home)
        .output()
        .unwrap();
    assert_eq!(o.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&o.stderr).contains("outcomes are off"));
    assert!(ep::read_registry(&home).is_none());
    // and a client does not start one either
    assert!(LocalSocket::connect_or_start(&home, Surface::Cli, Path::new(SYNT)).is_err());
}

#[test]
fn killing_the_orchestrator_loses_nothing_and_a_client_starts_another() {
    let t = tmp();
    let home = std::fs::canonicalize(t.path()).unwrap().join("h");
    settings(&home, true);
    let _reap = Reap(home.clone());
    // a client starts the process on demand, detached
    let s = LocalSocket::connect_or_start(&home, Surface::Cli, Path::new(SYNT)).unwrap();
    let mut c = OutcomeClient::new(s, Surface::Cli);
    assert_eq!(c.status().unwrap()["enabled"], true);
    c.enable().unwrap();
    let first = ep::read_registry(&home).unwrap();
    let pid = registry_pid(&home);
    // the process dies without a goodbye (a crash, a reboot)
    assert!(
        Command::new("kill")
            .args(["-9", &pid.to_string()])
            .status()
            .unwrap()
            .success()
    );
    let until = Instant::now() + Duration::from_secs(10);
    while LocalSocket::connect(&home, Surface::Cli).is_ok() {
        assert!(
            Instant::now() < until,
            "the killed orchestrator still answers"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    // the stored state is still there; another client starts a new process, which takes over
    let s = LocalSocket::connect_or_start(&home, Surface::Cli, Path::new(SYNT)).unwrap();
    let mut c = OutcomeClient::new(s, Surface::Cli);
    assert_eq!(
        c.status().unwrap()["schema"],
        5,
        "the migration survived the crash"
    );
    let second = ep::read_registry(&home).unwrap();
    assert_ne!(first, second, "a new endpoint was published");
    assert_ne!(registry_pid(&home), pid);
}

struct Rig {
    _t: tempfile::TempDir,
    base: PathBuf,
    repo: PathBuf,
    home: PathBuf,
    store: Store,
    wm: WorktreeManager,
    head: String,
}

fn rig() -> Rig {
    let t = tmp();
    let base = std::fs::canonicalize(t.path()).unwrap();
    let (repo, home) = (base.join("repo"), base.join("h"));
    std::fs::create_dir_all(&repo).unwrap();
    g(&repo, &["init", "-q"]);
    std::fs::write(repo.join("a.txt"), "one\n").unwrap();
    g(&repo, &["add", "."]);
    g(&repo, &["commit", "-qm", "init"]);
    let head = g(&repo, &["rev-parse", "HEAD"]);
    settings(&home, true);
    let store = Store::open(
        &home,
        StoreOptions {
            schema_target: SCHEMA_VERSION_OUTCOMES,
            ..StoreOptions::default()
        },
    )
    .unwrap();
    let mut o = WorktreeOptions::new(home.join("state"));
    o.worktrees_dir = Some(home.join("worktrees"));
    let wm = WorktreeManager::new(&Env::from_process(), o).unwrap();
    Rig {
        _t: t,
        base,
        repo,
        home,
        store,
        wm,
        head,
    }
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
            allowed_paths: vec!["**".into()],
            denied_paths: vec![],
            capabilities: caps,
            budgets: Budgets::new(5.0),
            quality_gates: vec![],
        },
        checks: vec![],
    }
}

/// A running outcome with one work item leased and an attempt `starting` that no worker runs
/// (what a dead owner leaves behind). The returned writer holds the control-session lock.
fn orphaned(r: &Rig) -> (OutcomeWriter, WorkItemId) {
    let ws = WorkspaceContext::open("w", &r.repo, &r.repo, &[], r.base.join("cart"), "pg").unwrap();
    let policy = OutcomePolicy::new(true, 16, 10, 24);
    let mut w = r
        .store
        .create_outcome(NewOutcome {
            workspace: &ws,
            base_commit: r.head.clone(),
            contract: &contract(),
            policy: &policy,
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
                key: "write".into(),
                kind: WorkItemKind::Agent,
                requires: vec![],
                capabilities: vec![],
                covers: vec![],
            },
            PlanItem {
                key: "check".into(),
                kind: WorkItemKind::Verify,
                requires: vec!["write".into()],
                capabilities: vec![],
                covers: vec!["c1".into()],
            },
        ],
    };
    let items = w.insert_plan(&plan).unwrap();
    go(&mut w, OutcomeState::Ready);
    go(&mut w, OutcomeState::Running);
    let it = items.iter().find(|(k, _)| k == "write").unwrap().1.clone();
    for to in [WorkItemState::Ready, WorkItemState::Leased] {
        let rev = w.get_work_item(&it).unwrap().revision;
        w.transition_work_item(&it, rev, to, &none).unwrap();
    }
    Allocator {
        store: &r.store,
        worktrees: &r.wm,
        platform: Platform::current(),
        artifact_root: r.base.join("art"),
    }
    .allocate(
        &mut w,
        &AllocSpec {
            item: &it,
            ordinal: 1,
            source: &r.repo,
            base_commit: &r.head,
            worker_kind: "local-engine",
        },
        OffsetDateTime::now_utc(),
    )
    .unwrap();
    (w, it)
}

#[test]
fn one_owner_per_outcome_and_a_new_owner_recovers_what_the_dead_one_left() {
    let r = rig();
    let (w, it) = orphaned(&r);
    let id = w.id().clone();
    // a person paused it, so the new owner must not start anything while it recovers
    r.store
        .enqueue_command(NewCommand {
            outcome_id: id.clone(),
            idempotency_key: "p".into(),
            kind: "outcome.pause".into(),
            params: json!({}),
            expected_revision: None,
            from_surface: Surface::Cli,
        })
        .unwrap();
    let spawn = SyntSpawn {
        log: r.base.join("w.log"),
        env: None,
    };
    let pol = OutcomePolicy::new(true, 16, 10, 24);
    let mut a = Owner::new(&r.home, &r.store, &r.wm, &spawn, PathBuf::from("sh"), None);
    // the first writer still holds the lock: the lock decides, nothing is touched
    a.cycle(&pol).unwrap();
    assert_eq!(a.owned_count(), 0);
    assert_eq!(
        r.store.reader().unwrap().list_attempts(&it).unwrap()[0].state,
        AttemptState::Starting
    );
    // the old owner dies (its lock is released with it)
    drop(w);
    a.cycle(&pol).unwrap();
    assert_eq!(a.owned_count(), 1);
    // a second process (here: a second owner) cannot take it while the first one holds it
    let mut b = Owner::new(&r.home, &r.store, &r.wm, &spawn, PathBuf::from("sh"), None);
    b.cycle(&pol).unwrap();
    assert_eq!(b.owned_count(), 0);
    // recovery ran once: the attempt nobody runs is lost, and the person is shown it
    let rd = r.store.reader().unwrap();
    assert_eq!(rd.list_attempts(&it).unwrap()[0].state, AttemptState::Lost);
    let o = rd.get_outcome(&id).unwrap();
    let recovered = rd
        .read_events(&o.control_session_id, 0, 5000)
        .unwrap()
        .into_iter()
        .any(
            |e| matches!(e, synthesis_events::Record::Unknown(x) if x.kind == "outcome.recovered"),
        );
    assert!(recovered, "outcome.recovered was recorded");
    // the pause was applied: still no second attempt
    assert_eq!(rd.list_attempts(&it).unwrap().len(), 1);
    assert_eq!(
        rd.get_command(&id, "p").unwrap().unwrap().state,
        CommandState::Applied
    );
    // letting go of the outcome frees the lock for the other owner
    a.release_all("test");
    b.cycle(&pol).unwrap();
    assert_eq!(b.owned_count(), 1);
}

#[test]
fn a_blocked_outcome_owes_one_id_only_notice_even_with_no_client_attached() {
    let r = rig();
    let (mut w, _) = orphaned(&r);
    let mut sent = std::collections::BTreeSet::new();
    let o = w.get_outcome(w.id()).unwrap();
    assert!(
        due_notices(&mut w, &o, &mut sent).is_empty(),
        "running needs nobody"
    );
    let rev = o.revision;
    w.transition_outcome(
        rev,
        OutcomeState::Blocked,
        Some("stuck"),
        &Effects::default(),
    )
    .unwrap();
    let o = w.get_outcome(w.id()).unwrap();
    let n = due_notices(&mut w, &o, &mut sent);
    assert_eq!(n.len(), 1);
    assert_eq!(n[0]["event"], "outcome.blocked");
    assert_eq!(n[0]["outcome_id"], o.id.as_str());
    // ids and codes: no goal, no reason text
    let text = n[0].to_string();
    assert!(!text.contains("goal") && !text.contains("stuck"), "{text}");
    assert!(due_notices(&mut w, &o, &mut sent).is_empty(), "told once");
}

#[test]
fn notices_are_told_once_across_a_restart_and_a_crash_after_the_mark_loses_one_never_two() {
    let r = rig();
    let (mut w, _) = orphaned(&r);
    let rev = w.get_outcome(w.id()).unwrap().revision;
    w.transition_outcome(
        rev,
        OutcomeState::Blocked,
        Some("stuck"),
        &Effects::default(),
    )
    .unwrap();
    let o = w.get_outcome(w.id()).unwrap();
    assert_eq!(due_notices(&mut w, &o, &mut Default::default()).len(), 1);
    // a restarted owner has an empty in-memory set: the outbox row still says it was told
    assert!(
        due_notices(&mut w, &o, &mut Default::default()).is_empty(),
        "told again after a restart"
    );
    let rows: Vec<_> = w
        .list_outbox(w.id())
        .unwrap()
        .into_iter()
        .filter(|r| r.kind == OutboxKind::Notify)
        .collect();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].state, OutboxState::Done);
    assert!(
        !rows[0].payload.to_string().contains("stuck"),
        "ids and codes only"
    );
    // a crash between the mark and the hook: the row is there, so the notice is not sent again
    // (a row left `pending` by an older or interrupted writer counts as told as well)
    let key = format!("notify:outcome.verified:{}:{}", o.id, o.revision + 1);
    w.atomically(|tx| {
        tx.enqueue(&NewOutbox {
            idempotency_key: key.clone(),
            kind: OutboxKind::Notify,
            payload: json!({"event": "outcome.verified"}),
        })
    })
    .unwrap();
    let mut o2 = o.clone();
    o2.state = OutcomeState::Verified;
    o2.revision += 1;
    assert!(due_notices(&mut w, &o2, &mut Default::default()).is_empty());
}
