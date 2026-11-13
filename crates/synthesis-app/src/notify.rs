//! Optional notifications for unattended runs (docs/UNATTENDED.md, "Notifications").
//!
//! `{"synt":{"notify":{"command":["/path/to/prog","arg"],"timeout_secs":10,"network":false,"include_text":false}}}` in
//! the USER or MANAGED settings (managed wins as a whole; a project file is never read). The
//! command gets one JSON event on stdin: `task.ended` (any status), `approval.blocked` (a headless
//! run denied an ask; once per task) and `budget.exceeded`. It runs through the sandboxed terminal
//! runtime in its own session with a scrubbed env (PATH/locale only: no API key, no `SYNT_*`), the
//! workspace as cwd, a short timeout, and at most 50 times per process (plus 50 more reserved for
//! `task.ended`). It is NOT started when the run is not sandboxed (`--sandbox off`, best-effort or
//! no policy): a command must never run unconfined on behalf of a run that asked for confinement.
//!
//! Threat model: the payload is built from the already redacted event stream and holds only ids,
//! the status, a reason CODE and the budget numbers (the redacted, truncated stop reason text only
//! with `include_text: true`); never the final
//! answer, tool input or output, file contents or the request. The command is the user's own
//! (project files cannot set it); it is not a shell string: every argv element is single-quoted.

use std::collections::{BTreeMap, HashSet};
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};
use synthesis_core::WorkspaceContext;
use synthesis_events::{Envelope, Event};
use synthesis_harness::EventSink;
use synthesis_terminal::{ExecSpec, SandboxPolicy, StderrMode, TerminalRuntime};
use tokio_util::sync::CancellationToken;

/// Most notifications sent by one process (a runaway approval loop cannot flood a channel).
const MAX_SENT: usize = 50;
/// Reserved for `task.ended`, so a flood of approvals can never crowd the final event out.
const MAX_ENDED: usize = 50;
/// Commands running at once; the rest wait for a slot (so a burst is not a fork bomb).
const MAX_CONCURRENT: usize = 4;
/// Longest `flush` waits for all notifications together.
const DRAIN_WAIT: Duration = Duration::from_secs(10);
const MAX_REASON: usize = 200;
const DEFAULT_TIMEOUT_SECS: u64 = 10;
const MAX_TIMEOUT_SECS: u64 = 60;
const MAX_ARGS: usize = 64;
const MAX_ARG_LEN: usize = 4096;
/// Env names a notifier may see: no credentials, no `SYNT_*`.
const ENV_ALLOW: [&str; 8] = [
    "PATH",
    "HOME",
    "LANG",
    "LC_ALL",
    "TZ",
    "SYSTEMROOT",
    "TMPDIR",
    "TEMP",
];

#[derive(Debug, Clone, PartialEq)]
pub enum Kind {
    Command(Vec<String>),
    /// The desktop app's own notification. The app has no notifier today, so this is skipped.
    Desktop,
}

#[derive(Debug, Clone, PartialEq)]
pub struct NotifyConfig {
    pub kind: Kind,
    pub timeout: Duration,
    /// Lets the command reach the network (a webhook); off by default.
    pub network: bool,
    /// Send the redacted stop reason text too (default: a reason code only).
    pub include_text: bool,
}

/// `synt.notify` from the user file, then the managed file (which replaces it as a whole). A wrong
/// shape is an error naming the key, never a silent "off".
pub fn read_config(home: &Path, managed: &Path) -> Result<Option<NotifyConfig>, String> {
    let mut out = None;
    for (path, what) in [
        (home.join("settings.json"), "user settings"),
        (managed.to_path_buf(), "managed settings"),
    ] {
        let Some(doc) = synthesis_settings::read_json(&path, what)? else {
            continue;
        };
        if let Some(n) = doc.get("synt").and_then(|s| s.get("notify")) {
            out = Some(parse(n).map_err(|e| format!("{what}: synt.notify {e}"))?);
        }
    }
    Ok(out)
}

fn parse(n: &Value) -> Result<NotifyConfig, String> {
    let o = n.as_object().ok_or("must be an object")?;
    let timeout_secs = match o.get("timeout_secs") {
        None => DEFAULT_TIMEOUT_SECS,
        Some(v) => v
            .as_u64()
            .filter(|t| (1..=MAX_TIMEOUT_SECS).contains(t))
            .ok_or("timeout_secs must be 1..=60")?,
    };
    let network = match o.get("network") {
        None => false,
        Some(v) => v.as_bool().ok_or("network must be true or false")?,
    };
    let include_text = match o.get("include_text") {
        None => false,
        Some(v) => v.as_bool().ok_or("include_text must be true or false")?,
    };
    let kind = match o.get("kind").and_then(Value::as_str).unwrap_or("command") {
        "desktop" => Kind::Desktop,
        "command" => {
            let argv = o
                .get("command")
                .and_then(Value::as_array)
                .ok_or("command must be an array of strings (argv, no shell)")?;
            let argv: Vec<String> = argv
                .iter()
                .map(|a| {
                    a.as_str()
                        .filter(|s| !s.is_empty() && !s.contains('\0') && s.len() <= MAX_ARG_LEN)
                        .map(str::to_string)
                })
                .collect::<Option<_>>()
                .ok_or("command holds an empty, too long or non-string element")?;
            if argv.is_empty() || argv.len() > MAX_ARGS {
                return Err(format!("command needs 1..={MAX_ARGS} elements"));
            }
            Kind::Command(argv)
        }
        _ => return Err("kind must be \"command\" or \"desktop\"".into()),
    };
    Ok(NotifyConfig {
        kind,
        timeout: Duration::from_secs(timeout_secs),
        network,
        include_text,
    })
}

/// The notify command runs only under an enforcing sandbox with a policy: with `--sandbox off`,
/// best-effort or a policy that could not be built it would run unconfined (and with the host's
/// network), which a run that did not ask for that must not get.
fn confined(mode: synthesis_terminal::SandboxMode, has_policy: bool) -> Result<(), String> {
    if mode != synthesis_terminal::SandboxMode::Enforce || !has_policy {
        return Err(
            "synt.notify needs an enforcing sandbox (not --sandbox off or best-effort, and a buildable policy); skipped".into(),
        );
    }
    Ok(())
}

/// POSIX single quoting: the shell sees one literal word, so nothing in an argument is expanded.
fn quote(a: &str) -> String {
    format!("'{}'", a.replace('\'', r"'\''"))
}

/// The JSON event for an envelope the notifier cares about; `None` for all others. `seen` holds
/// the tasks whose blocked approval was already reported.
pub fn payload(env: &Envelope, seen: &mut HashSet<String>, include_text: bool) -> Option<Value> {
    let short = |s: &str| s.chars().take(MAX_REASON).collect::<String>();
    let (event, detail) = match &env.event {
        Event::TaskEnded {
            status,
            reason,
            usage,
            ..
        } => ("task.ended", {
            let mut d = json!({"status": status, "reason_code": reason_code(reason), "cost_usd": usage.cost_usd});
            if include_text {
                d["reason"] = json!(short(reason));
            }
            d
        }),
        Event::ToolApprovalResolved {
            reviewer, decision, ..
        } if reviewer.starts_with("policy:non-interactive")
            && decision != "allow"
            && seen.insert(env.task_id.clone().unwrap_or_default()) =>
        {
            (
                "approval.blocked",
                json!({"reason": "an ask was denied: nobody can approve in this run"}),
            )
        }
        Event::BudgetExceeded {
            budget,
            limit,
            used,
            estimate,
            estimate_usd,
        } => (
            "budget.exceeded",
            // `used` is the actual spend; `estimate_usd` the refused call's worst case
            json!({"budget": short(budget), "limit": limit, "used": used, "estimate": estimate, "estimate_usd": estimate_usd}),
        ),
        _ => return None,
    };
    let mut v = json!({
        "event": event,
        "session_id": env.session_id,
        "task_id": env.task_id,
        "ts": env.ts,
    });
    if let (Some(m), Some(d)) = (v.as_object_mut(), detail.as_object()) {
        m.extend(d.clone());
    }
    Some(v)
}

/// A fixed vocabulary for the stop reason, so the default payload never carries free text.
fn reason_code(reason: &str) -> &'static str {
    const CODES: [(&str, &str); 9] = [
        ("budget_exceeded", "budget_exceeded"),
        ("cancelled", "cancelled"),
        ("no_progress", "no_progress"),
        ("model_error", "model_error"),
        ("context_error", "context_error"),
        ("verified", "verified"),
        ("unverified", "unverified"),
        ("needs_approval", "approval_unavailable"),
        ("event_sink_failed", "internal"),
    ];
    CODES
        .iter()
        .find(|(p, _)| reason.starts_with(p))
        .map_or("other", |(_, c)| c)
}

pub struct Notifier {
    argv: Vec<String>,
    timeout: Duration,
    rt: Arc<TerminalRuntime>,
    sid: String,
    include_text: bool,
    sent: AtomicUsize,
    ended: AtomicUsize,
    seen: Mutex<HashSet<String>>,
    pending: Mutex<Vec<std::thread::JoinHandle<()>>>,
    slots: (Mutex<usize>, std::sync::Condvar),
}

impl Notifier {
    /// Opens the notifier's own runtime session (scrubbed env, the run's sandbox policy, network
    /// per the setting) and taps `sink`. `Err` is a one-line reason the notifier is off.
    pub fn start(
        cfg: &NotifyConfig,
        rt: Arc<TerminalRuntime>,
        run_sid: &str,
        ws: Arc<WorkspaceContext>,
        run_env: &BTreeMap<String, String>,
        policy: Option<SandboxPolicy>,
        sink: &EventSink,
    ) -> Result<Arc<Self>, String> {
        confined(rt.mode(), policy.is_some())?;
        Self::start_unchecked(cfg, rt, run_sid, ws, run_env, policy, sink)
    }

    fn start_unchecked(
        cfg: &NotifyConfig,
        rt: Arc<TerminalRuntime>,
        run_sid: &str,
        ws: Arc<WorkspaceContext>,
        run_env: &BTreeMap<String, String>,
        policy: Option<SandboxPolicy>,
        sink: &EventSink,
    ) -> Result<Arc<Self>, String> {
        let Kind::Command(argv) = &cfg.kind else {
            return Err(
                "synt.notify kind \"desktop\" needs the desktop app's notifier, which does not exist; skipped".into(),
            );
        };
        if !rt.shell().is_posix() {
            return Err("synt.notify needs a POSIX shell (Git Bash on Windows); skipped".into());
        }
        let env: BTreeMap<String, String> = run_env
            .iter()
            .filter(|(k, _)| ENV_ALLOW.contains(&k.as_str()))
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        let policy = policy.map(|mut p| {
            // under `--network hosts` the commands' only way out is the run's proxy: the notifier
            // keeps it (its host must be on the allow list); `network: true` never widens that
            if cfg.network
                && matches!(
                    p.network,
                    synthesis_sandbox::NetworkPolicy::Deny
                        | synthesis_sandbox::NetworkPolicy::Loopback
                )
            {
                p.network = synthesis_sandbox::NetworkPolicy::Allow;
            }
            p
        });
        let sid = format!("{run_sid}-notify");
        rt.open_session_sandboxed(&sid, ws, env, policy);
        let n = Arc::new(Self {
            argv: argv.clone(),
            timeout: cfg.timeout,
            rt,
            sid,
            include_text: cfg.include_text,
            sent: AtomicUsize::new(0),
            ended: AtomicUsize::new(0),
            seen: Mutex::new(HashSet::new()),
            pending: Mutex::new(vec![]),
            slots: (Mutex::new(0), std::sync::Condvar::new()),
        });
        let me = n.clone();
        sink.observe(Arc::new(move |env| {
            let p = payload(
                env,
                &mut me.seen.lock().unwrap_or_else(|e| e.into_inner()),
                me.include_text,
            );
            if let Some(p) = p {
                me.send(p);
            }
        }));
        Ok(n)
    }

    /// Runs the command on its own thread so the run never waits for it (see [`Self::flush`]).
    /// `task.ended` has its own allowance. A thread or runtime that cannot be made is a warning.
    fn send(self: &Arc<Self>, payload: Value) {
        let counter = if payload["event"] == "task.ended" {
            (&self.ended, MAX_ENDED)
        } else {
            (&self.sent, MAX_SENT)
        };
        if counter.0.fetch_add(1, Ordering::SeqCst) >= counter.1 {
            return;
        }
        let me = self.clone();
        let spawned = std::thread::Builder::new().spawn(move || {
            match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(rt) => rt.block_on(me.run(payload)),
                Err(_) => eprintln!("synt: warning: the notify command could not run (no runtime)"),
            }
        });
        match spawned {
            Ok(h) => self
                .pending
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(h),
            Err(_) => eprintln!("synt: warning: the notify command could not run (no thread)"),
        }
    }

    /// Waits for one of [`MAX_CONCURRENT`] slots; the returned guard gives it back.
    fn slot(&self) -> impl Drop + '_ {
        struct Slot<'a>(&'a (Mutex<usize>, std::sync::Condvar));
        impl Drop for Slot<'_> {
            fn drop(&mut self) {
                *self.0.0.lock().unwrap_or_else(|e| e.into_inner()) -= 1;
                self.0.1.notify_one();
            }
        }
        let mut n = self.slots.0.lock().unwrap_or_else(|e| e.into_inner());
        while *n >= MAX_CONCURRENT {
            n = self.slots.1.wait(n).unwrap_or_else(|e| e.into_inner());
        }
        *n += 1;
        Slot(&self.slots)
    }

    async fn run(&self, payload: Value) {
        let _slot = self.slot();
        let words: Vec<String> = self.argv.iter().map(|a| quote(a)).collect();
        let mut spec = ExecSpec::new(format!("exec {}", words.join(" ")));
        spec.stdin = Some(payload.to_string().into_bytes());
        spec.stderr = StderrMode::Separate;
        spec.timeout = self.timeout;
        spec.max_output_bytes = 16 << 10;
        match self
            .rt
            .exec(&self.sid, spec, &CancellationToken::new())
            .await
        {
            Ok(r) if r.exit_code == Some(0) => {}
            Ok(r) if r.timed_out => eprintln!(
                "synt: warning: the notify command timed out after {} s",
                self.timeout.as_secs()
            ),
            Ok(r) => eprintln!(
                "synt: warning: the notify command failed (exit {})",
                r.exit_code.map_or("none".into(), |c| c.to_string())
            ),
            Err(e) => eprintln!(
                "synt: warning: the notify command could not run: {}",
                synthesis_harness::sanitize_for_display(&e.to_string())
            ),
        }
    }

    /// Waits for the notifications still running (each is bounded by the timeout). Call before
    /// the process exits, then [`Self::close`].
    pub fn flush(&self) {
        let hs: Vec<_> =
            std::mem::take(&mut *self.pending.lock().unwrap_or_else(|e| e.into_inner()));
        // one deadline for all of them (queued commands wait for a slot): what is still running
        // then is left behind, so shutdown never waits longer than this
        let deadline = std::time::Instant::now() + DRAIN_WAIT;
        for h in hs {
            while !h.is_finished() && std::time::Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(20));
            }
            if h.is_finished() {
                let _ = h.join();
            } else {
                eprintln!("synt: warning: a notify command is still running; not waiting longer");
            }
        }
    }

    pub fn close(&self) {
        self.rt.close_session(&self.sid);
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use synthesis_events::TaskStatus;
    use synthesis_terminal::SandboxMode;

    fn env(event: Event) -> Envelope {
        Envelope {
            v: 1,
            seq: 1,
            ts: "t".into(),
            session_id: "s1".into(),
            task_id: Some("k1".into()),
            turn_id: None,
            agent_id: None,
            event,
        }
    }

    fn ended(reason: &str) -> Event {
        Event::TaskEnded {
            status: TaskStatus::Failed,
            reason: reason.into(),
            verification_report_id: None,
            usage: synthesis_events::Usage {
                input_tokens: 1,
                output_tokens: 1,
                cache_read_input_tokens: None,
                cache_creation_input_tokens: None,
                cost_usd: Some(0.5),
            },
            changed_files: vec!["secret/path.rs".into()],
            artifacts_root: "/root".into(),
            needed_permissions: vec![],
        }
    }

    fn cfg(v: Value) -> Result<Option<NotifyConfig>, String> {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(
            d.path().join("settings.json"),
            json!({"synt": {"notify": v}}).to_string(),
        )
        .unwrap();
        read_config(d.path(), &d.path().join("none.json"))
    }

    #[test]
    fn payload_has_ids_status_and_short_reason_only() {
        let mut seen = HashSet::new();
        let p = payload(&env(ended(&"x".repeat(900))), &mut seen, true).unwrap();
        assert_eq!(p["event"], "task.ended");
        assert_eq!(p["status"], "failed");
        assert_eq!(p["reason"].as_str().unwrap().len(), MAX_REASON);
        // default: a code, no text
        let q = payload(
            &env(ended("budget_exceeded:max_cost_usd secret-ish text")),
            &mut seen,
            false,
        )
        .unwrap();
        assert_eq!(q["reason_code"], "budget_exceeded");
        assert!(q.get("reason").is_none() && !q.to_string().contains("secret-ish"));
        let text = p.to_string();
        assert!(
            !text.contains("secret/path.rs") && !text.contains("/root"),
            "{text}"
        );
        let b = Event::ToolApprovalResolved {
            approval_id: "a".into(),
            decision: "deny".into(),
            reviewer: "policy:non-interactive".into(),
            input_changed: false,
            message: Some("rm -rf secret".into()),
        };
        let first = payload(&env(b.clone()), &mut seen, false).unwrap();
        assert_eq!(first["event"], "approval.blocked");
        assert!(!first.to_string().contains("rm -rf"));
        assert!(
            payload(&env(b), &mut seen, false).is_none(),
            "once per task"
        );
        assert!(
            payload(
                &env(Event::Notice {
                    level: "i".into(),
                    text: "t".into()
                }),
                &mut seen,
                false
            )
            .is_none()
        );
    }

    #[test]
    fn config_is_validated_and_argv_is_quoted() {
        assert!(cfg(json!({"command": []})).is_err());
        assert!(cfg(json!({"command": "notify-send hi"})).is_err());
        assert!(cfg(json!({"command": ["a"], "timeout_secs": 0})).is_err());
        assert!(cfg(json!({"kind": "x"})).is_err());
        let c = cfg(json!({"command": ["a", "b c"]})).unwrap().unwrap();
        assert_eq!(c.timeout, Duration::from_secs(10));
        assert_eq!(
            cfg(json!({"kind": "desktop"})).unwrap().unwrap().kind,
            Kind::Desktop
        );
        assert_eq!(quote("it's $(x)"), r"'it'\''s $(x)'");
    }

    #[test]
    fn an_unconfined_run_gets_no_notifier() {
        use synthesis_terminal::SandboxMode::{BestEffort, Enforce, Off};
        assert!(confined(Enforce, true).is_ok());
        for (m, p) in [
            (Off, true),
            (BestEffort, true),
            (Enforce, false),
            (Off, false),
        ] {
            assert!(confined(m, p).is_err(), "{m:?} policy={p}");
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_fake_command_receives_the_event_on_stdin_with_a_scrubbed_env() {
        let d = tempfile::tempdir().unwrap();
        let base = std::fs::canonicalize(d.path()).unwrap();
        std::fs::create_dir_all(base.join("w")).unwrap();
        let out = base.join("w/got.json");
        let ws = Arc::new(
            WorkspaceContext::open(
                "w",
                &base.join("w"),
                &base.join("w"),
                &[],
                base.join("art"),
                "pg",
            )
            .unwrap(),
        );
        let rt = Arc::new(TerminalRuntime::new(SandboxMode::Off));
        let run_env = BTreeMap::from([
            ("PATH".to_string(), std::env::var("PATH").unwrap()),
            ("SYNT_API_KEY".to_string(), "hunter2".to_string()),
        ]);
        let sink = EventSink::new(std::io::sink(), "s1", []);
        let script = format!("cat > {o}; env >> {o}", o = out.display());
        let n = Notifier::start_unchecked(
            &NotifyConfig {
                kind: Kind::Command(vec!["/bin/sh".into(), "-c".into(), script]),
                timeout: Duration::from_secs(10),
                network: false,
                include_text: true,
            },
            rt,
            "s1",
            ws,
            &run_env,
            None,
            &sink,
        )
        .unwrap();
        sink.emit(
            &synthesis_harness::Scope::default(),
            ended("budget_exceeded:max_cost_usd"),
        );
        n.flush();
        n.close();
        let got = std::fs::read_to_string(&out).unwrap();
        assert!(
            got.contains(r#""event":"task.ended""#) && got.contains("budget_exceeded"),
            "{got}"
        );
        assert!(
            !got.contains("hunter2") && !got.contains("SYNT_API_KEY"),
            "{got}"
        );
    }
}
