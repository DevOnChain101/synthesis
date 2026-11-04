//! Sandbox integration. Real enforcement runs on macOS (Seatbelt); the fail-closed / mode logic
//! uses a fake backend and runs on every unix.
#![cfg(unix)]

use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;
use std::sync::Arc;

use synthesis_core::WorkspaceContext;
use synthesis_sandbox::{
    Guarantee, SandboxBackend, SandboxCapabilities, SandboxError, SandboxPolicy, SandboxedCommand,
};
use synthesis_terminal::{Error, ExecSpec, SandboxMode, SpawnSpec, TerminalRuntime};
use tokio_util::sync::CancellationToken;

const SID: &str = "s";

/// Dirs live under target/, not the system temp dir (which Seatbelt always makes writable).
struct Fx {
    _dir: tempfile::TempDir,
    root: PathBuf,
    outside: PathBuf,
}

fn open(rt: &TerminalRuntime, session_policy: Option<SandboxPolicy>) -> Fx {
    let dir = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let base = fs::canonicalize(dir.path()).unwrap();
    let (root, outside) = (base.join("w"), base.join("out"));
    fs::create_dir_all(&root).unwrap();
    fs::create_dir_all(&outside).unwrap();
    let ws = WorkspaceContext::open("w", &root, &root, &[], base.join("art"), "pg").unwrap();
    let env = BTreeMap::from([("PATH".to_string(), std::env::var("PATH").unwrap())]);
    let policy = session_policy.map(|mut p| {
        p.writable_roots.push(root.clone());
        p
    });
    rt.open_session_sandboxed(SID, Arc::new(ws), env, policy);
    Fx {
        _dir: dir,
        root,
        outside,
    }
}

async fn run(rt: &TerminalRuntime, spec: ExecSpec) -> synthesis_terminal::ExecResult {
    rt.exec(SID, spec, &CancellationToken::new()).await.unwrap()
}

struct Fake(Guarantee);

impl SandboxBackend for Fake {
    fn name(&self) -> &'static str {
        "fake"
    }
    fn capabilities(&self) -> SandboxCapabilities {
        SandboxCapabilities {
            filesystem: self.0,
            network: self.0,
            process_tree: self.0,
            gui: self.0,
            notes: vec![],
        }
    }
    fn wrap(
        &self,
        _: &SandboxPolicy,
        c: &SandboxedCommand,
    ) -> Result<SandboxedCommand, SandboxError> {
        Ok(c.clone())
    }
}

struct NoWrap;

impl SandboxBackend for NoWrap {
    fn name(&self) -> &'static str {
        "nowrap"
    }
    fn capabilities(&self) -> SandboxCapabilities {
        Fake(Guarantee::Strong).capabilities()
    }
    fn wrap(
        &self,
        _: &SandboxPolicy,
        _: &SandboxedCommand,
    ) -> Result<SandboxedCommand, SandboxError> {
        Err(SandboxError::Unsupported("landlock only".into()))
    }
}

#[tokio::test]
async fn unsupported_wrap_degrades_in_best_effort_but_enforce_refuses() {
    let rt = TerminalRuntime::new(SandboxMode::BestEffort).with_sandbox_backend(Arc::new(NoWrap));
    let f = open(&rt, Some(SandboxPolicy::default()));
    let r = run(&rt, ExecSpec::new("touch ran")).await;
    assert!(r.sandbox.is_none() && r.unsandboxed_by_request && f.root.join("ran").exists());
    let rt = TerminalRuntime::new(SandboxMode::Enforce).with_sandbox_backend(Arc::new(NoWrap));
    let f = open(&rt, Some(SandboxPolicy::default()));
    let err = rt
        .exec(SID, ExecSpec::new("touch ran"), &CancellationToken::new())
        .await
        .unwrap_err();
    assert!(matches!(err, Error::SandboxUnavailable(_)), "{err}");
    assert!(!f.root.join("ran").exists());
}

#[tokio::test]
async fn best_effort_degrades_to_next_backend_then_unconfined_enforce_stays_on_first() {
    let chain = || -> Vec<Arc<dyn SandboxBackend>> {
        vec![Arc::new(NoWrap), Arc::new(Fake(Guarantee::Weak))]
    };
    let rt = TerminalRuntime::new(SandboxMode::BestEffort).with_sandbox_chain(chain());
    let _f = open(&rt, Some(SandboxPolicy::default()));
    let r = run(&rt, ExecSpec::new("true")).await;
    assert!(
        r.sandbox.is_some() && !r.unsandboxed_by_request,
        "second backend confines"
    );
    let rt = TerminalRuntime::new(SandboxMode::BestEffort)
        .with_sandbox_chain(vec![Arc::new(NoWrap), Arc::new(NoWrap)]);
    let _f = open(&rt, Some(SandboxPolicy::default()));
    let r = run(&rt, ExecSpec::new("true")).await;
    assert!(
        r.sandbox.is_none() && r.unsandboxed_by_request,
        "all failed: unconfined"
    );
    let rt = TerminalRuntime::new(SandboxMode::Enforce).with_sandbox_chain(chain());
    let _f = open(&rt, Some(SandboxPolicy::default()));
    let err = rt
        .exec(SID, ExecSpec::new("true"), &CancellationToken::new())
        .await
        .unwrap_err();
    assert!(
        err.to_string().contains("landlock only"),
        "precise reason: {err}"
    );
}

#[tokio::test]
async fn enforce_refuses_loopback_the_backend_cannot_honour() {
    // Fake keeps the trait default: Deny = capabilities().network, Loopback = unsupported.
    let rt = TerminalRuntime::new(SandboxMode::Enforce)
        .with_sandbox_backend(Arc::new(Fake(Guarantee::Strong)));
    let f = open(&rt, None);
    let with = |network| {
        let mut spec = ExecSpec::new("touch ran");
        spec.sandbox = Some(SandboxPolicy {
            network,
            ..Default::default()
        });
        spec
    };
    let err = rt
        .exec(
            SID,
            with(synthesis_sandbox::NetworkPolicy::Loopback),
            &CancellationToken::new(),
        )
        .await
        .unwrap_err();
    assert!(matches!(err, Error::SandboxUnavailable(_)), "{err}");
    assert!(!f.root.join("ran").exists());
    run(&rt, with(synthesis_sandbox::NetworkPolicy::Deny)).await;
    assert!(f.root.join("ran").exists());
}

#[tokio::test]
async fn required_guarantee_not_met_runs_nothing() {
    let rt = TerminalRuntime::new(SandboxMode::Enforce)
        .with_sandbox_backend(Arc::new(Fake(Guarantee::Weak)));
    let f = open(&rt, None);
    let policy = |required| SandboxPolicy {
        required,
        ..Default::default()
    };
    let mut spec = ExecSpec::new("touch ran");
    spec.sandbox = Some(policy(Guarantee::Strong));
    let err = rt
        .exec(SID, spec, &CancellationToken::new())
        .await
        .unwrap_err();
    assert!(matches!(err, Error::SandboxUnavailable(_)), "{err}");
    assert!(!f.root.join("ran").exists());
    let mut sp = SpawnSpec::new("touch ran");
    sp.sandbox = Some(policy(Guarantee::Strong));
    assert!(matches!(
        rt.spawn(SID, sp).await,
        Err(Error::SandboxUnavailable(_))
    ));
    // Best effort runs and reports what it got.
    let mut spec = ExecSpec::new("touch ran");
    spec.sandbox = Some(policy(Guarantee::Weak));
    let r = run(&rt, spec).await;
    assert_eq!(r.sandbox.unwrap().filesystem, Guarantee::Weak);
    assert!(f.root.join("ran").exists());
}

#[tokio::test]
async fn off_mode_runs_unconfined_and_says_so() {
    let rt = TerminalRuntime::new(SandboxMode::Off);
    let f = open(&rt, None);
    let r = run(
        &rt,
        ExecSpec::new(format!("touch {}/x", f.outside.display())),
    )
    .await;
    assert_eq!(r.exit_code, Some(0));
    assert!(r.sandbox.is_none() && r.unsandboxed_by_request && f.outside.join("x").exists());
    let h = rt.spawn(SID, SpawnSpec::new("sleep 5")).await.unwrap();
    let st = rt.get_status(SID, &h.process_id).unwrap();
    assert!(st.sandbox.is_none() && st.unsandboxed_by_request);
    // Off ignores a policy too, and still reports it.
    let mut spec = ExecSpec::new("true");
    spec.sandbox = Some(SandboxPolicy::default());
    assert!(run(&rt, spec).await.unsandboxed_by_request);
}

/// (M2) fail-open fix: a policy with a backend that cannot confine must not run unconfined.
#[tokio::test]
async fn enforce_refuses_unusable_backend_even_when_policy_asks_for_nothing() {
    for required in [Guarantee::None, Guarantee::Weak] {
        let rt = TerminalRuntime::new(SandboxMode::Enforce)
            .with_sandbox_backend(Arc::new(Fake(Guarantee::None)));
        let f = open(
            &rt,
            Some(SandboxPolicy {
                required,
                ..Default::default()
            }),
        );
        let err = rt
            .exec(SID, ExecSpec::new("touch ran"), &CancellationToken::new())
            .await
            .unwrap_err();
        assert!(matches!(err, Error::SandboxUnavailable(_)), "{err}");
        assert!(matches!(
            rt.spawn(SID, SpawnSpec::new("touch ran")).await,
            Err(Error::SandboxUnavailable(_))
        ));
        assert!(!f.root.join("ran").exists(), "{required:?}");
    }
}

#[tokio::test]
async fn default_policy_requires_weak_and_default_mode_is_enforce() {
    assert_eq!(SandboxPolicy::default().required, Guarantee::Weak);
    assert_eq!(TerminalRuntime::default().mode(), SandboxMode::Enforce);
    let rt = TerminalRuntime::new(SandboxMode::Enforce)
        .with_sandbox_backend(Arc::new(Fake(Guarantee::None)));
    let _f = open(&rt, Some(SandboxPolicy::default()));
    assert!(
        rt.exec(SID, ExecSpec::new("true"), &CancellationToken::new())
            .await
            .is_err()
    );
}

#[tokio::test]
async fn enforce_without_any_policy_runs_nothing() {
    let rt = TerminalRuntime::new(SandboxMode::Enforce)
        .with_sandbox_backend(Arc::new(Fake(Guarantee::Strong)));
    let f = open(&rt, None);
    let err = rt
        .exec(SID, ExecSpec::new("touch ran"), &CancellationToken::new())
        .await
        .unwrap_err();
    assert!(matches!(err, Error::SandboxUnavailable(_)), "{err}");
    assert!(!f.root.join("ran").exists());
}

#[tokio::test]
async fn best_effort_runs_unconfined_only_when_backend_cannot_confine() {
    let rt = TerminalRuntime::new(SandboxMode::Enforce)
        .allow_unsandboxed(true)
        .with_sandbox_backend(Arc::new(Fake(Guarantee::None)));
    assert_eq!(rt.mode(), SandboxMode::BestEffort);
    let f = open(&rt, Some(SandboxPolicy::default()));
    let r = run(&rt, ExecSpec::new("touch ran")).await;
    assert!(r.sandbox.is_none() && r.unsandboxed_by_request && f.root.join("ran").exists());
    let h = rt.spawn(SID, SpawnSpec::new("sleep 5")).await.unwrap();
    assert!(
        rt.get_status(SID, &h.process_id)
            .unwrap()
            .unsandboxed_by_request
    );
    // A backend that can confine, but not enough, is still an error (no silent downgrade).
    let rt = TerminalRuntime::new(SandboxMode::BestEffort)
        .with_sandbox_backend(Arc::new(Fake(Guarantee::Weak)));
    let _f = open(&rt, None);
    let mut spec = ExecSpec::new("true");
    spec.sandbox = Some(SandboxPolicy {
        required: Guarantee::Strong,
        ..Default::default()
    });
    assert!(matches!(
        rt.exec(SID, spec, &CancellationToken::new()).await,
        Err(Error::SandboxUnavailable(_))
    ));
    assert_eq!(
        TerminalRuntime::new(SandboxMode::Enforce)
            .allow_unsandboxed(true)
            .allow_unsandboxed(false)
            .mode(),
        SandboxMode::Enforce
    );
}

#[tokio::test]
async fn sandboxed_run_is_not_flagged_and_private_tmp_is_created() {
    let rt = TerminalRuntime::new(SandboxMode::Enforce)
        .with_sandbox_backend(Arc::new(Fake(Guarantee::Weak)));
    let f = open(
        &rt,
        Some(SandboxPolicy {
            tmp_dir: Some(
                PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("synthesis-priv-tmp-test"),
            ),
            ..Default::default()
        }),
    );
    let r = run(&rt, ExecSpec::new("true")).await;
    assert!(
        r.sandbox.is_some() && !r.unsandboxed_by_request,
        "{:?}",
        f.root
    );
    assert!(
        PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
            .join("synthesis-priv-tmp-test")
            .is_dir()
    );
}

#[cfg(target_os = "macos")]
mod seatbelt {
    use super::*;
    use std::path::Path;
    use std::time::Duration;
    use synthesis_sandbox::NetworkPolicy;
    use synthesis_terminal::Readiness;

    fn policy(network: NetworkPolicy) -> SandboxPolicy {
        SandboxPolicy {
            network,
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn exec_write_inside_ok_outside_denied_session_default() {
        let rt = TerminalRuntime::new(SandboxMode::Enforce);
        let f = open(&rt, Some(policy(NetworkPolicy::Deny)));
        let r = run(&rt, ExecSpec::new("echo hi > in.txt")).await;
        assert_eq!(r.exit_code, Some(0), "{}", r.output);
        assert!(f.root.join("in.txt").exists());
        let out = f.outside.join("o.txt");
        let r = run(&rt, ExecSpec::new(format!("echo hi > '{}'", out.display()))).await;
        assert_ne!(r.exit_code, Some(0));
        assert!(!out.exists());
        let caps = r.sandbox.expect("capabilities reported");
        assert_eq!(caps.process_tree, Guarantee::Strong);
    }

    #[tokio::test]
    async fn per_call_policy_overrides_session_default() {
        let rt = TerminalRuntime::new(SandboxMode::Enforce);
        let f = open(&rt, None);
        let mut spec = ExecSpec::new("echo hi > in.txt");
        spec.sandbox = Some(SandboxPolicy {
            writable_roots: vec![f.root.clone()],
            ..Default::default()
        });
        assert_eq!(run(&rt, spec).await.exit_code, Some(0));
    }

    #[tokio::test]
    async fn network_denied_then_allowed() {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        let cmd = format!("/usr/bin/nc -z -w 2 127.0.0.1 {port}");
        for (net, ok) in [(NetworkPolicy::Deny, false), (NetworkPolicy::Allow, true)] {
            let rt = TerminalRuntime::new(SandboxMode::Enforce);
            let _f = open(&rt, Some(policy(net)));
            let r = run(&rt, ExecSpec::new(&cmd)).await;
            assert_eq!(r.exit_code == Some(0), ok, "{}", r.output);
        }
    }

    #[tokio::test]
    async fn stdin_and_separate_stderr_work_when_wrapped() {
        let rt = TerminalRuntime::new(SandboxMode::Enforce);
        let _f = open(&rt, Some(policy(NetworkPolicy::Deny)));
        let mut spec = ExecSpec::new("cat; echo e >&2");
        spec.stdin = Some(b"in".to_vec());
        spec.stderr = synthesis_terminal::StderrMode::Separate;
        let r = run(&rt, spec).await;
        assert_eq!((r.output.as_str(), r.stderr.trim()), ("in", "e"));
    }

    #[tokio::test]
    async fn spawned_service_is_confined() {
        let rt = TerminalRuntime::new(SandboxMode::Enforce);
        let f = open(&rt, Some(policy(NetworkPolicy::Deny)));
        let out = f.outside.join("svc.txt");
        let mut spec = SpawnSpec::new(format!("echo x > '{}'; echo done; sleep 30", out.display()));
        spec.ready = Some(Readiness::LogPattern("done".into()));
        let h = rt.spawn(SID, spec).await.unwrap();
        assert!(h.ready && !out.exists());
        let log = rt.read_logs(SID, &h.process_id, None, 4096).unwrap().data;
        assert!(log.contains("not permitted"), "{log}");
        let st = rt.get_status(SID, &h.process_id).unwrap();
        assert!(st.running && st.sandbox.is_some());
        rt.close_session(SID);
    }

    fn alive(pid: u32) -> bool {
        std::process::Command::new("kill")
            .args(["-0", &pid.to_string()])
            .stderr(std::process::Stdio::null())
            .status()
            .unwrap()
            .success()
    }

    #[tokio::test]
    async fn timeout_kills_sandboxed_tree() {
        let rt = TerminalRuntime::new(SandboxMode::Enforce);
        let f = open(&rt, Some(policy(NetworkPolicy::Deny)));
        let mut spec = ExecSpec::new("sleep 60 & echo $! > pid; wait");
        spec.timeout = Duration::from_millis(500);
        let r = run(&rt, spec).await;
        assert!(r.timed_out);
        let pid: u32 = fs::read_to_string(f.root.join("pid"))
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        for _ in 0..300 {
            if !alive(pid) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("sandboxed grandchild {pid} survived the timeout");
    }

    /// `SandboxPolicy::standard` through the real runtime: writes in the workspace and the
    /// private tmp, not in .git/hooks or .git/config, not in the system temp dirs.
    #[tokio::test]
    async fn standard_policy_end_to_end() {
        let rt = TerminalRuntime::new(SandboxMode::Enforce);
        let f = open(&rt, None);
        let tmp = f.root.parent().unwrap().join("priv-tmp");
        fs::create_dir_all(f.root.join(".git/hooks")).unwrap();
        fs::write(f.root.join(".git/config"), "[core]\n").unwrap();
        let ws = WorkspaceContext::open(
            "w",
            &f.root,
            &f.root,
            &[],
            f.root.parent().unwrap().join("art"),
            "pg",
        )
        .unwrap();
        let env = BTreeMap::from([("PATH".to_string(), std::env::var("PATH").unwrap())]);
        let policy = SandboxPolicy::standard(&f.root, &tmp);
        rt.open_session_sandboxed(SID, Arc::new(ws), env, Some(policy));
        let ok = run(
            &rt,
            ExecSpec::new("echo a > in.txt && echo b > \"$TMPDIR/t.txt\" && cat .git/config"),
        )
        .await;
        assert_eq!(ok.exit_code, Some(0), "{}", ok.output);
        assert!(f.root.join("in.txt").exists() && tmp.join("t.txt").exists());
        assert!(!ok.unsandboxed_by_request);
        for bad in [
            "echo x >> .git/config",
            "echo x > .git/hooks/pre-commit",
            "mv .git .git2",
            "echo x > /private/tmp/synthesis-std-test",
        ] {
            assert_ne!(
                run(&rt, ExecSpec::new(bad)).await.exit_code,
                Some(0),
                "{bad}"
            );
        }
        assert!(
            !f.root.join(".git/hooks/pre-commit").exists()
                && !Path::new("/private/tmp/synthesis-std-test").exists()
        );
    }
    /// Default policy + real HOME toolchains: `cargo test` and `npm test` in a throwaway project
    /// run inside the sandbox, while credential dirs stay unreadable. Missing tools are skipped.
    #[tokio::test]
    async fn toolchains_under_home_run_and_secrets_stay_hidden() {
        let home = PathBuf::from(std::env::var("HOME").unwrap());
        let rt = TerminalRuntime::new(SandboxMode::Enforce);
        let f = open(&rt, None);
        let tmp = f.root.parent().unwrap().join("priv-tmp2");
        let ws = WorkspaceContext::open("w", &f.root, &f.root, &[], f.root.join("../art2"), "pg")
            .unwrap();
        let env = BTreeMap::from([
            ("PATH".to_string(), std::env::var("PATH").unwrap()),
            ("HOME".to_string(), home.display().to_string()),
        ]);
        let policy = SandboxPolicy::standard(&f.root, &tmp);
        rt.open_session_sandboxed(SID, Arc::new(ws), env, Some(policy));
        let has = |c: &str| {
            std::process::Command::new("/bin/sh")
                .args(["-c", &format!("command -v {c}")])
                .env("PATH", std::env::var("PATH").unwrap())
                .output()
                .is_ok_and(|o| o.status.success())
        };
        if has("cargo") {
            let w = f.root.join("rs");
            fs::create_dir_all(w.join("src")).unwrap();
            fs::write(
                w.join("Cargo.toml"),
                "[package]\nname=\"t\"\nversion=\"0.1.0\"\nedition=\"2021\"\n[workspace]\n",
            )
            .unwrap();
            fs::write(
                w.join("src/lib.rs"),
                "#[test] fn ok() { assert_eq!(1 + 1, 2); }\n",
            )
            .unwrap();
            let r = run(&rt, ExecSpec::new("cd rs && cargo test --offline 2>&1")).await;
            assert_eq!(r.exit_code, Some(0), "{}", r.output);
            assert!(r.output.contains("1 passed"), "{}", r.output);
        } else {
            eprintln!("skipped: cargo not installed");
        }
        if has("npm") {
            let w = f.root.join("js");
            fs::create_dir_all(&w).unwrap();
            fs::write(w.join("package.json"), r#"{"name":"t","version":"1.0.0","scripts":{"test":"node -e \"console.log('js-ok')\""}}"#).unwrap();
            let r = run(&rt, ExecSpec::new("cd js && npm test 2>&1")).await;
            assert_eq!(r.exit_code, Some(0), "{}", r.output);
            assert!(r.output.contains("js-ok"), "{}", r.output);
        } else {
            eprintln!("skipped: npm not installed");
        }
        for secret in [
            ".ssh",
            ".aws",
            ".gnupg",
            "Library/Keychains",
            ".config/gh",
            ".cargo/credentials.toml",
        ] {
            let p = home.join(secret);
            if p.exists() {
                let r = run(
                    &rt,
                    ExecSpec::new(format!("ls '{0}' || cat '{0}'", p.display())),
                )
                .await;
                assert_ne!(r.exit_code, Some(0), "{secret} readable: {}", r.output);
            }
        }
    }
}
