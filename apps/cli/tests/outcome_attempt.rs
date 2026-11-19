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

/// A host without an enforcing sandbox cannot run an attempt. Where CI requires one
/// (`SYNT_REQUIRE_BWRAP`, as the sandbox tests do) that is a failure, not a skip.
fn no_sandbox_is_ok(what: &str) {
    assert!(
        std::env::var_os("SYNT_REQUIRE_BWRAP").is_none_or(|v| v.is_empty()),
        "SYNT_REQUIRE_BWRAP is set but this host has no enforcing sandbox: {what}"
    );
}

fn host_confines(home: &Path) -> bool {
    let o = Command::new(env!("CARGO_BIN_EXE_synt"))
        .args(["doctor", "--cwd"])
        .arg(home)
        .env("SYNT_HOME", home)
        .output()
        .unwrap();
    // native Windows: the suites below are written for POSIX shells and are not run under the
    // AppContainer (it is exercised by crates/synthesis-sandbox/tests/appcontainer.rs)
    !cfg!(windows) && String::from_utf8_lossy(&o.stdout).contains("enforce_ready: yes")
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
        kind: ContractKind::Write,
        version: 1,
        goal: "- write hello.txt".into(),
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
        checks: vec![],
    }
}

fn plan() -> Plan {
    let item = |key: &str, kind, requires: &[&str], covers: &[&str]| PlanItem {
        key: key.into(),
        kind,
        requires: requires.iter().map(|s| (*s).into()).collect(),
        capabilities: vec![],
        covers: covers.iter().map(|s| (*s).to_string()).collect(),
        title: String::new(),
        instructions: String::new(),
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
    leased_item_with(r, &contract())
}

fn leased_item_with(r: &Rig, contract: &Contract) -> (OutcomeWriter, WorkItemId) {
    let ws = WorkspaceContext::open("w", &r.repo, &r.repo, &[], r.dir.path().join("cart"), "pg")
        .unwrap();
    let mut policy = OutcomePolicy::new(true, 16, 10, 24);
    policy.max_cost_usd = None;
    let mut w = r
        .store
        .create_outcome(NewOutcome {
            workspace: &ws,
            base_commit: r.head.clone(),
            contract,
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

type Ran = (
    OutcomeWriter,
    WorkItemId,
    AttemptEnv,
    Result<Settled, RunError>,
);

fn run(r: &Rig, script: serde_json::Value) -> Ran {
    run_with(r, &contract(), "write hello.txt", script)
}

fn run_with(r: &Rig, contract: &Contract, prompt: &str, script: serde_json::Value) -> Ran {
    let (mut w, it) = leased_item_with(r, contract);
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
        contract,
        &policy,
        0.0,
        prompt.into(),
    )
    .unwrap()
    .with_extra(&["--provider", &format!("fake:{}", script_path.display())])
    .unwrap();
    let spawn = SyntSpawn {
        log: r.dir.path().join("worker.log"),
        env: Some(worker_env(r)),
    };
    let runner = Runner::new(
        &r.store,
        &r.wm,
        &spawn,
        &OsProbe,
        Duration::from_millis(100),
        Duration::from_secs(5),
    );
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
        no_sandbox_is_ok("the attempt cannot run");
        // `--unattended` refuses to start without an enforcing sandbox: the attempt FAILS, no
        // command ran, and nothing was written outside the worktree
        let s = settled.unwrap();
        assert_eq!(s.end, WorkerEnd::Failed, "{log}");
        assert!(
            log.contains("sandbox") || log.contains("unattended"),
            "{log}"
        );
        assert!(
            !env.git
                .as_ref()
                .unwrap()
                .worktree
                .join("hello.txt")
                .exists()
        );
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
        std::fs::read_to_string(env.git.as_ref().unwrap().worktree.join("hello.txt")).unwrap(),
        "hi\n"
    );
    assert_eq!(
        g(
            &env.git.as_ref().unwrap().worktree,
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
        no_sandbox_is_ok("the exclusion test needs a running worker");
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
            // the server writes outside the workspace: the control must not be blocked by the sandbox
            "--sandbox",
            "off",
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
    // positive control for the hook: the same project, TRUSTED, in a plain run DOES run its hook
    let o = Command::new(env!("CARGO_BIN_EXE_synt"))
        .args(["run", "--cwd"])
        .arg(&r.repo)
        .args([
            "--provider",
            &format!("fake:{}", s.display()),
            "--prompt",
            "x",
            "--no-cost-cap",
            "--no-worktree",
            "--trust-project",
            // the hook writes outside the workspace: the control must not be blocked by the sandbox
            "--sandbox",
            "off",
        ])
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap())
        .env("HOME", &base)
        .env("SYNT_HOME", &r.home)
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap();
    assert!(
        hook.exists(),
        "control: a trusted project's hook runs in a plain run ({})",
        String::from_utf8_lossy(&o.stderr)
    );
    std::fs::remove_file(&hook).unwrap();
    if mcp.exists() {
        std::fs::remove_file(&mcp).unwrap();
    }
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
        0.0,
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
        assert!(
            !env.git
                .as_ref()
                .unwrap()
                .worktree
                .join("hello.txt")
                .exists()
        );
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

/// The planning run's flag is internal and narrow: it needs `--resume` and is no attempt.
#[test]
fn plan_session_needs_resume_and_is_not_an_attempt_or_unattended() {
    for bad in [
        vec!["run", "--plan-session", "--prompt", "x"],
        vec![
            "run",
            "--plan-session",
            "--resume",
            "x",
            "--attempt-session",
            "--prompt",
            "x",
        ],
        vec![
            "run",
            "--plan-session",
            "--resume",
            "x",
            "--unattended",
            "--prompt",
            "x",
        ],
    ] {
        let o = Command::new(env!("CARGO_BIN_EXE_synt"))
            .args(&bad)
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap())
            .output()
            .unwrap();
        assert_eq!(o.status.code(), Some(2), "{bad:?}");
        // a clap refusal that names the flag or the one it needs
        let err = String::from_utf8_lossy(&o.stderr);
        assert!(
            err.contains("--plan-session") || err.contains("--resume"),
            "{bad:?}: {err}"
        );
    }
}

// ---- the gateway: the envelope's rules, non-interactive attempts, the closed bypass paths ----

/// Contract that allows writing only under `src/`.
fn src_only() -> Contract {
    let mut c = contract();
    c.envelope.allowed_paths = vec!["src/**".into()];
    c
}

fn write_call(id: &str, path: &str) -> serde_json::Value {
    json!({"text": "writing", "tools": [{"id": id, "name": "Write", "input": {"file_path": path, "content": "x\n"}}]})
}

fn done() -> [serde_json::Value; 3] {
    [
        json!({"text": "done"}),
        json!({"text": "done"}),
        json!({"text": "done"}),
    ]
}

fn script(calls: Vec<serde_json::Value>) -> serde_json::Value {
    calls.into_iter().chain(done()).collect()
}

fn decisions(w: &OutcomeWriter) -> Vec<DecisionRecord> {
    w.list_decisions(w.id()).unwrap()
}

#[test]
fn a_write_outside_allowed_paths_blocks_the_attempt_and_becomes_a_decision() {
    let r = rig();
    if !host_confines(&r.home) {
        no_sandbox_is_ok("the attempt cannot run");
        return;
    }
    // positive control first: src/ok.txt is inside the envelope and is written
    let calls = vec![
        write_call("w1", "src/ok.txt"),
        write_call("w2", "outside.txt"),
    ];
    let (w, it, env, settled) = run_with(&r, &src_only(), "write files", script(calls));
    let log = std::fs::read_to_string(r.dir.path().join("worker.log")).unwrap_or_default();
    let s = settled.unwrap();
    assert_eq!(s.end, WorkerEnd::Blocked, "{log}");
    assert_eq!(s.item, WorkItemState::WaitingDecision, "{log}");
    assert!(
        env.git
            .as_ref()
            .unwrap()
            .worktree
            .join("src/ok.txt")
            .exists(),
        "control: {log}"
    );
    assert!(
        !env.git
            .as_ref()
            .unwrap()
            .worktree
            .join("outside.txt")
            .exists()
    );
    // not in the user's tree either
    assert!(!r.repo.join("outside.txt").exists());
    // a durable, pending decision names what was needed; no live prompt was involved
    let ds = decisions(&w);
    assert_eq!(ds.len(), 1, "{ds:?}");
    let d = &ds[0];
    assert_eq!(d.kind, "approval_needed");
    assert_eq!(d.row_state, DecisionState::Pending);
    let brief = d.brief.to_string();
    assert!(
        brief.contains("outside.txt") && brief.contains("HOLD"),
        "{brief}"
    );
    assert!(d.brief.get("selected").is_none() && d.brief.get("default").is_none());
    for k in [
        "contract_version",
        "plan_version",
        "policy_hash",
        "action_hash",
        "attempt_ordinal",
    ] {
        assert!(d.binding.get(k).is_some(), "{k} in {}", d.binding);
    }
    assert_eq!(d.binding["attempt_ordinal"], 1);
    let _ = it;
}

#[test]
fn ask_user_question_becomes_a_decision_not_a_prompt() {
    let r = rig();
    if !host_confines(&r.home) {
        no_sandbox_is_ok("the attempt cannot run");
        return;
    }
    let ask = json!({"text": "asking", "tools": [{"id": "q1", "name": "AskUserQuestion", "input": {"questions": [
        {"question": "Which API shape?", "options": [{"label": "REST"}, {"label": "gRPC"}]}]}}]});
    let (w, _, _, settled) = run_with(&r, &contract(), "decide", script(vec![ask]));
    let s = settled.unwrap();
    assert_eq!(
        (s.end, s.item),
        (WorkerEnd::Blocked, WorkItemState::WaitingDecision)
    );
    let ds = decisions(&w);
    assert_eq!(ds.len(), 1);
    assert_eq!(ds[0].kind, "ambiguity");
    let brief = ds[0].brief.to_string();
    assert!(brief.contains("Which API shape?") && brief.contains("REST") && brief.contains("gRPC"));
}

/// One attempt for one bypass path: the evidence of its session and its decisions.
struct PathRun {
    ran: Ran,
    log: String,
    events: Vec<synthesis_events::Record>,
}

fn path_run(r: &Rig, prompt: &str, calls: Vec<serde_json::Value>) -> PathRun {
    let ran = run_with(r, &contract(), prompt, script(calls));
    let log = std::fs::read_to_string(r.dir.path().join("worker.log")).unwrap_or_default();
    let events = r
        .store
        .reader()
        .unwrap()
        .read_events(&ran.2.session_id, 0, 5000)
        .unwrap();
    PathRun { ran, log, events }
}

impl PathRun {
    fn end(&self) -> WorkerEnd {
        self.ran
            .3
            .as_ref()
            .unwrap_or_else(|e| panic!("{e}: {}", self.log))
            .end
    }
    fn decisions(&self) -> Vec<DecisionRecord> {
        decisions(&self.ran.0)
    }
    fn tool_error(&self, tool: &str) -> Option<bool> {
        self.events.iter().find_map(|rec| match rec {
            synthesis_events::Record::Known(e) => match &e.event {
                synthesis_events::Event::ToolCompleted { name, is_error, .. } if name == tool => {
                    Some(*is_error)
                }
                _ => None,
            },
            _ => None,
        })
    }
}

fn tool_call(id: &str, name: &str, input: serde_json::Value) -> serde_json::Value {
    json!({"text": "calling", "tools": [{"id": id, "name": name, "input": input}]})
}

#[test]
fn bypass_bang_cmd_is_plain_text_in_an_attempt() {
    let r = rig();
    if !host_confines(&r.home) {
        no_sandbox_is_ok("the attempt cannot run");
        return;
    }
    let marker = r.dir.path().join("bang-ran");
    let p = path_run(&r, &format!("!touch {}", marker.display()), vec![]);
    assert!(
        matches!(p.end(), WorkerEnd::Completed | WorkerEnd::Unverified),
        "{}",
        p.log
    );
    assert!(!marker.exists(), "`!cmd` ran");
    assert!(p.decisions().is_empty());
}

#[test]
fn bypass_git_alias_and_repo_scripts_are_refused_and_nothing_widens() {
    let r = rig();
    if !host_confines(&r.home) {
        no_sandbox_is_ok("the attempt cannot run");
        return;
    }
    let marker = r.dir.path().join("alias-ran");
    g(
        &r.repo,
        &[
            "config",
            "alias.st",
            &format!("!touch {}", marker.display()),
        ],
    );
    let settings_before = std::fs::read_to_string(r.home.join("settings.json")).unwrap();
    let p = path_run(
        &r,
        "x",
        vec![tool_call("b1", "Bash", json!({"command": "git st"}))],
    );
    assert_eq!(p.end(), WorkerEnd::Blocked, "{}", p.log);
    assert!(!marker.exists(), "a git alias ran");
    let ds = p.decisions();
    assert_eq!(ds.len(), 1);
    assert_eq!(ds[0].kind, "approval_needed");
    assert!(
        ds[0].brief["evidence"]["refused_calls"]
            .to_string()
            .contains("git st")
    );
    // no `Always`: nothing was written to any settings file
    assert_eq!(
        std::fs::read_to_string(r.home.join("settings.json")).unwrap(),
        settings_before
    );
    assert!(!r.repo.join(".synt/settings.local.json").exists());
    assert!(
        !p.ran
            .2
            .git
            .as_ref()
            .unwrap()
            .worktree
            .join(".synt/settings.local.json")
            .exists()
    );
}

#[test]
fn bypass_exit_plan_mode_blocks_with_a_blocked_decision() {
    let r = rig();
    if !host_confines(&r.home) {
        no_sandbox_is_ok("the attempt cannot run");
        return;
    }
    let p = path_run(
        &r,
        "x",
        vec![tool_call("p1", "ExitPlanMode", json!({"plan": "do it"}))],
    );
    assert_eq!(p.end(), WorkerEnd::Blocked, "{}", p.log);
    let ds = p.decisions();
    assert_eq!(ds.len(), 1);
    assert_eq!(ds[0].kind, "blocked");
    assert!(
        ds[0].brief["why"].as_str().unwrap().contains("plan mode"),
        "{}",
        ds[0].brief
    );
}

#[test]
fn bypass_tool_search_is_read_only_and_makes_no_decision() {
    let r = rig();
    if !host_confines(&r.home) {
        no_sandbox_is_ok("the attempt cannot run");
        return;
    }
    let p = path_run(
        &r,
        "x",
        vec![tool_call(
            "s1",
            "ToolSearch",
            json!({"query": "select:nothing"}),
        )],
    );
    assert!(
        matches!(p.end(), WorkerEnd::Completed | WorkerEnd::Unverified),
        "{}",
        p.log
    );
    assert!(p.decisions().is_empty());
    assert!(p.tool_error("ToolSearch").is_some(), "the call ran");
}

#[test]
fn bypass_browser_tools_do_not_exist_in_an_attempt() {
    let r = rig();
    if !host_confines(&r.home) {
        no_sandbox_is_ok("the attempt cannot run");
        return;
    }
    let p = path_run(
        &r,
        "x",
        vec![tool_call(
            "n1",
            "browser_navigate",
            json!({"url": "http://127.0.0.1:9"}),
        )],
    );
    assert_eq!(p.tool_error("browser_navigate"), Some(true), "{}", p.log);
    // the permission check comes first, so a decision is recorded, but never a grant for a tool
    // that cannot exist in an attempt
    let ds = p.decisions();
    assert_eq!(ds.len(), 1);
    assert!(
        !ds[0].brief.to_string().contains("grant_once"),
        "{}",
        ds[0].brief
    );
}

#[test]
fn a_decision_binds_to_the_policy_the_attempt_started_under_and_hashes_the_full_action() {
    let r = rig();
    if !host_confines(&r.home) {
        no_sandbox_is_ok("the attempt cannot run");
        return;
    }
    // two refused writes whose paths agree for far longer than the brief shows
    let long = |tail: &str| format!("{}{tail}", "d/".repeat(300));
    let a = run_with(
        &r,
        &src_only(),
        "x",
        script(vec![write_call("w1", &long("a.txt"))]),
    );
    let b = run_with(
        &r,
        &src_only(),
        "x",
        script(vec![write_call("w1", &long("b.txt"))]),
    );
    let (da, db) = (decisions(&a.0).remove(0), decisions(&b.0).remove(0));
    assert_ne!(
        da.binding["action_hash"], db.binding["action_hash"],
        "the hash covers the full action"
    );
    assert_eq!(da.brief["evidence"]["text_shortened_or_cleaned"], true);
    assert!(da.brief.to_string().len() < 3000);
    // the binding names the contract version and policy hash stored on the attempt at start
    let at = a.0.list_attempts(&a.1).unwrap().remove(0);
    assert_eq!(at.bound_contract_version, Some(1));
    let hash = at.bound_policy_hash.clone().expect("bound at start");
    assert_eq!(da.binding["policy_hash"], hash.as_str());
    assert_eq!(da.binding["contract_version"], 1);
}

#[test]
fn an_attempt_refuses_auto_and_bypass_modes_whatever_the_caller_says() {
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
        0.0,
        "x".into(),
    )
    .unwrap();
    let empty = r.dir.path().join("empty.json");
    std::fs::write(&empty, "[]").unwrap();
    let p = p
        .with_extra(&["--provider", &format!("fake:{}", empty.display())])
        .unwrap();
    for mode in ["bypassPermissions", "auto"] {
        // the mode value of the real argv is replaced (not appended: clap would refuse a repeat)
        let mut args = p.argv();
        let i = args.iter().position(|a| a == "--permission-mode").unwrap();
        args[i + 1] = mode.into();
        let o = Command::new(env!("CARGO_BIN_EXE_synt"))
            .args(&args)
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap())
            .env("HOME", r.dir.path())
            .env("SYNT_HOME", &r.home)
            .stdin(std::process::Stdio::null())
            .output()
            .unwrap();
        let err = String::from_utf8_lossy(&o.stderr);
        assert_eq!(o.status.code(), Some(2), "{mode}: {err}");
        assert!(
            err.contains("never auto") || err.contains("refuses"),
            "{mode}: {err}"
        );
    }
}

#[test]
fn user_settings_cannot_widen_the_compiled_envelope_of_an_attempt() {
    let r = rig();
    if !host_confines(&r.home) {
        no_sandbox_is_ok("the attempt cannot run");
        return;
    }
    // hostile user settings: blanket edit and push allows, an extra writable directory
    let extra = r.dir.path().join("extra");
    std::fs::create_dir_all(&extra).unwrap();
    let remote = r.dir.path().join("remote.git");
    g(&r.repo, &["init", "-q", "--bare", remote.to_str().unwrap()]);
    g(
        &r.repo,
        &["remote", "add", "origin", remote.to_str().unwrap()],
    );
    let settings = json!({
        "synt": {"pricing": {"fake/fake-model": {"input_per_mtok": 1.0, "output_per_mtok": 1.0}}},
        "permissions": {"allow": ["Edit", "Write", "Bash(git push:*)", "Bash"],
                         "additionalDirectories": [extra.display().to_string()],
                         "defaultMode": "acceptEdits"}
    });
    std::fs::write(r.home.join("settings.json"), settings.to_string()).unwrap();
    // push needs approval in this contract (not denied), so only the engine's table stands in the way
    let mut c = src_only();
    c.envelope
        .capabilities
        .insert(Capability::GitPush, CapabilityMode::ApprovalRequired);
    let bash = |id: &str, cmd: String| json!({"text": "run", "tools": [{"id": id, "name": "Bash", "input": {"command": cmd}}]});
    let calls = vec![
        write_call("w1", "src/ok.txt"),  // control: inside the envelope
        write_call("w2", "outside.txt"), // outside allowed_paths
        write_call("w3", &format!("{}/x.txt", extra.display())), // the extra directory
        bash("b1", "git push origin HEAD:main".into()),
    ];
    let (w, _, env, settled) = run_with(&r, &c, "go", script(calls));
    let log = std::fs::read_to_string(r.dir.path().join("worker.log")).unwrap_or_default();
    let s = settled.unwrap();
    assert_eq!(s.end, WorkerEnd::Blocked, "{log}");
    assert!(
        env.git
            .as_ref()
            .unwrap()
            .worktree
            .join("src/ok.txt")
            .exists(),
        "control: {log}"
    );
    assert!(
        !env.git
            .as_ref()
            .unwrap()
            .worktree
            .join("outside.txt")
            .exists(),
        "user allow widened the envelope"
    );
    assert!(
        !extra.join("x.txt").exists(),
        "an additional directory was writable"
    );
    assert_eq!(g(&remote, &["for-each-ref"]), "", "the push went through");
    // all three refusals are recorded as one decision of the blocked attempt
    let ds = decisions(&w);
    assert_eq!(ds.len(), 1);
    let brief = ds[0].brief.to_string();
    assert!(
        brief.contains("outside.txt") && brief.contains("git push"),
        "{brief}"
    );
}

// ---- PR-10: a blocked attempt, a person's answer, a one-shot grant, a new attempt ----

#[test]
fn a_person_grants_one_refused_write_and_only_the_next_attempt_may_make_it() {
    let r = rig();
    if !host_confines(&r.home) {
        no_sandbox_is_ok("the attempt cannot run");
        return;
    }
    // attempt 1: refused (outside src/**), the decision names exactly that write
    let (mut w, it, _, settled) = run_with(
        &r,
        &src_only(),
        "write",
        script(vec![write_call("w1", "outside.txt")]),
    );
    assert_eq!(settled.unwrap().end, WorkerEnd::Blocked);
    let d = decisions(&w).remove(0);
    assert_eq!(d.kind, "approval_needed");
    assert_eq!(
        w.decision_action(&d.id).unwrap().unwrap()["grant_rules"],
        json!(["Edit(/outside.txt)"]),
        "the grant rules come from the raw call, not from the brief"
    );
    // AC-10: the brief has the shape of a decision, offers choices and pre-selects none
    for k in [
        "why",
        "delta_from_contract",
        "alternatives",
        "evidence",
        "unknowns",
        "safe_default",
    ] {
        assert!(d.brief.get(k).is_some(), "{k} in {}", d.brief);
    }
    assert_eq!(d.brief["safe_default"], "HOLD");
    for k in [
        "selected",
        "recommended",
        "default",
        "default_choice",
        "preselected",
    ] {
        assert!(d.brief.get(k).is_none(), "{k}");
    }
    assert!(
        d.brief["alternatives"]
            .as_array()
            .unwrap()
            .iter()
            .all(|a| a.get("id").is_some() && a.get("label").is_some())
    );
    assert_eq!(d.row_state, DecisionState::Pending);
    // the person answers: not an unknown choice, not a stale brief; then grant_once
    let (_, current) = synthesis_orchestrator::current_binding_for(&w, &d, None)
        .unwrap()
        .unwrap();
    assert!(
        w.binding_is_current(&d, &current).unwrap(),
        "the binding recomputes to the stored one"
    );
    let res = |choice: &str, brief: &str| Resolution {
        decision_id: d.id.clone(),
        decision_revision: d.revision,
        approver: "user:cli".into(),
        choice: choice.into(),
        brief_sha256: brief.into(),
    };
    assert!(
        w.resolve_decision(&res("yes please", &d.brief_sha256), &current)
            .is_err()
    );
    assert!(
        w.resolve_decision(&res("grant_once", &"0".repeat(64)), &current)
            .is_err()
    );
    w.resolve_decision(&res("grant_once", &d.brief_sha256), &current)
        .unwrap();
    let mut driver = synthesis_orchestrator::DecisionDriver::default();
    let notes =
        synthesis_orchestrator::Decisions::drive(&mut driver, &mut w, OffsetDateTime::now_utc())
            .unwrap();
    assert!(notes.iter().any(|n| n.contains("created")), "{notes:?}");
    assert_eq!(w.get_work_item(&it).unwrap().state, WorkItemState::Ready);

    // attempt 2 writes the granted file AND another one: only the granted one is allowed
    let script_path = r.dir.path().join("script.json");
    std::fs::write(
        &script_path,
        script(vec![
            write_call("w1", "outside.txt"),
            write_call("w2", "other.txt"),
        ])
        .to_string(),
    )
    .unwrap();
    let spawn = SyntSpawn {
        log: r.dir.path().join("worker2.log"),
        env: Some(worker_env(&r)),
    };
    let mut policy = OutcomePolicy::new(true, 16, 10, 24);
    policy.attempt_max_wall_secs = Some(120);
    let host = ProcessHost {
        allocator: Allocator {
            store: &r.store,
            worktrees: &r.wm,
            platform: Platform::current(),
            artifact_root: r.dir.path().join("art"),
        },
        runner: Runner::new(
            &r.store,
            &r.wm,
            &spawn,
            &OsProbe,
            Duration::from_millis(100),
            Duration::from_secs(5),
        ),
        contract: src_only(),
        policy,
        program: PathBuf::from(env!("CARGO_BIN_EXE_synt")),
        extra: vec![
            "--provider".into(),
            format!("fake:{}", script_path.display()),
        ],
        source: r.repo.clone(),
        base_commit: r.head.clone(),
        worker_kind: "local-engine",
    };
    let mut sched = Scheduler::new(host, HandOff, SchedConfig::new(1, 3));
    let (stop, reports) = sched
        .run(&mut w, &OffsetDateTime::now_utc, Duration::from_millis(50))
        .unwrap();
    assert!(matches!(stop, Stop::Blocked(_)), "{stop:?} {reports:?}");
    let attempts = w.list_attempts(&it).unwrap();
    assert_eq!(attempts.len(), 2, "{reports:?}");
    let worktree = r.home.join("worktrees").join(&attempts[1].session_id);
    assert!(
        worktree.join("outside.txt").exists(),
        "the granted write was made"
    );
    assert!(
        !worktree.join("other.txt").exists(),
        "a write nobody granted was refused"
    );
    // the grant was used once, by that attempt, and the new decision asks only for the other file
    let grants = w.list_grants(w.id()).unwrap();
    assert_eq!(grants.len(), 1);
    assert_eq!(
        grants[0].consumed_by_attempt.as_deref(),
        Some(attempts[1].id.as_str())
    );
    let second = decisions(&w)
        .into_iter()
        .find(|x| x.id != d.id)
        .expect("a second decision");
    assert_eq!(
        w.decision_action(&second.id).unwrap().unwrap()["grant_rules"],
        json!(["Edit(/other.txt)"])
    );
    // the audit trail holds the grant's life
    let kinds: Vec<String> = w
        .read_events(w.control_session_id(), 0, 5000)
        .unwrap()
        .into_iter()
        .filter_map(|rec| match rec {
            synthesis_events::Record::Unknown(raw) => Some(raw.kind),
            _ => None,
        })
        .collect();
    for k in ["grant.created", "grant.consumed", "policy.decision"] {
        assert!(kinds.iter().any(|x| x == k), "{k} in {kinds:?}");
    }
    // a third attempt has no grant: the used one cannot be used again
    assert!(w.usable_grants(&it).unwrap().is_empty());
}

// ---- read-only goals (S1 section 2.1, 2.5): in place, four read tools, no write path ----

/// A leased read-only `research` item of `root`, allocated in place and run with the real worker.
fn run_read_only(r: &Rig, root: &Path, script: serde_json::Value) -> Ran {
    let c = Contract::read_only(
        "review the folder".into(),
        &["names the risks".into()],
        Budgets::new(5.0),
    );
    let ws = WorkspaceContext::open("w", root, root, &[], r.dir.path().join("cart"), "pg").unwrap();
    let mut policy = OutcomePolicy::new(true, 16, 10, 24);
    policy.max_cost_usd = None;
    let mut w = r
        .store
        .create_outcome(NewOutcome {
            workspace: &ws,
            base_commit: String::new(),
            contract: &c,
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
    let (plan, _) = make_plan(&StubPlanner, &c, 1).unwrap();
    let it = w.insert_plan(&plan).unwrap()[0].1.clone();
    go(&mut w, OutcomeState::Ready);
    go(&mut w, OutcomeState::Running);
    for to in [WorkItemState::Ready, WorkItemState::Leased] {
        let rev = w.get_work_item(&it).unwrap().revision;
        w.transition_work_item(&it, rev, to, &none).unwrap();
    }
    let alloc = Allocator {
        store: &r.store,
        worktrees: &r.wm,
        platform: Platform::current(),
        artifact_root: r.dir.path().join("art"),
    };
    let env = alloc
        .allocate_in_place(
            &mut w,
            &AllocSpec {
                item: &it,
                ordinal: 1,
                source: root,
                base_commit: "",
                worker_kind: "local-engine",
            },
            OffsetDateTime::now_utc(),
        )
        .unwrap();
    let script_path = r.dir.path().join("script.json");
    std::fs::write(&script_path, script.to_string()).unwrap();
    policy.attempt_max_wall_secs = Some(120);
    let profile = Profile::build(
        PathBuf::from(env!("CARGO_BIN_EXE_synt")),
        &env,
        &c,
        &policy,
        0.0,
        "review the folder".into(),
    )
    .unwrap()
    .with_extra(&["--provider", &format!("fake:{}", script_path.display())])
    .unwrap();
    let spawn = SyntSpawn {
        log: r.dir.path().join("worker.log"),
        env: Some(worker_env(r)),
    };
    let runner = Runner::new(
        &r.store,
        &r.wm,
        &spawn,
        &OsProbe,
        Duration::from_millis(100),
        Duration::from_secs(5),
    );
    let settled = runner.run_attempt(&mut w, &env, &profile, &CancelFlag::default());
    (w, it, env, settled)
}

fn tree_of(d: &Path) -> Vec<(String, Vec<u8>)> {
    let mut v: Vec<_> = std::fs::read_dir(d)
        .unwrap()
        .map(|e| {
            let p = e.unwrap().path();
            (
                p.file_name().unwrap().to_string_lossy().into_owned(),
                std::fs::read(&p).unwrap_or_default(),
            )
        })
        .collect();
    v.sort();
    v
}

/// `ro-write-tools-absent`, `ro-secret-not-readable` and the report: the model tries Write, Edit,
/// Bash and a read of `.env` in a folder that is not a git repository; only the reads work, the
/// folder's tree is byte for byte unchanged, no worktree exists, and the last message is the report.
#[test]
fn ro_worker_has_only_read_tools_cannot_read_secrets_and_its_last_message_is_the_report() {
    let r = rig();
    let confines = host_confines(&r.home);
    let root = std::fs::canonicalize(r.dir.path()).unwrap().join("plain");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(root.join("a.txt"), "the module\n").unwrap();
    std::fs::write(root.join(".env"), "API_KEY=TOPSECRET-VALUE\n").unwrap();
    let marker = root.join("bash-ran");
    let before = tree_of(&root);
    let tools = json!([
        {"id": "t1", "name": "Write", "input": {"file_path": root.join("hello.txt"), "content": "x\n"}},
        {"id": "t2", "name": "Edit", "input": {"file_path": root.join("a.txt"), "old_string": "the", "new_string": "evil"}},
        {"id": "t3", "name": "Bash", "input": {"command": format!("touch {}", marker.display())}},
        {"id": "t4", "name": "Read", "input": {"file_path": root.join("a.txt")}},
        {"id": "t5", "name": "Read", "input": {"file_path": root.join(".env")}},
    ]);
    let script = json!([{"text": "looking", "tools": tools}, {"text": "REPORT"}, {"text": "REPORT"}, {"text": "REPORT"}, {"text": "REPORT"}]);
    let (w, it, env, settled) = run_read_only(&r, &root, script);
    let log = std::fs::read_to_string(r.dir.path().join("worker.log")).unwrap_or_default();
    assert_eq!(tree_of(&root), before, "the folder is unchanged: {log}");
    assert!(!root.join("hello.txt").exists() && !marker.exists());
    assert!(
        !root.join(".git").exists(),
        "git was never run in the folder"
    );
    assert!(env.git.is_none());
    if !confines {
        no_sandbox_is_ok("the read-only attempt cannot run");
        assert_eq!(settled.unwrap().end, WorkerEnd::Failed, "{log}");
        return;
    }
    let s = settled.unwrap();
    assert!(
        matches!(s.end, WorkerEnd::Completed | WorkerEnd::Unverified),
        "{log}"
    );
    assert_eq!(s.item, WorkItemState::Verifying, "{log}");
    let evs = r
        .store
        .reader()
        .unwrap()
        .read_events(&env.session_id, 0, 5000)
        .unwrap();
    let outcome_of = |tool: &str, id: &str| {
        evs.iter().find_map(|e| match e {
            synthesis_events::Record::Known(e) => match &e.event {
                synthesis_events::Event::ToolCompleted {
                    tool_call_id,
                    name,
                    is_error,
                    preview,
                    output,
                    ..
                } if tool_call_id == id && name == tool => Some((
                    *is_error,
                    format!("{preview}{}", output.clone().unwrap_or_default()),
                )),
                _ => None,
            },
            _ => None,
        })
    };
    // the write tools do not exist for this worker (an allow-list of four), and neither does a shell
    for (tool, id) in [("Write", "t1"), ("Edit", "t2"), ("Bash", "t3")] {
        let (err, _) = outcome_of(tool, id).unwrap_or_else(|| (true, "no result".into()));
        assert!(err, "{tool} must not run: {log}");
    }
    let (err, shown) = outcome_of("Read", "t4").expect("the read ran");
    assert!(!err && shown.contains("the module"), "{shown}");
    let (_, shown) = outcome_of("Read", "t5").unwrap_or((true, String::new()));
    assert!(
        !shown.contains("TOPSECRET-VALUE"),
        "a secret file was read: {shown}"
    );
    // no worktree was made; the subject is the report, named by its hash
    assert!(std::fs::read_dir(r.home.join("worktrees")).map_or(true, |mut d| d.next().is_none()));
    let report = attempt_report(&r.store, &env.session_id).expect("a report");
    assert_eq!(report.text, "REPORT");
    assert_eq!(s.subject_commit, Some(report.subject()));
    let a = w.list_attempts(&it).unwrap().remove(0);
    assert_eq!(a.subject_commit, s.subject_commit);
}

/// The read scope is the declared folder, not the repository around it: a goal on `mono/pkg` cannot
/// read `mono/` (a file, `..`, or a symlink to it), and key files inside the folder stay unread.
#[test]
fn ro_read_scope_is_the_folder_not_the_enclosing_repository_and_key_files_are_denied() {
    let r = rig();
    if !host_confines(&r.home) {
        no_sandbox_is_ok("the read-only attempt cannot run");
        println!("SKIP: no enforcing sandbox on this host");
        return;
    }
    let mono = std::fs::canonicalize(r.dir.path()).unwrap().join("mono");
    let pkg = mono.join("pkg");
    std::fs::create_dir_all(&pkg).unwrap();
    g(&mono, &["init", "-q"]);
    std::fs::write(mono.join("secret.txt"), "PARENTSECRET\n").unwrap();
    std::fs::write(pkg.join("inside.txt"), "INSIDEOK\n").unwrap();
    std::fs::write(pkg.join("id_rsa"), "RSASECRET\n").unwrap();
    std::fs::write(pkg.join("x.pem"), "PEMSECRET\n").unwrap();
    std::os::unix::fs::symlink("../secret.txt", pkg.join("link")).unwrap();
    let rd = |id: &str, p: PathBuf| json!({"id": id, "name": "Read", "input": {"file_path": p}});
    let tools = json!([
        rd("t1", mono.join("secret.txt")),
        rd("t2", pkg.join("../secret.txt")),
        rd("t3", pkg.join("link")),
        rd("t4", pkg.join("inside.txt")),
        rd("t5", pkg.join("id_rsa")),
        rd("t6", pkg.join("x.pem")),
        rd("t7", mono.join(".git/config")),
    ]);
    let script =
        json!([{"text": "looking", "tools": tools}, {"text": "R"}, {"text": "R"}, {"text": "R"}]);
    let (_, _, env, settled) = run_read_only(&r, &pkg, script);
    let log = std::fs::read_to_string(r.dir.path().join("worker.log")).unwrap_or_default();
    let s = settled.unwrap();
    assert!(
        matches!(s.end, WorkerEnd::Completed | WorkerEnd::Unverified),
        "{log}"
    );
    let evs = r
        .store
        .reader()
        .unwrap()
        .read_events(&env.session_id, 0, 5000)
        .unwrap();
    let result = |id: &str| {
        evs.iter().find_map(|e| match e {
            synthesis_events::Record::Known(e) => match &e.event {
                synthesis_events::Event::ToolCompleted {
                    tool_call_id,
                    is_error,
                    preview,
                    output,
                    ..
                } if tool_call_id == id => Some((
                    *is_error,
                    format!("{preview}{}", output.clone().unwrap_or_default()),
                )),
                _ => None,
            },
            _ => None,
        })
    };
    for (id, secret) in [
        ("t1", "PARENTSECRET"),
        ("t2", "PARENTSECRET"),
        ("t3", "PARENTSECRET"),
        ("t5", "RSASECRET"),
        ("t6", "PEMSECRET"),
    ] {
        let (err, shown) = result(id).unwrap_or_else(|| panic!("{id} unanswered: {log}"));
        assert!(err && !shown.contains(secret), "{id} was read: {shown}");
    }
    let (err, shown) = result("t7").expect("t7");
    assert!(err, "the enclosing repository's .git was read: {shown}");
    let (err, shown) = result("t4").expect("t4");
    assert!(
        !err && shown.contains("INSIDEOK"),
        "a read inside the folder works: {shown}"
    );
}

/// `ro-project-hook-not-run`: in place inside a git repository whose project settings carry a
/// hook, and with a user MCP server configured, the worker starts neither, and the repository's
/// tree, index and HEAD are as they were.
#[test]
fn ro_project_hook_not_run_and_the_repository_is_untouched() {
    let r = rig();
    if !host_confines(&r.home) {
        no_sandbox_is_ok("the exclusion test needs a running worker");
        println!("SKIP: no enforcing sandbox on this host");
        return;
    }
    let base = std::fs::canonicalize(r.dir.path()).unwrap();
    let (mcp, hook) = (base.join("mcp-started"), base.join("project-hook-ran"));
    // a USER-level hook too: only `--bare` keeps it from running in the attempt
    let user_hook = base.join("user-hook-ran");
    std::fs::write(
        r.home.join("settings.json"),
        json!({"synt": {"pricing": {"fake/fake-model": {"input_per_mtok": 1.0, "output_per_mtok": 1.0}}},
            "hooks": {"SessionStart": [{"hooks": [{"type": "command",
                "command": format!("touch {}", user_hook.display())}]}]}})
        .to_string(),
    )
    .unwrap();
    let head = g(&r.repo, &["rev-parse", "HEAD"]);
    let status = g(&r.repo, &["status", "--porcelain", "--ignored"]);
    let (_, _, _, settled) = run_read_only(
        &r,
        &r.repo.clone(),
        json!([{"text": "REPORT"}, {"text": "REPORT"}, {"text": "REPORT"}]),
    );
    let log = std::fs::read_to_string(base.join("worker.log")).unwrap_or_default();
    let s = settled.unwrap();
    assert!(
        matches!(s.end, WorkerEnd::Completed | WorkerEnd::Unverified),
        "{log}"
    );
    std::thread::sleep(Duration::from_millis(500));
    assert!(
        !mcp.exists(),
        "the read-only attempt started the user's MCP server"
    );
    assert!(
        !hook.exists(),
        "the read-only attempt ran the repository's own hook"
    );
    assert_eq!(g(&r.repo, &["rev-parse", "HEAD"]), head);
    assert_eq!(g(&r.repo, &["status", "--porcelain", "--ignored"]), status);
    assert_eq!(
        g(&r.repo, &["worktree", "list"]).lines().count(),
        1,
        "no worktree"
    );
}

/// The read-only flag is internal and narrow: it only rides on an attempt session (which needs
/// `--resume` and `--unattended`), never alone and never with a planning run or a worktree.
#[test]
fn read_only_session_needs_an_attempt_session_and_takes_no_worktree() {
    for bad in [
        vec!["run", "--read-only-session", "--prompt", "x"],
        vec![
            "run",
            "--read-only-session",
            "--resume",
            "x",
            "--prompt",
            "x",
        ],
        vec![
            "run",
            "--read-only-session",
            "--resume",
            "x",
            "--plan-session",
            "--prompt",
            "x",
        ],
        vec![
            "run",
            "--read-only-session",
            "--attempt-session",
            "--resume",
            "x",
            "--unattended",
            "--worktree",
            "--prompt",
            "x",
        ],
    ] {
        let o = Command::new(env!("CARGO_BIN_EXE_synt"))
            .args(&bad)
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap())
            .output()
            .unwrap();
        assert_eq!(o.status.code(), Some(2), "{bad:?}");
    }
}
