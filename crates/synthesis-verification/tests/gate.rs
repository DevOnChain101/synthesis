use std::collections::BTreeMap;
use std::fs;
use std::path::Path;
use std::sync::Arc;
use synthesis_core::WorkspaceContext;
use synthesis_events::{Event, TaskStatus};
use synthesis_terminal::{SandboxMode, TerminalRuntime};
use synthesis_verification::*;
use tokio_util::sync::CancellationToken;

const SID: &str = "s1";

struct Fx {
    _dir: tempfile::TempDir,
    rt: Arc<TerminalRuntime>,
    ws: Arc<WorkspaceContext>,
}

fn fx_path(files: &[(&str, &str)], path: Option<&str>) -> Fx {
    let dir = tempfile::tempdir().unwrap();
    let base = synthesis_core::canonicalize(dir.path()).unwrap();
    let w = base.join("w");
    fs::create_dir_all(&w).unwrap();
    for (n, c) in files {
        fs::write(w.join(n), c).unwrap();
    }
    let ws = Arc::new(WorkspaceContext::open("w", &w, &w, &[], base.join("art"), "pg").unwrap());
    let mut env = BTreeMap::new();
    let host_path = std::env::var("PATH").unwrap();
    env.insert("PATH".to_string(), path.map_or(host_path, Into::into));
    // Windows needs its own set for cargo/node/python to start under an otherwise empty env.
    let win = [
        "SystemRoot",
        "USERPROFILE",
        "APPDATA",
        "LOCALAPPDATA",
        "TEMP",
        "TMP",
        "PATHEXT",
        "ComSpec",
        "HOMEDRIVE",
        "HOMEPATH",
        // rustc locates the MSVC linker through these; without them it falls back to Git's `link`.
        "ProgramFiles",
        "ProgramFiles(x86)",
        "ProgramData",
        "windir",
        "SystemDrive",
        "LIB",
        "INCLUDE",
    ];
    for k in ["HOME", "CARGO_HOME", "RUSTUP_HOME", "RUSTUP_TOOLCHAIN"]
        .into_iter()
        .chain(win.into_iter().filter(|_| cfg!(windows)))
    {
        if let Ok(v) = std::env::var(k) {
            env.insert(k.into(), v);
        }
    }
    let rt = Arc::new(TerminalRuntime::new(SandboxMode::Off));
    rt.open_session(SID, ws.clone(), env);
    Fx { _dir: dir, rt, ws }
}

fn fx(files: &[(&str, &str)]) -> Fx {
    fx_path(files, None)
}

impl Fx {
    fn gate(&self, cfg: GateConfig) -> CompletionGate {
        CompletionGate::new(
            VerifyRunner::new(self.rt.clone(), SID, self.ws.clone()),
            cfg,
        )
    }
    fn runner(&self) -> VerifyRunner {
        VerifyRunner::new(self.rt.clone(), SID, self.ws.clone())
    }
}

fn check(f: &Fx, id: &str, cmd: &str, timeout_ms: u64) -> Check {
    Check {
        id: id.into(),
        kind: CheckKind::Custom,
        command: Some(cmd.into()),
        cwd: f.ws.cwd.clone(),
        timeout_ms,
        declared_by: Declarer::Model,
        criteria_ids: vec![],
    }
}

fn state(criteria: Vec<Criterion>, changed: &[&str]) -> TaskState {
    TaskState {
        task_id: "t1".into(),
        session_id: SID.into(),
        criteria,
        side_effects: true,
        changed_files: changed.iter().map(|s| s.to_string()).collect(),
        ..Default::default()
    }
}

fn have(tool: &str) -> bool {
    std::process::Command::new(tool)
        .arg("--version")
        .output()
        .is_ok()
}

const NODE_OK: &str =
    "const t=require('node:test');const a=require('node:assert');t('a',()=>a.equal(1,1));";
const NODE_BAD: &str =
    "const t=require('node:test');const a=require('node:assert');t('a',()=>a.equal(1,2));";
const PY_OK: &str =
    "import unittest\nclass T(unittest.TestCase):\n    def test_a(self): self.assertEqual(1, 1)\n";
const PY_BAD: &str =
    "import unittest\nclass T(unittest.TestCase):\n    def test_a(self): self.assertEqual(1, 2)\n";

fn node_criteria() -> Vec<Criterion> {
    vec![Criterion::user("c1", "math works", Some("node --test"))]
}

#[tokio::test]
async fn node_pass_completes_with_evidence() {
    if !have("node") {
        return;
    }
    let f = fx(&[("a.test.js", NODE_OK)]);
    let mut g = f.gate(GateConfig::default());
    let d = g
        .evaluate(
            &state(node_criteria(), &["a.test.js"]),
            true,
            &CancellationToken::new(),
        )
        .await;
    let GateDecision::Complete(Some(r)) = d else {
        panic!("{d:?}")
    };
    assert_eq!(r.status, VerificationStatus::Passed);
    assert_eq!(r.criteria[0].status, VerificationStatus::Passed);
    assert_eq!(r.criteria[0].evidence, ["k1"]);
    let k = &r.checks[0];
    assert_eq!(k.exit_code, Some(0));
    assert_eq!(k.artifacts.len(), 1);
    assert!(f.ws.artifact_root.join(&k.artifacts[0].path).is_file());
    assert_eq!(k.artifacts[0].sha256.len(), 64);
}

#[tokio::test]
async fn node_fail_gives_feedback_then_no_progress_stop() {
    if !have("node") {
        return;
    }
    let f = fx(&[("a.test.js", NODE_BAD)]);
    let mut g = f.gate(GateConfig::default());
    let st = state(node_criteria(), &["a.test.js"]);
    let c = CancellationToken::new();
    let GateDecision::Continue { feedback, report } = g.evaluate(&st, true, &c).await else {
        panic!("expected Continue")
    };
    assert_eq!(report.status, VerificationStatus::Failed);
    assert!(feedback.contains("FAILED k1"), "{feedback}");
    assert!(feedback.contains("NOT verified"));
    assert!(!report.checks[0].failure_fingerprint.is_none());
    // same failure, same changed files => stop
    let GateDecision::Stop { status, reason, .. } = g.evaluate(&st, true, &c).await else {
        panic!("expected Stop")
    };
    assert_eq!(
        (status, reason.as_str()),
        (TaskStatus::Failed, "no_progress")
    );
}

#[tokio::test]
async fn budget_stop_when_progress_is_made_but_attempts_run_out() {
    if !have("node") {
        return;
    }
    let f = fx(&[("a.test.js", NODE_BAD)]);
    let cfg = GateConfig {
        max_verify_attempts: 2,
        ..Default::default()
    };
    let mut g = f.gate(cfg);
    let c = CancellationToken::new();
    assert!(matches!(
        g.evaluate(&state(node_criteria(), &["a"]), true, &c).await,
        GateDecision::Continue { .. }
    ));
    let GateDecision::Stop {
        status,
        reason,
        report,
    } = g
        .evaluate(&state(node_criteria(), &["a", "b"]), true, &c)
        .await
    else {
        panic!("expected Stop")
    };
    assert_eq!(
        (status, reason.as_str()),
        (TaskStatus::Failed, "verify_budget_exhausted")
    );
    assert_eq!(report.unwrap().attempt, 2);
}

#[tokio::test]
async fn python_unittest_inferred_pass_and_fail() {
    if !have("python3") {
        return;
    }
    let f = fx(&[("test_a.py", PY_BAD)]);
    let mut g = f.gate(GateConfig::default());
    let c = CancellationToken::new();
    let GateDecision::Continue { report, .. } = g.evaluate(&state(vec![], &[]), true, &c).await
    else {
        panic!("expected Continue")
    };
    assert_eq!(report.status, VerificationStatus::Failed);
    assert!(
        report.checks[0]
            .command
            .as_deref()
            .unwrap()
            .contains("unittest discover")
    );

    // Inferred-only pass: one passed check and none failed is `passed` (verification.md §3 rule 4).
    let f = fx(&[("test_a.py", PY_OK)]);
    let mut g = f.gate(GateConfig::default());
    let GateDecision::Complete(Some(r)) = g.evaluate(&state(vec![], &[]), true, &c).await else {
        panic!("expected Complete")
    };
    assert_eq!(r.status, VerificationStatus::Passed);
    assert!(
        r.limitations
            .iter()
            .any(|l| l.contains("inferred checks only"))
    );

    // Opt-out: capped at unverified, one correction round, then stop.
    let cfg = GateConfig {
        inferred_only: InferredOnlyPolicy::CapUnverified,
        ..Default::default()
    };
    let mut g = f.gate(cfg);
    let GateDecision::Continue { report, feedback } =
        g.evaluate(&state(vec![], &[]), true, &c).await
    else {
        panic!("expected Continue")
    };
    assert_eq!(report.status, VerificationStatus::Unverified);
    assert!(feedback.contains("verify_run"));
    let GateDecision::Stop { status, .. } = g.evaluate(&state(vec![], &[]), true, &c).await else {
        panic!("expected Stop")
    };
    assert_eq!(status, TaskStatus::Unverified);
}

#[test]
fn aggregate_inferred_pass_with_a_blocked_check_is_passed_with_a_limitation() {
    use synthesis_verification::{CheckResult, aggregate};
    let mk = |id: &str, status| CheckResult {
        id: id.into(),
        status,
        command: Some(id.into()),
        ..serde_json::from_value(serde_json::json!({
            "id": "", "kind": "unit", "command": null, "cwd": ".", "declared_by": "harness",
            "started_at": "", "finished_at": "", "duration_ms": 0, "exit_code": null,
            "timed_out": false, "status": "passed", "blocked_reason": null, "summary": "",
            "failure_fingerprint": null, "criteria_ids": [], "artifacts": []
        }))
        .unwrap()
    };
    let p = InferredOnlyPolicy::default();
    let lint = |c: CheckResult| CheckResult {
        kind: CheckKind::Lint,
        ..c
    };
    let (s, lim) = aggregate(
        &mut [],
        &[
            mk("a", VerificationStatus::Passed),
            lint(mk("b", VerificationStatus::Blocked)),
        ],
        p,
    );
    assert_eq!(s, VerificationStatus::Passed);
    assert!(lim.iter().any(|l| l.contains("check b")), "{lim:?}");
    let (s, _) = aggregate(
        &mut [],
        &[
            mk("a", VerificationStatus::Passed),
            mk("b", VerificationStatus::Failed),
        ],
        p,
    );
    assert_eq!(s, VerificationStatus::Failed);
    let (s, _) = aggregate(&mut [], &[mk("b", VerificationStatus::Blocked)], p);
    assert_eq!(s, VerificationStatus::Unverified);
}

#[tokio::test]
async fn python_user_criterion_passes() {
    if !have("python3") {
        return;
    }
    let f = fx(&[("test_a.py", PY_OK)]);
    let crit = vec![Criterion::user(
        "c1",
        "a holds",
        Some("python3 -m unittest"),
    )];
    let mut g = f.gate(GateConfig::default());
    let d = g
        .evaluate(&state(crit, &[]), true, &CancellationToken::new())
        .await;
    assert!(matches!(d, GateDecision::Complete(Some(r)) if r.status == VerificationStatus::Passed));
}

#[tokio::test]
async fn cargo_test_offline_pass_and_fail() {
    if !have("cargo") {
        return;
    }
    for (body, want) in [
        ("assert_eq!(1, 1)", VerificationStatus::Passed),
        ("assert_eq!(1, 2)", VerificationStatus::Failed),
    ] {
        let src = format!("#[test]\nfn t() {{ {body} }}\n");
        let f = fx(&[
            (
                "Cargo.toml",
                "[package]\nname=\"fx\"\nversion=\"0.1.0\"\nedition=\"2021\"\n",
            ),
            ("lib.rs", &src),
        ]);
        fs::write(
            f.ws.cwd.join("Cargo.toml"),
            "[package]\nname=\"fx\"\nversion=\"0.1.0\"\nedition=\"2021\"\n[lib]\npath=\"lib.rs\"\n",
        )
        .unwrap();
        let k = check(&f, "k1", "cargo test --offline", 300_000);
        let r = f.runner().run_one(&k, &CancellationToken::new()).await;
        assert_eq!(r.status, want, "{}", r.summary);
    }
}

#[tokio::test]
async fn timeout_is_failed() {
    let f = fx(&[]);
    let r = f
        .runner()
        .run_one(&check(&f, "k1", "sleep 30", 300), &CancellationToken::new())
        .await;
    assert_eq!(r.status, VerificationStatus::Failed);
    assert!(r.timed_out);
    assert!(r.duration_ms < 10_000);
}

/// Three checks that all need `cargo` (not on the sandbox PATH) are one limitation with one hint, and the
/// gate still stops `blocked` (nothing was verified).
#[tokio::test]
async fn a_program_missing_from_the_sandbox_path_is_named_once() {
    let f = fx_path(&[("a.test.js", NODE_OK)], Some("/nonexistent"));
    let mut results = vec![];
    for (id, cmd) in [
        ("k1", "cargo fmt --check"),
        ("k2", "cargo clippy"),
        ("k3", "cargo test"),
    ] {
        let mut c = check(&f, id, cmd, 5000);
        c.kind = CheckKind::Unit;
        c.declared_by = Declarer::Harness;
        results.push(f.runner().run_one(&c, &CancellationToken::new()).await);
    }
    let (s, lim) = aggregate(&mut [], &results, InferredOnlyPolicy::default());
    assert_eq!(s, VerificationStatus::Unverified);
    let named: Vec<_> = lim
        .iter()
        .filter(|l| l.contains("program not found in the sandbox PATH"))
        .collect();
    assert_eq!(named.len(), 1, "{lim:?}");
    assert!(
        named[0].starts_with("blocked: ")
            && named[0].contains("cargo (checks k1, k2, k3)")
            && named[0].contains("PATH"),
        "{named:?}"
    );
    assert!(
        !lim.iter()
            .any(|l| l.contains("command not found") || l.contains("planned test")),
        "{lim:?}"
    );
}

#[tokio::test]
async fn missing_binary_is_blocked_not_failed() {
    let f = fx(&[]);
    let r = f
        .runner()
        .run_one(
            &check(&f, "k1", "definitely-not-a-real-tool-xyz --x", 5000),
            &CancellationToken::new(),
        )
        .await;
    assert_eq!(r.status, VerificationStatus::Blocked);
    assert_eq!(
        r.blocked_reason.unwrap(),
        "program not found in the sandbox PATH: definitely-not-a-real-tool-xyz"
    );
    assert!(r.failure_fingerprint.is_none());

    // Each language tool, when it is absent from PATH, is blocked; gate stops `blocked`.
    let f = fx_path(
        &[("a.test.js", NODE_OK), ("test_a.py", PY_OK)],
        Some("/nonexistent"),
    );
    for cmd in ["node --test", "python3 -m unittest", "cargo test --offline"] {
        let r = f
            .runner()
            .run_one(&check(&f, "k1", cmd, 5000), &CancellationToken::new())
            .await;
        assert_eq!(
            r.status,
            VerificationStatus::Blocked,
            "{cmd}: {}",
            r.summary
        );
    }
    let mut g = f.gate(GateConfig::default());
    let d = g
        .evaluate(
            &state(node_criteria(), &[]),
            true,
            &CancellationToken::new(),
        )
        .await;
    let GateDecision::Stop { status, reason, .. } = d else {
        panic!("{d:?}")
    };
    assert_eq!(
        (status, reason.as_str()),
        (TaskStatus::Blocked, "verification_blocked")
    );
}

#[tokio::test]
async fn env_and_port_problems_are_blocked_and_zero_tests_unverified() {
    let f = fx(&[]);
    let c = CancellationToken::new();
    let run = |cmd: &'static str| {
        let k = check(&f, "k1", cmd, 5000);
        let r = f.runner();
        let c = c.clone();
        async move { r.run_one(&k, &c).await }
    };
    assert_eq!(
        run("echo 'listen EADDRINUSE :::3000' >&2; exit 1")
            .await
            .status,
        VerificationStatus::Blocked
    );
    assert_eq!(
        run("echo 'API_KEY environment variable is not set'; exit 1")
            .await
            .status,
        VerificationStatus::Blocked
    );
    assert_eq!(
        run("echo 'assertion failed'; exit 1").await.status,
        VerificationStatus::Failed
    );
    assert_eq!(
        run("echo 'Ran 0 tests in 0.000s'").await.status,
        VerificationStatus::Unverified
    );
}

#[tokio::test]
async fn cwd_outside_workspace_is_refused() {
    let f = fx(&[]);
    let mut k = check(&f, "k1", "true", 5000);
    k.cwd = "/".into();
    let r = f.runner().run_one(&k, &CancellationToken::new()).await;
    assert_eq!(r.status, VerificationStatus::Blocked);
}

#[tokio::test]
async fn qa_task_skips_gate_and_cancel_stops() {
    let f = fx(&[("test_a.py", PY_OK)]);
    let mut g = f.gate(GateConfig::default());
    let st = TaskState {
        task_id: "t".into(),
        ..Default::default()
    };
    assert_eq!(
        g.evaluate(&st, true, &CancellationToken::new()).await,
        GateDecision::Complete(None)
    );

    let cancel = CancellationToken::new();
    cancel.cancel();
    let d = g.evaluate(&state(vec![], &[]), true, &cancel).await;
    assert!(matches!(
        d,
        GateDecision::Stop {
            status: TaskStatus::Cancelled,
            ..
        }
    ));
}

#[tokio::test]
async fn regression_is_reported_and_report_roundtrips() {
    let f = fx(&[]);
    let mut g = f.gate(GateConfig::default());
    let c = CancellationToken::new();
    let flag = f.ws.cwd.join("flag");
    let cmd = "test -e flag".to_string(); // relative: a `\` path would be eaten by the shell on Windows
    // attempt 1: passes (flag exists) -> but add a second failing criterion to continue
    fs::write(&flag, "").unwrap();
    let two = vec![
        Criterion::user("c1", "x", Some(&cmd)),
        Criterion::user("c2", "y", Some("exit 1")),
    ];
    assert!(matches!(
        g.evaluate(&state(two.clone(), &["a"]), true, &c).await,
        GateDecision::Continue { .. }
    ));
    fs::remove_file(&flag).unwrap();
    let GateDecision::Continue { report, .. } = g.evaluate(&state(two, &["b"]), true, &c).await
    else {
        panic!("expected Continue")
    };
    assert!(
        report
            .limitations
            .iter()
            .any(|l| l.starts_with("regression:"))
    );

    let json = serde_json::to_string(&*report).unwrap();
    let back: VerificationReport = serde_json::from_str(&json).unwrap();
    assert_eq!(*report, back);
    let Event::VerificationCompleted {
        report_id,
        status,
        attempt,
        ..
    } = report.to_event()
    else {
        panic!()
    };
    assert_eq!(
        (report_id.as_str(), status.as_str(), attempt),
        ("vr-t1-2", "failed", 2)
    );
}

#[test]
fn aggregate_rules() {
    use VerificationStatus::*;
    let mk = |id: &str, st, crit: &str| CheckResult {
        id: id.into(),
        kind: CheckKind::Custom,
        command: Some(id.into()),
        cwd: Path::new("/").into(),
        declared_by: Declarer::User,
        started_at: String::new(),
        finished_at: String::new(),
        duration_ms: 0,
        exit_code: None,
        timed_out: false,
        status: st,
        blocked_reason: None,
        summary: String::new(),
        failure_fingerprint: None,
        criteria_ids: vec![crit.into()],
        artifacts: vec![],
        tests: None,
    };
    let p = InferredOnlyPolicy::CapUnverified;
    let mut c = vec![Criterion::user("c1", "", None)];
    assert_eq!(aggregate(&mut c, &[mk("k1", Blocked, "c1")], p).0, Blocked);
    assert_eq!(
        aggregate(&mut c, &[mk("k1", Unverified, "c1")], p).0,
        Unverified
    );
    assert_eq!(aggregate(&mut c, &[], p).0, Unverified);
    assert_eq!(aggregate(&mut c, &[mk("k1", Passed, "c1")], p).0, Passed);
    // a failed inferred check fails the report even when the user criterion passed
    assert_eq!(
        aggregate(&mut c, &[mk("k1", Passed, "c1"), mk("k2", Failed, "")], p).0,
        Failed
    );
    // passed + blocked on the same criterion: best evidence is passed
    assert_eq!(
        aggregate(
            &mut c,
            &[mk("k1", Passed, "c1"), mk("k2", Blocked, "c1")],
            p
        )
        .0,
        Passed
    );
}

struct Policy(Vec<(&'static str, CheckAuth)>);

impl CheckAuthorizer for Policy {
    fn authorize<'a>(
        &'a self,
        c: &'a Check,
        _: &'a CancellationToken,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = CheckAuth> + Send + 'a>> {
        let cmd = c.command.as_deref().unwrap_or("");
        let a = self
            .0
            .iter()
            .find(|(p, _)| cmd.starts_with(p))
            .map_or(CheckAuth::Allow, |(_, a)| a.clone());
        Box::pin(async move { a })
    }
}

#[tokio::test]
async fn authorizer_blocks_a_check_without_running_or_failing_it() {
    let f = fx(&[]);
    let r = f.runner();
    let checks = [
        check(&f, "ok", "echo fine", 10_000),
        check(&f, "ask", "touch ran-anyway", 10_000),
        check(&f, "deny", "echo no", 10_000),
    ];
    let policy = Policy(vec![
        (
            "touch",
            CheckAuth::Blocked(format!("{NEEDS_APPROVAL}Bash(touch:*)")),
        ),
        (
            "echo no",
            CheckAuth::Blocked("denied by rule Bash(echo no)".into()),
        ),
    ]);
    let out = r
        .run_with(&checks, &CancellationToken::new(), Some(&policy))
        .await;
    let st: Vec<_> = out.iter().map(|k| k.status).collect();
    assert_eq!(
        st,
        [
            VerificationStatus::Passed,
            VerificationStatus::Blocked,
            VerificationStatus::Blocked
        ]
    );
    assert_eq!(
        out[1].blocked_reason.as_deref(),
        Some("needs_approval:Bash(touch:*)")
    );
    assert!(out[1].exit_code.is_none() && !f.ws.cwd.join("ran-anyway").exists());
    assert!(
        out[2]
            .blocked_reason
            .as_deref()
            .unwrap()
            .starts_with("denied by rule")
    );
}

#[tokio::test]
async fn gate_with_a_blocked_user_check_is_blocked_not_failed() {
    let f = fx(&[]);
    let mut g = f.gate(GateConfig::default());
    let policy = Policy(vec![(
        "echo",
        CheckAuth::Blocked(format!("{NEEDS_APPROVAL}Bash(echo:*)")),
    )]);
    let crit = vec![Criterion::user("c1", "says hi", Some("echo hi"))];
    let d = g
        .evaluate_with(
            &state(crit, &[]),
            true,
            &CancellationToken::new(),
            Some(&policy),
        )
        .await;
    let GateDecision::Stop {
        status,
        report: Some(r),
        ..
    } = d
    else {
        panic!("{d:?}")
    };
    assert_eq!(status, TaskStatus::Blocked);
    assert_eq!(r.checks[0].status, VerificationStatus::Blocked);
}

#[tokio::test]
async fn edit_without_a_relevant_check_is_unverified_not_not_applicable() {
    let c = CancellationToken::new();
    // Irrelevant-only: a changed .txt file makes the python check irrelevant -> nothing runs.
    let f = fx(&[("test_a.py", PY_OK), ("hello.txt", "hi")]);
    let mut g = f.gate(GateConfig::default());
    let st = state(vec![], &["hello.txt"]);
    let GateDecision::Continue { report, .. } = g.evaluate(&st, true, &c).await else {
        panic!("expected one feedback round")
    };
    assert_eq!(report.status, VerificationStatus::Unverified);
    assert!(report.checks.is_empty());
    assert_eq!(report.changed_files, vec!["hello.txt".to_string()]);
    let GateDecision::Stop { status, report, .. } = g.evaluate(&st, true, &c).await else {
        panic!("expected Stop")
    };
    assert_eq!(status, TaskStatus::Unverified);
    assert!(report.is_some());

    // No checks at all in the workspace: same outcome.
    let f = fx(&[("hello.txt", "hi")]);
    let mut g = f.gate(GateConfig::default());
    assert!(matches!(
        g.evaluate(&state(vec![], &["hello.txt"]), true, &c).await,
        GateDecision::Continue { .. }
    ));

    // One relevant passing check -> passed.
    if have("python3") {
        let f = fx(&[("test_a.py", PY_OK)]);
        let mut g = f.gate(GateConfig::default());
        let d = g.evaluate(&state(vec![], &["test_a.py"]), true, &c).await;
        assert!(matches!(d, GateDecision::Complete(Some(_))), "{d:?}");
    }
}

// ---- honesty: skipped tests and weakened tests are not a clean pass -------------------------

/// A check whose stdout is a node:test summary (portable: no node needed).
fn summary_criteria(pass: u32, skipped: u32) -> Vec<Criterion> {
    let cmd = format!(
        "echo 'ok 1 - needs db # SKIP'; echo '# pass {pass}'; echo '# fail 0'; echo '# skipped {skipped}'; echo '# todo 0'; hostname"
    );
    vec![Criterion::user("c1", "db works", Some(&cmd))]
}

async fn run_gate(st: TaskState) -> (GateDecision, Fx) {
    let f = fx(&[]);
    let d = f
        .gate(GateConfig::default())
        .evaluate(&st, true, &CancellationToken::new())
        .await;
    (d, f)
}

fn unverified(d: GateDecision) -> VerificationReport {
    match d {
        GateDecision::Continue { report, .. } => *report,
        GateDecision::Stop { report, .. } => *report.unwrap(),
        d => panic!("expected unverified, got {d:?}"),
    }
}

#[tokio::test]
async fn skipped_tests_are_passed_with_limitations_not_passed() {
    let (d, _f) = run_gate(state(summary_criteria(1, 1), &["t.js"])).await;
    let r = unverified(d);
    assert_eq!(r.status, VerificationStatus::Unverified);
    assert_eq!(r.checks[0].exit_code, Some(0));
    assert_eq!(r.checks[0].tests.as_ref().unwrap().skipped, 1);
    let l = r
        .limitations
        .iter()
        .find(|l| l.starts_with("skipped tests"))
        .unwrap();
    assert!(l.contains("needs db"), "{l}");
    assert!(r.summary().contains("skipped tests"), "{}", r.summary());

    // no skips: clean pass
    let (d, _f) = run_gate(state(summary_criteria(2, 0), &["t.js"])).await;
    assert!(matches!(d, GateDecision::Complete(Some(_))), "{d:?}");
}

#[tokio::test]
async fn a_skip_that_pre_existed_in_the_baseline_is_not_held_against_the_agent() {
    let c = summary_criteria(1, 1);
    let mut st = state(c.clone(), &["t.js"]);
    st.baseline_skipped = [(c[0].check.clone().unwrap(), 1)].into();
    let (d, _f) = run_gate(st).await;
    assert!(matches!(d, GateDecision::Complete(Some(_))), "{d:?}");
}

#[tokio::test]
async fn weakening_a_test_voids_a_pass_and_is_reported() {
    let mut st = state(summary_criteria(1, 0), &["test/db.test.js"]);
    st.edits = vec![FileEdit {
        path: "test/db.test.js".into(),
        added: vec!["process.env.DB_URL ??= 'postgres://x'".into()],
        removed: vec![],
    }];
    let (d, _f) = run_gate(st).await;
    let r = unverified(d);
    assert_eq!(r.status, VerificationStatus::Unverified);
    assert!(
        r.limitations
            .iter()
            .any(|l| l.starts_with("test files modified: test/db.test.js")),
        "{:?}",
        r.limitations
    );
    assert!(r.summary().contains("test files modified"));
}

#[tokio::test]
async fn defaulting_a_missing_credential_in_source_voids_a_pass() {
    let mut st = state(summary_criteria(2, 0), &["src/db.js"]);
    st.edits = vec![FileEdit {
        path: "src/db.js".into(),
        added: vec!["const url = process.env.DB_URL || 'sqlite::memory:';".into()],
        removed: vec!["  if (!url) throw new Error('DB_URL is not set');".into()],
    }];
    let (d, _f) = run_gate(st).await;
    let r = unverified(d);
    assert!(
        r.summary()
            .contains("prerequisite guard removed: src/db.js"),
        "{}",
        r.summary()
    );
}

#[tokio::test]
async fn node_skip_is_detected_end_to_end() {
    if !have("node") {
        return;
    }
    let f = fx(&[(
        "a.test.js",
        "require('node:test')('needs db', { skip: !process.env.DB_URL }, () => {});",
    )]);
    let d = f
        .gate(GateConfig::default())
        .evaluate(
            &state(node_criteria(), &["a.test.js"]),
            true,
            &CancellationToken::new(),
        )
        .await;
    let r = unverified(d);
    assert_eq!(r.status, VerificationStatus::Unverified);
    assert!(
        r.limitations.iter().any(|l| l.starts_with("skipped tests")),
        "{:?}",
        r.limitations
    );
}

// ---- gate hardening: weak passes, redefined checks, shell edits, flaky checks ---------------

fn result(id: &str, kind: CheckKind, by: Declarer, status: VerificationStatus) -> CheckResult {
    CheckResult {
        id: id.into(),
        kind,
        command: Some(id.into()),
        cwd: Path::new("/").into(),
        declared_by: by,
        started_at: String::new(),
        finished_at: String::new(),
        duration_ms: 0,
        exit_code: None,
        timed_out: false,
        status,
        blocked_reason: None,
        summary: String::new(),
        failure_fingerprint: None,
        criteria_ids: vec![],
        artifacts: vec![],
        tests: None,
    }
}

#[test]
fn a_lint_pass_never_verifies_a_task_whose_tests_did_not_pass() {
    use CheckKind::*;
    use Declarer::*;
    use VerificationStatus::*;
    let p = InferredOnlyPolicy::default();
    for unit in [Blocked, Unverified] {
        let (s, lim) = aggregate(
            &mut [],
            &[
                result("fmt", Lint, Harness, Passed),
                result("test", Unit, Harness, unit),
            ],
            p,
        );
        assert_eq!(s, Unverified, "{unit:?}");
        assert!(
            lim.iter()
                .any(|l| l.starts_with("planned test check not passed")),
            "{lim:?}"
        );
    }
    // no test planned: a lint pass still counts; a passed test completes
    assert_eq!(
        aggregate(&mut [], &[result("fmt", Lint, Harness, Passed)], p).0,
        Passed
    );
    let both = [
        result("fmt", Lint, Harness, Passed),
        result("test", Unit, Harness, Passed),
    ];
    assert_eq!(aggregate(&mut [], &both, p).0, Passed);
    // a user criterion that passed does not excuse a planned test that did not run
    let mut c = vec![Criterion::user("c1", "", None)];
    let mut user = result("u", Custom, User, Passed);
    user.criteria_ids = vec!["c1".into()];
    let (s, _) = aggregate(&mut c, &[user, result("test", Unit, Harness, Blocked)], p);
    assert_eq!(s, Unverified);
}

#[test]
fn model_declared_checks_alone_never_pass() {
    use CheckKind::*;
    use Declarer::*;
    use VerificationStatus::*;
    let p = InferredOnlyPolicy::default();
    let (s, lim) = aggregate(&mut [], &[result("m1", Custom, Model, Passed)], p);
    assert_eq!(s, Unverified);
    assert!(
        lim.iter()
            .any(|l| l.starts_with("model-declared checks only")),
        "{lim:?}"
    );
    let mixed = [
        result("m1", Custom, Model, Passed),
        result("fmt", Lint, Harness, Passed),
    ];
    assert_eq!(aggregate(&mut [], &mixed, p).0, Passed);
    // a model's unit check that did not pass does not block a planned pass
    let blocked = [
        result("m1", Unit, Model, Blocked),
        result("fmt", Lint, Harness, Passed),
    ];
    assert_eq!(aggregate(&mut [], &blocked, p).0, Passed);
}

fn block(prefixes: &[&'static str]) -> Policy {
    Policy(
        prefixes
            .iter()
            .map(|p| (*p, CheckAuth::Blocked("approval denied".into())))
            .collect(),
    )
}

async fn eval_with(
    g: &mut CompletionGate,
    st: &TaskState,
    auth: Option<&dyn CheckAuthorizer>,
) -> GateDecision {
    g.evaluate_with(st, true, &CancellationToken::new(), auth)
        .await
}

#[tokio::test]
async fn fmt_passes_while_cargo_test_is_blocked_ends_unverified() {
    if !have("cargo") {
        return;
    }
    let f = fx(&[
        (
            "Cargo.toml",
            "[package]\nname=\"fx\"\nversion=\"0.1.0\"\nedition=\"2021\"\n[lib]\npath=\"lib.rs\"\n",
        ),
        ("lib.rs", "#[test]\nfn t() {\n    assert_eq!(1, 1);\n}\n"),
    ]);
    let mut g = f.gate(GateConfig::default());
    g.begin_now();
    let auth = block(&["cargo test", "cargo clippy"]);
    let r = unverified(eval_with(&mut g, &state(vec![], &["lib.rs"]), Some(&auth)).await);
    let fmt = r
        .checks
        .iter()
        .find(|k| k.command.as_deref() == Some("cargo fmt --check"))
        .unwrap();
    assert_eq!(fmt.status, VerificationStatus::Passed, "{}", fmt.summary);
    assert_eq!(r.status, VerificationStatus::Unverified);
    assert!(
        r.summary().contains("planned test check not passed"),
        "{}",
        r.summary()
    );
}

fn pkg_fixture(extra: &[(&str, &str)]) -> Fx {
    let mut files = vec![("package.json", r#"{"scripts":{"lint":"x"}}"#)];
    files.extend_from_slice(extra);
    fx(&files)
}

/// A user criterion whose check passes on any host (no tool needed).
fn cd_criteria() -> Vec<Criterion> {
    vec![Criterion::user("c1", "ok", Some("hostname"))]
}

#[tokio::test]
async fn changing_a_check_definition_during_the_task_caps_at_unverified() {
    let auth = block(&["npm"]);
    let f = pkg_fixture(&[]);
    let mut g = f.gate(GateConfig::default());
    g.begin_now();
    let d = eval_with(&mut g, &state(cd_criteria(), &[]), Some(&auth)).await;
    assert!(
        matches!(d, GateDecision::Complete(Some(_))),
        "control: {d:?}"
    );

    let f = pkg_fixture(&[]);
    let mut g = f.gate(GateConfig::default());
    g.begin_now();
    fs::write(
        f.ws.cwd.join("package.json"),
        r#"{"scripts":{"lint":"true"}}"#,
    )
    .unwrap();
    let r = unverified(eval_with(&mut g, &state(cd_criteria(), &[]), Some(&auth)).await);
    assert!(
        r.limitations
            .iter()
            .any(|l| l.starts_with("check definitions changed") && l.contains("package.json")),
        "{:?}",
        r.limitations
    );
    assert!(r.summary().contains("check definitions changed"));
}

#[tokio::test]
async fn shell_made_test_changes_are_seen_without_any_edit_record() {
    let t = "tests/a.test.js";
    let body = "test('a', () => {\n  assert.equal(add(1, 1), 2);\n  assert.equal(add(2, 2), 4);\n});\ntest('b', () => {});\n";
    type Case = (&'static str, fn(&Path), &'static str);
    let cases: [Case; 5] = [
        (
            "rm",
            |p| fs::remove_file(p.join("tests/a.test.js")).unwrap(),
            "deleted or renamed",
        ),
        (
            "mv",
            |p| fs::rename(p.join("tests/a.test.js"), p.join("tests/b.js")).unwrap(),
            "deleted or renamed",
        ),
        (
            "truncate",
            |p| fs::write(p.join("tests/a.test.js"), "").unwrap(),
            "truncated",
        ),
        (
            "sed",
            |p| {
                let f = p.join("tests/a.test.js");
                let s = fs::read_to_string(&f)
                    .unwrap()
                    .replace("assert.equal(add(2, 2), 4);", "");
                fs::write(f, s).unwrap();
            },
            "removed assertions",
        ),
        (
            "sed-expected-value",
            |p| {
                let f = p.join("tests/a.test.js");
                let s = fs::read_to_string(&f)
                    .unwrap()
                    .replace("add(2, 2), 4)", "add(2, 2), 0)");
                fs::write(f, s).unwrap();
            },
            "existing assertions changed",
        ),
    ];
    for (name, act, want) in cases {
        let f = fx(&[]);
        fs::create_dir(f.ws.cwd.join("tests")).unwrap();
        fs::write(f.ws.cwd.join(t), body).unwrap();
        let mut g = f.gate(GateConfig::default());
        g.begin_now();
        let d = eval_with(&mut g, &state(cd_criteria(), &[]), None).await;
        assert!(
            matches!(d, GateDecision::Complete(Some(_))),
            "{name} control: {d:?}"
        );
        act(&f.ws.cwd);
        let r = unverified(eval_with(&mut g, &state(cd_criteria(), &[]), None).await);
        let lim = r.limitations.join("|");
        assert!(
            lim.contains("test files modified") && lim.contains(want),
            "{name}: {lim}"
        );
    }
}

#[tokio::test]
async fn no_op_checks_are_unverified() {
    // a user criterion proven by `echo` proves nothing
    let f = fx(&[]);
    let c = vec![Criterion::user("c1", "ok", Some("echo all good"))];
    let r = unverified(eval_with(&mut f.gate(GateConfig::default()), &state(c, &[]), None).await);
    assert!(
        r.limitations.iter().any(|l| l.starts_with("no-op check")),
        "{:?}",
        r.limitations
    );
    // `"test": "true"` in package.json is the same
    if !have("npm") {
        return;
    }
    let f = fx(&[("package.json", r#"{"scripts":{"test":"true"}}"#)]);
    let r = unverified(
        eval_with(
            &mut f.gate(GateConfig::default()),
            &state(vec![], &["a.js"]),
            None,
        )
        .await,
    );
    assert_eq!(r.status, VerificationStatus::Unverified);
    assert!(
        r.limitations.iter().any(|l| l.starts_with("no-op check")),
        "{:?}",
        r.limitations
    );
}

#[tokio::test]
async fn a_check_failing_then_passing_is_flaky_and_stays_failed() {
    if !have("node") {
        return;
    }
    let js = "const fs=require('fs');if(fs.existsSync('seen')){process.exit(0)}fs.writeFileSync('seen','1');console.log('boom');process.exit(1);";
    let f = fx(&[("flaky.js", js)]);
    let mut st = state(vec![], &["x.rs"]);
    st.model_checks = vec![check(&f, "m1", "node flaky.js", 60_000)];
    let mut g = f.gate(GateConfig::default());
    // failed once, passed on the re-run: reported as failed, with the flake named
    let r = unverified(eval_with(&mut g, &st, None).await);
    assert_eq!(r.status, VerificationStatus::Failed);
    assert!(r.summary().contains("flaky"), "{}", r.summary());
    // later it passes 3 runs in a row: not blocked forever, but the earlier flake is on record
    let r = unverified(eval_with(&mut g, &st, None).await);
    assert!(
        r.limitations
            .iter()
            .any(|l| l.contains("passed 3 runs in a row")),
        "{:?}",
        r.limitations
    );
}

#[tokio::test]
async fn a_model_check_with_a_planned_command_cannot_stand_in_for_it() {
    let f = fx(&[("package.json", r#"{"scripts":{"test":"node --test"}}"#)]);
    let mut g = f.gate(GateConfig::default());
    g.begin_now();
    let mut st = state(vec![], &["a.js"]);
    st.model_checks = vec![check(&f, "m1", "npm run test", 60_000)];
    let auth = block(&["npm"]);
    let r = unverified(eval_with(&mut g, &st, Some(&auth)).await);
    assert_eq!(r.status, VerificationStatus::Unverified);
    assert!(
        r.limitations
            .iter()
            .any(|l| l.starts_with("planned test check not passed")),
        "{:?}",
        r.limitations
    );
    assert!(
        r.checks.iter().all(|k| k.declared_by != Declarer::Model),
        "model duplicate was planned"
    );
}

#[tokio::test]
async fn inline_rust_tests_and_ignore_in_src_are_seen() {
    let lib = "pub fn f() -> u8 { 1 }\n#[cfg(test)]\nmod t {\n    #[test]\n    fn a() {\n        assert_eq!(super::f(), 1);\n    }\n}\n";
    for (name, new) in [
        (
            "deleted assert",
            "pub fn f() -> u8 { 1 }\n#[cfg(test)]\nmod t {\n    #[test]\n    fn a() {\n    }\n}\n",
        ),
        (
            "ignore",
            "pub fn f() -> u8 { 1 }\n#[cfg(test)]\nmod t {\n    #[test]\n    #[ignore]\n    fn a() {\n        assert_eq!(super::f(), 1);\n    }\n}\n",
        ),
    ] {
        let f = fx(&[]);
        fs::create_dir(f.ws.cwd.join("src")).unwrap();
        fs::write(f.ws.cwd.join("src/lib.rs"), lib).unwrap();
        let mut g = f.gate(GateConfig::default());
        g.begin_now();
        fs::write(f.ws.cwd.join("src/lib.rs"), new).unwrap();
        let r = unverified(eval_with(&mut g, &state(cd_criteria(), &[]), None).await);
        let lim = r.limitations.join("|");
        assert!(
            lim.contains("test files modified") && lim.contains("src/lib.rs"),
            "{name}: {lim}"
        );
    }
}

#[tokio::test]
async fn a_planned_test_that_counts_no_tests_is_unverified() {
    if !have("npm") {
        return;
    }
    let f = fx(&[(
        "package.json",
        r#"{"scripts":{"test":"node -e \"console.log(1)\""}}"#,
    )]);
    let r = unverified(
        eval_with(
            &mut f.gate(GateConfig::default()),
            &state(vec![], &["a.js"]),
            None,
        )
        .await,
    );
    assert_eq!(r.status, VerificationStatus::Unverified);
    assert!(
        r.limitations
            .iter()
            .any(|l| l.contains("counts no passing test")),
        "{:?}",
        r.limitations
    );
}

#[tokio::test]
async fn a_shell_that_only_touches_files_still_triggers_the_gate() {
    let f = fx(&[]);
    let mut g = f.gate(GateConfig::default());
    g.begin_now();
    let mut st = state(vec![], &[]);
    st.side_effects = false;
    let d = eval_with(&mut g, &st, None).await;
    assert!(matches!(d, GateDecision::Complete(None)), "control: {d:?}");
    fs::write(f.ws.cwd.join("made-by-an-inert-looking-command.txt"), "x").unwrap();
    let d = eval_with(&mut g, &st, None).await;
    assert!(!matches!(d, GateDecision::Complete(None)), "{d:?}");
}

#[tokio::test]
async fn changes_from_earlier_tasks_do_not_make_a_question_need_the_gate() {
    let f = fx(&[]);
    let mut g = f.gate(GateConfig::default());
    g.begin_now();
    fs::write(f.ws.cwd.join("earlier.txt"), "x").unwrap();
    g.set_preexisting(vec!["earlier.txt".into()]);
    let mut st = state(vec![], &[]);
    st.side_effects = false;
    let d = eval_with(&mut g, &st, None).await;
    assert!(matches!(d, GateDecision::Complete(None)), "{d:?}");
}
