//! Optional notifications for unattended runs (docs/UNATTENDED.md, "Notifications").
//!
//! `{"synt":{"notify":{"command":["/path/to/prog","arg"],"timeout_secs":10,"network":false}}}` in
//! the USER or MANAGED settings (managed wins as a whole; a project file is never read). The
//! command gets one JSON event on stdin: `task.ended` (any status), `approval.blocked` (a headless
//! run denied an ask; once per task) and `budget.exceeded`. It runs through the sandboxed terminal
//! runtime in its own session with a scrubbed env (PATH/locale only: no API key, no `SYNT_*`), the
//! workspace as cwd, a short timeout, and at most 50 times per process.
//!
//! Threat model: the payload is built from the already redacted event stream and holds only ids,
//! the status, the redacted and truncated stop reason and the budget numbers; never the final
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
    })
}

/// POSIX single quoting: the shell sees one literal word, so nothing in an argument is expanded.
fn quote(a: &str) -> String {
    format!("'{}'", a.replace('\'', r"'\''"))
}

/// The JSON event for an envelope the notifier cares about; `None` for all others. `seen` holds
/// the tasks whose blocked approval was already reported.
pub fn payload(env: &Envelope, seen: &mut HashSet<String>) -> Option<Value> {
    let short = |s: &str| s.chars().take(MAX_REASON).collect::<String>();
    let (event, detail) = match &env.event {
        Event::TaskEnded {
            status,
            reason,
            usage,
            ..
        } => (
            "task.ended",
            json!({"status": status, "reason": short(reason), "cost_usd": usage.cost_usd}),
        ),
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
        } => (
            "budget.exceeded",
            json!({"budget": short(budget), "limit": limit, "used": used}),
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

pub struct Notifier {
    argv: Vec<String>,
    timeout: Duration,
    rt: Arc<TerminalRuntime>,
    sid: String,
    sent: AtomicUsize,
    seen: Mutex<HashSet<String>>,
    pending: Mutex<Vec<std::thread::JoinHandle<()>>>,
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
            if cfg.network {
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
            sent: AtomicUsize::new(0),
            seen: Mutex::new(HashSet::new()),
            pending: Mutex::new(vec![]),
        });
        let me = n.clone();
        sink.observe(Arc::new(move |env| {
            let p = payload(env, &mut me.seen.lock().unwrap_or_else(|e| e.into_inner()));
            if let Some(p) = p {
                me.send(p);
            }
        }));
        Ok(n)
    }

    /// Runs the command on its own thread so the run never waits for it (see [`Self::flush`]).
    fn send(self: &Arc<Self>, payload: Value) {
        if self.sent.fetch_add(1, Ordering::SeqCst) >= MAX_SENT {
            return;
        }
        let me = self.clone();
        let h = std::thread::spawn(move || {
            let Ok(rt) = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            else {
                return;
            };
            rt.block_on(me.run(payload));
        });
        self.pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(h);
    }

    async fn run(&self, payload: Value) {
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
        for h in hs {
            let _ = h.join();
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
        let p = payload(&env(ended(&"x".repeat(900))), &mut seen).unwrap();
        assert_eq!(p["event"], "task.ended");
        assert_eq!(p["status"], "failed");
        assert_eq!(p["reason"].as_str().unwrap().len(), MAX_REASON);
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
        let first = payload(&env(b.clone()), &mut seen).unwrap();
        assert_eq!(first["event"], "approval.blocked");
        assert!(!first.to_string().contains("rm -rf"));
        assert!(payload(&env(b), &mut seen).is_none(), "once per task");
        assert!(
            payload(
                &env(Event::Notice {
                    level: "i".into(),
                    text: "t".into()
                }),
                &mut seen
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
        let n = Notifier::start(
            &NotifyConfig {
                kind: Kind::Command(vec!["/bin/sh".into(), "-c".into(), script]),
                timeout: Duration::from_secs(10),
                network: false,
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
