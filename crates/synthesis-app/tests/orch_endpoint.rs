//! The standalone orchestrator process and its endpoint (plan PR-12): who may write, single owner,
//! create/enable/lifecycle through the owner. The process runs in a thread of the test (`serve_with`),
//! the clients talk to it over the real socket. Unix only: the Windows named pipe is unverified.
#![cfg(unix)]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use synthesis_app::orch_endpoint::{self as ep, LocalSocket};
use synthesis_app::orch_owner::serve_with;
use synthesis_app::outcome_client::{ClientError, OutcomeClient, Transport};
use synthesis_app::outcome_service::Service;
use synthesis_app::proto::{Call, CommandArgs, DecisionHint, OutcomeCommand};
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

/// A repository with one commit; returns its root and the commit id.
fn git_repo(at: &Path) -> (PathBuf, String) {
    std::fs::create_dir_all(at).unwrap();
    let at = std::fs::canonicalize(at).unwrap();
    let g = |args: &[&str]| {
        let o = std::process::Command::new("git")
            .args([
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "-c",
                "commit.gpgsign=false",
            ])
            .args(args)
            .current_dir(&at)
            .output()
            .unwrap();
        assert!(o.status.success(), "{args:?}");
        String::from_utf8(o.stdout).unwrap().trim().to_string()
    };
    g(&["init", "-q"]);
    std::fs::write(at.join("a.txt"), "one\n").unwrap();
    g(&["add", "."]);
    g(&["commit", "-qm", "init"]);
    let head = g(&["rev-parse", "HEAD"]);
    (at, head)
}

#[test]
fn create_approve_and_cancel_go_through_the_owner() {
    let p = start();
    let mut c = p.client(Surface::Cli);
    // not migrated yet: nothing can be created
    let (repo, base) = git_repo(&p._tmp.path().join("repo"));
    let repo_s = repo.to_string_lossy().to_string();
    assert!(c.create(&repo_s, &base, contract()).is_err());
    // refused inputs never reach the store
    let e = err_text(c.create("relative/dir", &base, contract()));
    assert!(e.contains("absolute"), "{e}");
    let e = err_text(c.create(&repo_s, "HEAD", contract()));
    assert!(e.contains("commit id"), "{e}");
    // the explicit migration, then the creation
    let en = c.enable().unwrap();
    assert_eq!(en["schema"], 5);
    assert_eq!(
        en["disclosure"]["text"],
        synthesis_app::outcome_policy::INTEGRITY_DISCLOSURE
    );
    // a well-formed id that is no commit of this repository
    let e = err_text(c.create(&repo_s, &"a".repeat(40), contract()));
    assert!(e.contains("not in the repository"), "{e}");
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

/// A read-only goal needs no git: any folder, an empty base commit; and the kinds never mix (a
/// write goal cannot be created without its commit, nor a read-only one with one).
#[test]
fn ro_goal_is_created_in_a_folder_that_is_not_a_git_repository_and_the_kinds_never_mix() {
    let p = start();
    let mut c = p.client(Surface::Cli);
    let plain = p._tmp.path().join("plain");
    std::fs::create_dir_all(&plain).unwrap();
    let plain_s = plain.to_string_lossy().to_string();
    c.enable().unwrap();
    let mut features = vec!["read_only_goal", "report_review"];
    if !cfg!(windows) {
        features.push("attempt_message");
    }
    assert_eq!(c.status().unwrap()["features"], serde_json::json!(features));
    let ro = Contract::read_only(
        "review it".into(),
        &["names the risks".into()],
        Budgets::new(2.0),
    );
    let made = c.create(&plain_s, "", ro.clone()).unwrap();
    let id = made["id"].as_str().unwrap().to_string();
    assert_eq!(c.outcome(&id).unwrap().state, "contract_pending");
    let stored = c.contract(&id, None).unwrap();
    assert_eq!(stored["contract"]["kind"], "read_only", "{stored}");
    // a read-only goal has no commit, and a write goal without one is not made
    let e = err_text(c.create(&plain_s, &"a".repeat(40), ro));
    assert!(e.contains("no base commit"), "{e}");
    let e = err_text(c.create(&plain_s, "", contract()));
    assert!(e.contains("commit id"), "{e}");
    assert_eq!(c.outcomes(None).unwrap().len(), 1);
}

#[test]
fn a_create_retried_with_the_same_key_returns_the_same_draft() {
    let p = start();
    let mut c = p.client(Surface::Cli);
    let (repo, base) = git_repo(&p._tmp.path().join("repo"));
    let repo_s = repo.to_string_lossy().to_string();
    c.enable().unwrap();
    let first = c
        .create_with_key(&repo_s, &base, contract(), "retry-key-1")
        .unwrap();
    // the reply was lost: the retry (here on a new link) gets the same draft, not a second one
    let mut c2 = p.client(Surface::Cli);
    let again = c2
        .create_with_key(&repo_s, &base, contract(), "retry-key-1")
        .unwrap();
    assert_eq!(first, again);
    assert_eq!(c.outcomes(None).unwrap().len(), 1);
    // the same key for another request is refused, and a bad key is too
    let mut other = contract();
    other.goal = "another goal".into();
    let e = err_text(c.create_with_key(&repo_s, &base, other, "retry-key-1"));
    assert!(e.contains("different request"), "{e}");
    let e = err_text(c.create_with_key(&repo_s, &base, contract(), "k"));
    assert!(e.contains("idempotency_key"), "{e}");
    // another key is another outcome
    c.create_with_key(&repo_s, &base, contract(), "retry-key-2")
        .unwrap();
    assert_eq!(c.outcomes(None).unwrap().len(), 2);
}

#[test]
fn the_orchestrator_log_is_private() {
    use std::os::unix::fs::PermissionsExt;
    let tmp = short_tmp();
    let home = tmp.path().join("h");
    // the first spawn fails (no such binary) but the log is made before it
    let _ = ep::spawn_detached(Path::new("/no/such/synt"), &home);
    let log = run_dir(&home).join("orch.log");
    assert_eq!(
        std::fs::metadata(&log).unwrap().permissions().mode() & 0o777,
        0o600
    );
    // one an earlier version left with the umask's mode is tightened
    std::fs::set_permissions(&log, std::fs::Permissions::from_mode(0o644)).unwrap();
    let _ = ep::spawn_detached(Path::new("/no/such/synt"), &home);
    assert_eq!(
        std::fs::metadata(&log).unwrap().permissions().mode() & 0o777,
        0o600
    );
}

#[test]
fn the_orchestrator_log_is_never_opened_through_a_symlink() {
    use std::os::unix::fs::PermissionsExt;
    let tmp = short_tmp();
    let home = tmp.path().join("h");
    let _ = ep::spawn_detached(Path::new("/no/such/synt"), &home);
    let log = run_dir(&home).join("orch.log");
    let target = tmp.path().join("victim");
    std::fs::write(&target, "keep").unwrap();
    std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o644)).unwrap();
    std::fs::remove_file(&log).unwrap();
    std::os::unix::fs::symlink(&target, &log).unwrap();
    assert!(ep::spawn_detached(Path::new("/no/such/synt"), &home).is_err());
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "keep");
    assert_eq!(
        std::fs::metadata(&target).unwrap().permissions().mode() & 0o777,
        0o644,
        "the target of the link was changed"
    );
}

/// A bare endpoint (no owner thread behind it) with its own limits.
struct Bare {
    _tmp: tempfile::TempDir,
    rt: tokio::runtime::Runtime,
    ctx: Arc<ep::Ctx>,
    endpoint: String,
    key: Key,
    _rx: std::sync::mpsc::Receiver<ep::Job>,
}

fn bare(idle: Duration, queue: usize) -> Bare {
    let tmp = short_tmp();
    let home = std::fs::canonicalize(tmp.path()).unwrap().join("h");
    settings(&home, true);
    let key = ep::load_key(&home).unwrap();
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let (tx, rx) = std::sync::mpsc::sync_channel(queue);
    let ctx = Arc::new(ep::Ctx {
        env: env_of(&home),
        home: home.clone(),
        jobs: tx,
        link_idle: idle,
        clients: std::sync::atomic::AtomicUsize::new(0),
        push: tokio::sync::broadcast::channel(ep::PUSH_QUEUE).0,
        last_call: std::sync::atomic::AtomicU64::new(ep::now_secs()),
    });
    let listener = {
        let _g = rt.enter();
        synthesis_peers::net::Listener::bind(&run_dir(&home)).unwrap()
    };
    let endpoint = listener.endpoint();
    rt.spawn(ep::serve(listener, key.clone(), ctx.clone()));
    Bare {
        _tmp: tmp,
        rt,
        ctx,
        endpoint,
        key,
        _rx: rx,
    }
}

impl Bare {
    /// A link that proved the key and attached as the CLI.
    async fn link(&self) -> synthesis_peers::net::Conn {
        self.attach(json!({"t": "attach", "surface": "cli"})).await
    }

    async fn attach(&self, attach: Value) -> synthesis_peers::net::Conn {
        let mut c = connect(&self.endpoint).await.unwrap();
        handshake_client(&mut c, &self.key).await.unwrap();
        c.send(&attach).await.unwrap();
        assert_eq!(c.recv().await.unwrap().unwrap()["msg"], "hello");
        c
    }

    fn hint(&self, n: i64) -> usize {
        self.ctx
            .push
            .send(DecisionHint {
                outcome: "o1".into(),
                id: "d1".into(),
                revision: n,
                state: "pending".into(),
            })
            .unwrap_or(0)
    }
}

fn status_req(id: u64) -> Value {
    serde_json::to_value(synthesis_app::proto::Request {
        id,
        call: Call::OrchestratorStatus,
    })
    .unwrap()
}

#[test]
fn push_only_after_opt_in_and_the_line_holds_ids_and_codes_only() {
    let b = bare(Duration::from_secs(60), 4);
    b.rt.block_on(async {
        let mut plain = b.link().await;
        let mut pushed = b
            .attach(json!({"t": "attach", "surface": "cli", "push": ["decision"]}))
            .await;
        assert_eq!(b.hint(7), 1, "only the link that opted in listens");
        let line = tokio::time::timeout(Duration::from_secs(5), pushed.recv())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(
            line,
            json!({"msg": "decision", "outcome": "o1", "id": "d1", "revision": 7, "state": "pending"})
        );
        // the plain link's next line is the answer to its call, not a hint
        plain.send(&status_req(1)).await.unwrap();
        let r = plain.recv().await.unwrap().unwrap();
        assert_eq!((r["msg"].as_str(), r["id"].as_u64()), (Some("result"), Some(1)));
    });
}

#[test]
fn an_unknown_push_topic_is_refused_at_attach() {
    let b = bare(Duration::from_secs(60), 4);
    b.rt.block_on(async {
        for push in [
            json!(["decision", "x"]),
            json!("decision"),
            json!([]),
            json!(["brief"]),
        ] {
            let mut c = connect(&b.endpoint).await.unwrap();
            handshake_client(&mut c, &b.key).await.unwrap();
            c.send(&json!({"t": "attach", "surface": "cli", "push": push}))
                .await
                .unwrap();
            let r = tokio::time::timeout(Duration::from_secs(5), c.recv())
                .await
                .unwrap();
            assert!(matches!(r, Ok(None) | Err(_)), "{push}: {r:?}");
        }
    });
}

#[test]
fn a_slow_reader_loses_old_hints_and_keeps_the_newest() {
    let b = bare(Duration::from_secs(60), 4);
    b.rt.block_on(async {
        let mut c = b
            .attach(json!({"t": "attach", "surface": "cli", "push": ["decision"]}))
            .await;
        // far more than the socket buffer and the queue hold, while nobody reads
        for n in 0..20_000 {
            b.hint(n);
            if n % 500 == 0 {
                tokio::task::yield_now().await;
            }
        }
        let (mut got, mut last) = (0, -1);
        while let Ok(Ok(Some(v))) = tokio::time::timeout(Duration::from_millis(500), c.recv()).await
        {
            let n = v["revision"].as_i64().unwrap();
            assert!(n > last, "out of order");
            (got, last) = (got + 1, n);
        }
        assert!(got < 20_000, "nothing was dropped: {got}");
        assert_eq!(last, 19_999, "the newest hint must survive");
    });
}

#[test]
fn a_push_client_queues_hints_that_come_before_a_result_and_hands_them_out() {
    let b = bare(Duration::from_secs(60), 4);
    let home = b.ctx.home.clone();
    ep::write_registry(&home, &b.endpoint).unwrap();
    let mut t = LocalSocket::connect_with(&home, Surface::Cli, true).unwrap();
    assert_eq!(b.hint(1), 1);
    assert_eq!(b.hint(2), 1);
    // the call's result arrives after the hints; the hints are kept, not lost or fatal
    std::thread::sleep(Duration::from_millis(200));
    assert!(t.call(Call::OrchestratorStatus).is_ok());
    let h = t.next_hint(Duration::from_secs(5)).unwrap().unwrap();
    assert_eq!((h.id.as_str(), h.revision), ("d1", 1));
    assert_eq!(
        t.next_hint(Duration::from_secs(5))
            .unwrap()
            .unwrap()
            .revision,
        2
    );
    assert_eq!(t.next_hint(Duration::from_millis(100)).unwrap(), None);
}

#[test]
fn a_hung_link_is_dropped_and_frees_its_slot() {
    let b = bare(Duration::from_millis(300), 4);
    b.rt.block_on(async {
        let mut c = b.link().await;
        assert_eq!(b.ctx.clients.load(Ordering::SeqCst), 1);
        // it says nothing: the endpoint ends the link (EOF or an error), it does not wait for ever
        let ended = tokio::time::timeout(Duration::from_secs(5), c.recv()).await;
        assert!(matches!(ended, Ok(Ok(None) | Err(_))), "{ended:?}");
        let until = Instant::now() + Duration::from_secs(5);
        while b.ctx.clients.load(Ordering::SeqCst) != 0 {
            assert!(Instant::now() < until, "the slot was not freed");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    });
}

#[test]
fn a_full_job_queue_is_refused_as_busy_not_queued_for_ever() {
    let b = bare(Duration::from_secs(60), 1);
    b.rt.block_on(async {
        let enable = |id| {
            serde_json::to_value(synthesis_app::proto::Request {
                id,
                call: Call::OrchestratorEnable {
                    surface: Surface::Cli,
                },
            })
            .unwrap()
        };
        // nobody drains the queue: fill its only place directly, so the state is certain (a first
        // call over the wire could still be in flight when the second arrives)
        let (tx, _rx) = tokio::sync::oneshot::channel();
        b.ctx
            .jobs
            .try_send((
                Call::OrchestratorEnable {
                    surface: Surface::Cli,
                },
                tx,
            ))
            .expect("the queue is empty at the start");
        let mut link = b.link().await;
        link.send(&enable(2)).await.unwrap();
        let r = tokio::time::timeout(Duration::from_secs(30), link.recv())
            .await
            .expect("the busy answer comes at once, not after the job wait")
            .unwrap()
            .unwrap();
        assert!(
            r["error"].as_str().is_some_and(|e| e.contains("busy")),
            "not refused as busy: {r}"
        );
    });
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
