//! An outcome attempt end to end through the real `synt` binary and a scripted model (feature
//! `test-provider`): allocation, the unattended profile, the worker, settlement, and what the
//! profile leaves out (MCP servers, project hooks).
#![cfg(all(unix, feature = "test-provider"))]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use serde_json::json;
use synthesis_config::Env;
use synthesis_core::WorkspaceContext;
use synthesis_core::outcome::*;
use synthesis_orchestrator::*;
use synthesis_session::outcome::*;
use synthesis_session::{OsProbe, SCHEMA_VERSION_OUTCOMES, Store, StoreOptions};
use synthesis_workspace::{WorktreeManager, WorktreeOptions};
use time::OffsetDateTime;

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
    assert!(
        o.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&o.stderr)
    );
    String::from_utf8_lossy(&o.stdout).trim().to_string()
}

fn host_confines(home: &Path) -> bool {
    let o = Command::new(env!("CARGO_BIN_EXE_synt"))
        .args(["doctor", "--cwd"])
        .arg(home)
        .env("SYNT_HOME", home)
        .output()
        .unwrap();
    String::from_utf8_lossy(&o.stdout).contains("enforce_ready: yes")
}

struct Rig {
    dir: tempfile::TempDir,
    repo: PathBuf,
    home: PathBuf,
    store: Store,
    wm: WorktreeManager,
    head: String,
}

fn rig() -> Rig {
    let dir = tempfile::tempdir().unwrap();
    let base = std::fs::canonicalize(dir.path()).unwrap();
    let (repo, home) = (base.join("repo"), base.join("home"));
    std::fs::create_dir_all(&repo).unwrap();
    std::fs::create_dir_all(&home).unwrap();
    g(&repo, &["init", "-q"]);
    std::fs::write(repo.join("a.txt"), "one\n").unwrap();
    // a project that tries to run its own hook (it is not trusted, so it must not)
    std::fs::create_dir_all(repo.join(".synt")).unwrap();
    let hook_marker = base.join("project-hook-ran");
    std::fs::write(
        repo.join(".synt/settings.json"),
        json!({"hooks": {"SessionStart": [{"hooks": [{"type": "command",
            "command": format!("touch {}", hook_marker.display())}]}]}})
        .to_string(),
    )
    .unwrap();
    g(&repo, &["add", "."]);
    g(&repo, &["commit", "-qm", "init"]);
    let head = g(&repo, &["rev-parse", "HEAD"]);
    // a user-configured MCP server that records that it was started
    let mcp_marker = base.join("mcp-started");
    std::fs::write(
        home.join("synt.json"),
        json!({"$schema": "config.json/v1", "mcpServers": {"evil": {"command": "sh",
            "args": ["-c", format!("touch {}; sleep 5", mcp_marker.display())]}}})
        .to_string(),
    )
    .unwrap();
    // the cost cap needs a price for the scripted model
    std::fs::write(
        home.join("settings.json"),
        json!({"synt": {"pricing": {"fake/fake-model": {"input_per_mtok": 1.0, "output_per_mtok": 1.0}}}})
            .to_string(),
    )
    .unwrap();
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
        dir,
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
        goal: "write hello.txt".into(),
        criteria: vec![ContractCriterion {
            id: "c1".into(),
            text: "hello.txt exists".into(),
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
    }
}

fn plan() -> Plan {
    let item = |key: &str, kind, requires: &[&str], covers: &[&str]| PlanItem {
        key: key.into(),
        kind,
        requires: requires.iter().map(|s| (*s).into()).collect(),
        capabilities: vec![],
        covers: covers.iter().map(|s| (*s).to_string()).collect(),
    };
    Plan {
        version: 1,
        contract_version: 1,
        items: vec![
            item("write", WorkItemKind::Agent, &[], &[]),
            item("check", WorkItemKind::Verify, &["write"], &["c1"]),
        ],
    }
}

fn leased_item(r: &Rig) -> (OutcomeWriter, WorkItemId) {
    let ws = WorkspaceContext::open("w", &r.repo, &r.repo, &[], r.dir.path().join("cart"), "pg")
        .unwrap();
    let mut policy = OutcomePolicy::new(true, 16, 10, 24);
    policy.max_cost_usd = None;
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
    let items = w.insert_plan(&plan()).unwrap();
    go(&mut w, OutcomeState::Ready);
    go(&mut w, OutcomeState::Running);
    let it = items.iter().find(|(k, _)| k == "write").unwrap().1.clone();
    for to in [WorkItemState::Ready, WorkItemState::Leased] {
        let rev = w
            .list_work_items(w.id())
            .unwrap()
            .into_iter()
            .find(|i| i.id == it)
            .unwrap()
            .revision;
        w.transition_work_item(&it, rev, to, &none).unwrap();
    }
    (w, it)
}

fn worker_env(r: &Rig) -> Vec<(std::ffi::OsString, std::ffi::OsString)> {
    vec![
        ("PATH".into(), std::env::var_os("PATH").unwrap_or_default()),
        ("HOME".into(), r.dir.path().into()),
        ("SYNT_HOME".into(), r.home.clone().into()),
    ]
}

fn run(
    r: &Rig,
    script: serde_json::Value,
) -> (
    OutcomeWriter,
    WorkItemId,
    AttemptEnv,
    Result<Settled, RunError>,
) {
    let (mut w, it) = leased_item(r);
    let alloc = Allocator {
        store: &r.store,
        worktrees: &r.wm,
        platform: Platform::current(),
        artifact_root: r.dir.path().join("art"),
    };
    let source = r.repo.clone();
    let env = alloc
        .allocate(
            &mut w,
            &AllocSpec {
                item: &it,
                ordinal: 1,
                source: &source,
                base_commit: &r.head,
                worker_kind: "local-engine",
            },
            OffsetDateTime::now_utc(),
        )
        .unwrap();
    let script_path = r.dir.path().join("script.json");
    std::fs::write(&script_path, script.to_string()).unwrap();
    let mut policy = OutcomePolicy::new(true, 16, 10, 24);
    policy.attempt_max_wall_secs = Some(120);
    let profile = Profile::build(
        PathBuf::from(env!("CARGO_BIN_EXE_synt")),
        &env,
        &contract(),
        &policy,
        "write hello.txt".into(),
    )
    .unwrap()
    .with_extra(&["--provider", &format!("fake:{}", script_path.display())])
    .unwrap();
    let spawn = SyntSpawn {
        log: r.dir.path().join("worker.log"),
        env: Some(worker_env(r)),
    };
    let runner = Runner {
        store: &r.store,
        worktrees: &r.wm,
        spawn: &spawn,
        probe: &OsProbe,
        poll: Duration::from_millis(100),
        grace: Duration::from_secs(5),
    };
    let settled = runner.run_attempt(&mut w, &env, &profile, &CancelFlag::default());
    (w, it, env, settled)
}

#[test]
fn the_worker_runs_in_the_attempts_worktree_under_the_unattended_profile() {
    let r = rig();
    let confines = host_confines(&r.home);
    let hello = |r: &Rig| {
        // the model writes into the attempt's own worktree (its cwd), then finishes
        let wt = r.home.join("worktrees");
        let _ = wt;
        json!([{"text": "writing", "tools": [{"id": "w1", "name": "Write", "input": {"file_path": "hello.txt", "content": "hi\n"}}]}, {"text": "done"}, {"text": "done"}, {"text": "done"}])
    };
    let (w, it, env, settled) = run(&r, hello(&r));
    let log = std::fs::read_to_string(r.dir.path().join("worker.log")).unwrap_or_default();
    if !confines {
        // `--unattended` refuses to start without an enforcing sandbox: the attempt FAILS, no
        // command ran, and nothing was written outside the worktree
        let s = settled.unwrap();
        assert_eq!(s.end, WorkerEnd::Failed, "{log}");
        assert!(
            log.contains("sandbox") || log.contains("unattended"),
            "{log}"
        );
        assert!(!env.git.worktree.join("hello.txt").exists());
        return;
    }
    let s = settled.unwrap();
    // an unattended worker with no declared check ends `unverified`: verification is independent
    assert!(
        matches!(s.end, WorkerEnd::Completed | WorkerEnd::Unverified),
        "{log}"
    );
    assert_eq!(s.item, WorkItemState::Verifying, "{log}");
    // the file is in the attempt's worktree and in its commit, not in the user's tree
    assert_eq!(
        std::fs::read_to_string(env.git.worktree.join("hello.txt")).unwrap(),
        "hi\n"
    );
    assert_eq!(
        g(
            &env.git.worktree,
            &["show", "--name-only", "--format=", "HEAD"]
        ),
        "hello.txt"
    );
    assert!(!r.repo.join("hello.txt").exists());
    assert_eq!(g(&r.repo, &["status", "--porcelain"]), "");
    // the worker used the allocated session, with events in the store
    let a = w.list_attempts(&it).unwrap().remove(0);
    assert_eq!(a.session_id, env.session_id);
    let evs = r
        .store
        .reader()
        .unwrap()
        .read_events(&env.session_id, 0, 1000)
        .unwrap();
    assert!(!evs.is_empty(), "the session has the worker's events");
}

#[test]
fn an_attempt_starts_no_mcp_server_and_runs_no_project_hook() {
    let r = rig();
    if !host_confines(&r.home) {
        println!(
            "SKIP: no enforcing sandbox on this host (the worker would be refused before it started)"
        );
        return;
    }
    let base = std::fs::canonicalize(r.dir.path()).unwrap();
    let (mcp, hook) = (base.join("mcp-started"), base.join("project-hook-ran"));
    // positive control: the same configuration in a plain headless run DOES start the server
    let ws = base.join("plain-ws");
    std::fs::create_dir_all(&ws).unwrap();
    let s = base.join("plain-script.json");
    std::fs::write(&s, json!([{"text": "ok"}]).to_string()).unwrap();
    let o = Command::new(env!("CARGO_BIN_EXE_synt"))
        .args(["run", "--cwd"])
        .arg(&ws)
        .args([
            "--provider",
            &format!("fake:{}", s.display()),
            "--prompt",
            "x",
            "--no-cost-cap",
            "--no-worktree",
        ])
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap())
        .env("HOME", &base)
        .env("SYNT_HOME", &r.home)
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap();
    assert!(
        mcp.exists(),
        "control: a plain run starts the user's MCP server ({})",
        String::from_utf8_lossy(&o.stderr)
    );
    std::fs::remove_file(&mcp).unwrap();
    // the attempt
    let (_, _, _, settled) = run(&r, json!([{"text": "ok"}]));
    let s = settled.unwrap();
    assert_eq!(
        s.end,
        WorkerEnd::Completed,
        "{}",
        std::fs::read_to_string(base.join("worker.log")).unwrap_or_default()
    );
    std::thread::sleep(Duration::from_millis(500));
    assert!(!mcp.exists(), "the attempt started the user's MCP server");
    assert!(!hook.exists(), "the attempt ran the repository's own hook");
    let log = std::fs::read_to_string(base.join("worker.log")).unwrap_or_default();
    assert!(log.contains("MCP server(s) not started"), "{log}");
}

#[test]
fn the_worker_cannot_be_given_a_weaker_profile() {
    let r = rig();
    let (mut w, it) = leased_item(&r);
    let alloc = Allocator {
        store: &r.store,
        worktrees: &r.wm,
        platform: Platform::current(),
        artifact_root: r.dir.path().join("art"),
    };
    let source = r.repo.clone();
    let env = alloc
        .allocate(
            &mut w,
            &AllocSpec {
                item: &it,
                ordinal: 1,
                source: &source,
                base_commit: &r.head,
                worker_kind: "local-engine",
            },
            OffsetDateTime::now_utc(),
        )
        .unwrap();
    let p = Profile::build(
        PathBuf::from(env!("CARGO_BIN_EXE_synt")),
        &env,
        &contract(),
        &OutcomePolicy::new(true, 16, 10, 24),
        "x".into(),
    )
    .unwrap();
    assert!(p.clone().with_extra(&["--sandbox", "off"]).is_err());
    // the binary itself also refuses the weaker forms of an attempt
    for bad in [
        ["--sandbox", "off"],
        ["--trust-project", ""],
        ["--approve-mcp", "x"],
    ] {
        let args: Vec<_> = p
            .argv()
            .into_iter()
            .chain(bad.iter().filter(|a| !a.is_empty()).map(Into::into))
            .collect();
        let o = Command::new(env!("CARGO_BIN_EXE_synt"))
            .args(&args)
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap())
            .env("HOME", r.dir.path())
            .env("SYNT_HOME", &r.home)
            .stdin(std::process::Stdio::null())
            .output()
            .unwrap();
        assert_eq!(
            o.status.code(),
            Some(2),
            "{bad:?}: {}",
            String::from_utf8_lossy(&o.stderr)
        );
        assert!(!env.git.worktree.join("hello.txt").exists());
    }
    // --attempt-session needs --unattended and --resume (clap), and a live attempt (the store)
    let o = Command::new(env!("CARGO_BIN_EXE_synt"))
        .args(["run", "--attempt-session", "--prompt", "x", "--cwd"])
        .arg(&r.repo)
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap())
        .env("SYNT_HOME", &r.home)
        .output()
        .unwrap();
    assert_ne!(o.status.code(), Some(0));
    let o = Command::new(env!("CARGO_BIN_EXE_synt"))
        .args([
            "run",
            "--unattended",
            "--max-cost-usd",
            "1",
            "--attempt-session",
            "--resume",
            &w.control_session_id().to_string(),
            "--prompt",
            "x",
            "--cwd",
        ])
        .arg(&r.repo)
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap())
        .env("SYNT_HOME", &r.home)
        .output()
        .unwrap();
    assert_ne!(
        o.status.code(),
        Some(0),
        "the control session is not an attempt's: {}",
        String::from_utf8_lossy(&o.stderr)
    );
}
