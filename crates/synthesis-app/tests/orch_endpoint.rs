//! The standalone orchestrator process and its endpoint (plan PR-12): who may write, single owner,
//! create/enable/lifecycle through the owner. The process runs in a thread of the test (`serve_with`),
//! the clients talk to it over the real socket. Unix only: the Windows named pipe is unverified.
#![cfg(unix)]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use serde_json::json;
use synthesis_app::orch_endpoint::{self as ep, LocalSocket};
use synthesis_app::orch_owner::serve_with;
use synthesis_app::outcome_client::{ClientError, OutcomeClient, Transport};
use synthesis_app::outcome_service::Service;
use synthesis_app::proto::{Call, CommandArgs, OutcomeCommand};
use synthesis_config::Env;
use synthesis_core::outcome::*;
use synthesis_peers::net::{connect, handshake_client};
use synthesis_peers::{Key, run_dir};
use synthesis_session::outcome::{CommandState, Surface};

/// A short path: a unix socket path is limited to about 100 bytes.
fn short_tmp() -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix("o")
        .tempdir_in("/tmp")
        .unwrap()
}

struct Proc {
    _tmp: tempfile::TempDir,
    home: PathBuf,
    stop: Arc<AtomicBool>,
    th: Option<std::thread::JoinHandle<Result<(), String>>>,
}

fn env_of(home: &Path) -> Env {
    Env::from_pairs([
        ("SYNT_HOME", home.to_string_lossy().to_string()),
        ("HOME", home.to_string_lossy().to_string()),
        ("PATH", std::env::var("PATH").unwrap_or_default()),
    ])
}

fn settings(home: &Path, enabled: bool) {
    std::fs::create_dir_all(home).unwrap();
    std::fs::write(
        home.join("settings.json"),
        json!({"synt": {"orchestrator": {"enabled": enabled}}}).to_string(),
    )
    .unwrap();
}

fn start() -> Proc {
    let tmp = short_tmp();
    let home = std::fs::canonicalize(tmp.path()).unwrap().join("h");
    settings(&home, true);
    let stop = Arc::new(AtomicBool::new(false));
    let (e, s) = (env_of(&home), stop.clone());
    // workers (never started in these tests) would run `sh`, not the test binary
    let th = std::thread::spawn(move || serve_with(e, s, Some("sh".into())));
    let until = Instant::now() + Duration::from_secs(20);
    while ep::read_registry(&home).is_none() {
        assert!(
            Instant::now() < until,
            "the orchestrator did not publish an endpoint"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    Proc {
        _tmp: tmp,
        home,
        stop,
        th: Some(th),
    }
}

impl Proc {
    fn client(&self, surface: Surface) -> OutcomeClient<LocalSocket> {
        OutcomeClient::new(LocalSocket::connect(&self.home, surface).unwrap(), surface)
    }
}

impl Drop for Proc {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(t) = self.th.take() {
            let _ = t.join();
        }
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
            allowed_paths: vec!["src/**".into()],
            denied_paths: vec![],
            capabilities: caps,
            budgets: Budgets::new(5.0),
            quality_gates: vec![],
        },
        checks: vec![],
    }
}

fn wait_applied(c: &mut OutcomeClient<LocalSocket>, outcome: &str, key: &str) {
    let until = Instant::now() + Duration::from_secs(20);
    loop {
        let r = c.command_status(outcome, key).unwrap();
        if r.state != CommandState::Queued {
            assert_eq!(r.state, CommandState::Applied, "{r:?}");
            return;
        }
        assert!(Instant::now() < until, "the owner never applied {key}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn err_text(r: Result<impl std::fmt::Debug, ClientError>) -> String {
    r.expect_err("must be refused").to_string()
}

#[test]
fn a_client_that_does_not_hold_the_key_gets_nothing() {
    let p = start();
    let endpoint = ep::read_registry(&p.home).unwrap();
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let wrong = Key::from_bytes_for_test([7; 32]);
    let res = rt.block_on(async {
        let mut c = connect(&endpoint).await?;
        handshake_client(&mut c, &wrong).await
    });
    assert!(res.is_err(), "a wrong key must fail the handshake");
}

#[test]
fn a_key_holder_that_does_not_attach_as_a_surface_is_dropped() {
    let p = start();
    let endpoint = ep::read_registry(&p.home).unwrap();
    let key = ep::load_key(&p.home).unwrap();
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let got = rt.block_on(async {
        let mut c = connect(&endpoint).await.unwrap();
        handshake_client(&mut c, &key).await.unwrap();
        // a request instead of the attach frame
        c.send(&json!({"id": 1, "method": "outcome.list", "params": {}}))
            .await
            .unwrap();
        c.recv().await
    });
    assert!(matches!(got, Ok(None) | Err(_)), "{got:?}");
}

#[test]
fn a_mutating_call_must_name_the_surface_the_link_attached_as() {
    let p = start();
    // attached as the CLI, claiming to be the TUI
    let mut c = OutcomeClient::new(
        LocalSocket::connect(&p.home, Surface::Cli).unwrap(),
        Surface::Tui,
    );
    let e = err_text(c.command("cancel", "11111111-1111-4111-8111-111111111111", None, "k1"));
    assert!(e.contains("another surface"), "{e}");
    let e = err_text(c.approve_contract("11111111-1111-4111-8111-111111111111", 1, "h", "k2"));
    assert!(e.contains("another surface"), "{e}");
}

#[test]
fn the_endpoint_serves_outcome_methods_only() {
    let p = start();
    let mut t = LocalSocket::connect(&p.home, Surface::Cli).unwrap();
    let e = t.call(Call::Close).expect_err("not an outcome method");
    assert!(e.to_string().contains("outcome methods only"), "{e}");
}

#[test]
fn the_stdio_engine_cannot_queue_anything() {
    let tmp = short_tmp();
    let home = tmp.path().join("h");
    settings(&home, true);
    let svc = Service::read_only(&home, true);
    let cmd = CommandArgs {
        idempotency_key: "k".into(),
        surface: Surface::Cli,
    };
    let id = "11111111-1111-4111-8111-111111111111";
    for call in [
        Call::DecisionResolve {
            outcome: id.into(),
            id: id.into(),
            expected_revision: 1,
            choice: "x".into(),
            brief_sha256: "0".repeat(64),
            note: None,
            cmd: cmd.clone(),
        },
        Call::OutcomeContractApprove {
            id: id.into(),
            version: 1,
            hash: "h".into(),
            cmd: cmd.clone(),
        },
        Call::OutcomeStart(OutcomeCommand {
            id: id.into(),
            expected_revision: None,
            cmd: cmd.clone(),
        }),
        Call::OrchestratorEnable {
            surface: Surface::Cli,
        },
    ] {
        let e = svc.handle(call).expect_err("read-only");
        assert!(e.contains("orchestrator"), "{e}");
    }
}

#[test]
fn it_serves_status_and_refuses_to_start_when_outcomes_are_off() {
    let p = start();
    let st = p.client(Surface::Cli).status().unwrap();
    assert_eq!(st["enabled"], true);
    let off = short_tmp();
    let home = off.path().join("h");
    settings(&home, false);
    let e = serve_with(env_of(&home), Arc::new(AtomicBool::new(false)), None).unwrap_err();
    assert!(e.contains("outcomes are off"), "{e}");
    assert!(ep::read_registry(&home).is_none(), "nothing was published");
}

#[test]
fn a_second_orchestrator_for_the_same_home_does_not_start() {
    let p = start();
    let before = ep::read_registry(&p.home).unwrap();
    // the lock decides: the second returns at once and leaves the first one's endpoint alone
    serve_with(env_of(&p.home), Arc::new(AtomicBool::new(false)), None).unwrap();
    assert_eq!(ep::read_registry(&p.home).unwrap(), before);
    assert!(p.client(Surface::Cli).status().is_ok());
}

#[test]
fn create_approve_and_cancel_go_through_the_owner() {
    let p = start();
    let mut c = p.client(Surface::Cli);
    // not migrated yet: nothing can be created
    let repo = p._tmp.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    let repo = std::fs::canonicalize(repo).unwrap();
    let base = "a".repeat(40);
    let repo_s = repo.to_string_lossy().to_string();
    assert!(c.create(&repo_s, &base, contract()).is_err());
    // refused inputs never reach the store
    let e = err_text(c.create("relative/dir", &base, contract()));
    assert!(e.contains("absolute"), "{e}");
    let e = err_text(c.create(&repo_s, "HEAD", contract()));
    assert!(e.contains("commit id"), "{e}");
    // the explicit migration, then the creation
    assert_eq!(c.enable().unwrap()["schema"], 5);
    let made = c.create(&repo_s, &base, contract()).unwrap();
    let id = made["id"].as_str().unwrap().to_string();
    let hash = made["contract_hash"].as_str().unwrap().to_string();
    assert_eq!(c.outcome(&id).unwrap().state, "contract_pending");
    // approve: queued by the surface, applied by the owner that adopts the outcome
    let r = c.approve_contract(&id, 1, &hash, "ap1").unwrap();
    assert_eq!(r.kind, "outcome.contract.approve");
    wait_applied(&mut c, &id, "ap1");
    // cancel before it runs
    c.command("cancel", &id, None, "cx1").unwrap();
    wait_applied(&mut c, &id, "cx1");
    assert_eq!(c.outcome(&id).unwrap().state, "cancelled");
}

#[test]
fn the_endpoint_and_key_are_under_the_sandbox_deny_list() {
    let home = Path::new("/h");
    let denied = synthesis_sandbox::synthesis_state_paths(home);
    for f in [ep::KEY_FILE, ep::REGISTRY_FILE, ep::LOCK_FILE] {
        let p = run_dir(home).join(f);
        assert!(
            denied.iter().any(|d| p.starts_with(d)),
            "{f} is not under a denied path"
        );
    }
}
