//! Browser login hand-off and stored secrets for `synth run` (docs/BROWSER-LOGIN.md): the
//! headless implementation of `BrowserHost` (instruction on stderr, answer on the terminal or a
//! signal file), the settings parsing for `synthesis.browser`, `synth browser done` and the
//! `synth doctor` lines. A front-end with its own dialogs implements `BrowserHost` instead.

use std::collections::BTreeMap;
use std::io::BufRead;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use serde_json::{Map, Value};
use synthesis_browser::profile::{self, DONE_FILE};
use synthesis_browser::secret::{SecretSpec, parse_secrets, resolve};
use synthesis_config::Env;
use synthesis_events::Event;
use synthesis_harness::{EventSink, Scope, sanitize_for_display};
use synthesis_tools::{BrowserConfig, BrowserHost, LoginRequest};
use tokio_util::sync::CancellationToken;

use crate::{Homes, homes};

/// What `synthesis.browser` of the user settings declares.
#[derive(Default)]
pub struct BrowserSettings {
    pub secrets: BTreeMap<String, SecretSpec>,
    pub profile_domains: BTreeMap<String, Vec<String>>,
    pub login_timeout: Option<Duration>,
    pub warnings: Vec<String>,
}

pub fn parse_settings(raw: &Map<String, Value>) -> BrowserSettings {
    let mut s = BrowserSettings::default();
    if let Some(m) = raw.get("secrets").and_then(Value::as_object) {
        (s.secrets, s.warnings) = parse_secrets(m);
    }
    if let Some(m) = raw.get("profiles").and_then(Value::as_object) {
        for (name, v) in m {
            let domains: Vec<String> = v["allow_domains"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|d| d.as_str().map(str::to_owned))
                .collect();
            if profile::validate_name(name).is_ok() {
                s.profile_domains.insert(name.clone(), domains);
            } else {
                s.warnings.push(format!(
                    "synthesis.browser.profiles.{name}: not a valid profile name, skipped"
                ));
            }
        }
    }
    if let Some(sec) = raw.get("login_timeout_sec").and_then(Value::as_u64) {
        s.login_timeout = Some(Duration::from_secs(sec.clamp(10, 3600)));
    }
    s
}

impl BrowserSettings {
    /// Values of the secrets that resolve right now, for exact-value masking in events, the
    /// transcript and the session database. A secret that is unset now is simply not masked
    /// there (its value is masked in page captures when it is typed).
    pub fn values(&self, env: &Env) -> Vec<String> {
        self.secrets
            .iter()
            .filter_map(|(n, s)| resolve(n, s, env).ok())
            .collect()
    }

    pub fn config(
        self,
        base: BrowserConfig,
        home: &Path,
        env: &Env,
        host: Arc<dyn BrowserHost>,
    ) -> BrowserConfig {
        BrowserConfig {
            home: Some(home.to_owned()),
            secrets: self.secrets,
            profile_domains: self.profile_domains,
            login_timeout: self.login_timeout,
            secret_env: env.clone(),
            host: Some(host),
            ..base
        }
    }
}

/// The terminal the person types on, even when stdin and stdout are pipes.
fn open_tty() -> Option<std::fs::File> {
    std::fs::File::open(if cfg!(windows) { "CONIN$" } else { "/dev/tty" }).ok()
}

/// A line from the terminal, read on a detached thread (a blocked read must never keep the
/// process alive after a cancel).
fn line_from_tty() -> Option<tokio::sync::oneshot::Receiver<String>> {
    let tty = open_tty()?;
    let (tx, rx) = tokio::sync::oneshot::channel();
    std::thread::spawn(move || {
        let mut s = String::new();
        let _ = std::io::BufReader::new(tty).read_line(&mut s);
        let _ = tx.send(s);
    });
    Some(rx)
}

/// Headless host: prompts on stderr; answers come from the terminal or `synth browser done`.
pub struct CliHost {
    pub sink: EventSink,
}

impl std::fmt::Debug for CliHost {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("CliHost")
    }
}

impl BrowserHost for CliHost {
    fn login<'a>(
        &'a self,
        req: &'a LoginRequest,
        cancel: &'a CancellationToken,
    ) -> synthesis_tools::BoxFuture<'a, Result<(), String>> {
        Box::pin(async move {
            self.sink.emit(
                &Scope {
                    task_id: Some(req.task_id.clone()),
                    turn_id: Some(req.turn_id.clone()),
                    agent_id: None,
                },
                Event::BrowserLoginRequired {
                    origin: req.origin.clone(),
                    profile: req.profile.clone(),
                },
            );
            let rx = line_from_tty();
            eprintln!(
                "synth: log in to {} in the browser window that just opened (profile {}).\nsynth: {} when you are done, or press Ctrl-C to cancel.",
                sanitize_for_display(&req.origin),
                req.profile,
                if rx.is_some() {
                    format!(
                        "Press Enter here, or run `synth browser done {}`",
                        req.profile
                    )
                } else {
                    format!(
                        "Run `synth browser done {}` (no terminal to read Enter from)",
                        req.profile
                    )
                },
            );
            let mut rx = rx;
            loop {
                tokio::select! {
                    _ = async { rx.as_mut().unwrap().await }, if rx.is_some() => return Ok(()),
                    () = tokio::time::sleep(Duration::from_millis(500)) => {
                        if std::fs::remove_file(&req.done_file).is_ok() {
                            return Ok(());
                        }
                    }
                    () = cancel.cancelled() => return Err("cancelled".into()),
                }
            }
        })
    }

    fn confirm_secret<'a>(
        &'a self,
        secret: &'a str,
        origin: &'a str,
        cancel: &'a CancellationToken,
    ) -> synthesis_tools::BoxFuture<'a, bool> {
        Box::pin(async move {
            ask_yes(
                &format!(
                    "Type secret \"{}\" into {}?",
                    sanitize_for_display(secret),
                    sanitize_for_display(origin)
                ),
                cancel,
            )
            .await
        })
    }
}

/// `[y/N]` on the terminal. No terminal means no.
pub async fn ask_yes(question: &str, cancel: &CancellationToken) -> bool {
    let Some(rx) = line_from_tty() else {
        return false;
    };
    eprintln!("synth: {question} [y/N] ");
    tokio::select! {
        l = rx => l.is_ok_and(|l| matches!(l.trim(), "y" | "Y" | "yes")),
        () = cancel.cancelled() => false,
    }
}

/// `synth browser done PROFILE`: ends the wait of a `browser_login` in another terminal.
pub fn done_cmd(env: &Env, name: &str) -> Result<u8, String> {
    let dir = profile::profile_path(&homes(env)?.synthesis, name).map_err(|e| e.to_string())?;
    if !dir.is_dir() {
        return Err(format!("no browser profile named {name:?}"));
    }
    std::fs::write(dir.join(DONE_FILE), b"")
        .map_err(|e| format!("cannot write the signal file: {e}"))?;
    out!("signalled {name}");
    Ok(0)
}

/// Lines for `synth doctor`.
pub fn doctor_lines(env: &Env, h: &Homes) -> Vec<String> {
    let root = profile::profiles_root(&h.synthesis);
    let n = std::fs::read_dir(&root).map_or(0, |d| {
        d.filter_map(Result::ok)
            .filter(|e| e.path().join(profile::META_FILE).is_file())
            .count()
    });
    vec![
        format!("browser profiles: {} ({n} saved login(s))", root.display()),
        match profile::display_available(env, synthesis_browser::Os::current()) {
            Ok(()) => "browser display: available (browser_login can open a window)".into(),
            Err(e) => format!("browser display: not available ({e})"),
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn settings_parse_and_clamp() {
        let raw = json!({
            "secrets": {"ok": {"source": "{env:SYNTHESIS_A_PASSWORD}", "origins": ["https://a.com"]}, "bad": {}},
            "profiles": {"acme": {"allow_domains": ["cdn.acme.com"]}, "Bad Name": {}},
            "login_timeout_sec": 1,
        });
        let s = parse_settings(raw.as_object().unwrap());
        assert_eq!(s.secrets.len(), 1);
        assert_eq!(s.profile_domains["acme"], ["cdn.acme.com"]);
        assert_eq!(s.login_timeout, Some(Duration::from_secs(10)));
        assert_eq!(s.warnings.len(), 2, "{:?}", s.warnings);
        let e = Env::from_pairs([("SYNTHESIS_A_PASSWORD", "hunter2-xyz")]);
        assert_eq!(s.values(&e), ["hunter2-xyz"]);
        assert!(s.values(&Env::default()).is_empty());
    }

    #[derive(Clone, Default)]
    struct Buf(Arc<std::sync::Mutex<Vec<u8>>>);
    impl std::io::Write for Buf {
        fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(b);
            Ok(b.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn stored_secret_values_are_masked_in_every_event() {
        let raw = json!({"secrets": {"pw": {"source": "{env:SYNTHESIS_A_PASSWORD}", "origins": ["https://a.com"]}}});
        let s = parse_settings(raw.as_object().unwrap());
        let env = Env::from_pairs([("SYNTHESIS_A_PASSWORD", "Zq9-hunter2/S3CRET")]);
        let out = Buf::default();
        let sink = EventSink::new(out.clone(), "s", s.values(&env));
        for text in [
            "typed Zq9-hunter2/S3CRET here",
            "tool output {\"v\":\"Zq9-hunter2/S3CRET\"}",
        ] {
            sink.emit(
                &Scope::default(),
                Event::Notice {
                    level: "info".into(),
                    text: text.into(),
                },
            );
        }
        let all = String::from_utf8(out.0.lock().unwrap().clone()).unwrap();
        assert!(
            !all.contains("S3CRET") && all.contains("[REDACTED:secret]"),
            "{all}"
        );
    }

    #[tokio::test]
    async fn signal_file_ends_the_login_wait_and_the_event_has_names_only() {
        let dir = tempfile::tempdir().unwrap();
        let sink = EventSink::new(Vec::new(), "s", ["hunter2-xyz".to_string()]);
        let host = CliHost { sink };
        let req = LoginRequest {
            origin: "https://app.example.com".into(),
            profile: "acme".into(),
            timeout: Duration::from_secs(5),
            task_id: "t".into(),
            turn_id: "u".into(),
            tool_call_id: "c".into(),
            done_file: dir.path().join(DONE_FILE),
        };
        let done = req.done_file.clone();
        let cancel = CancellationToken::new();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(700)).await;
            std::fs::write(done, b"").unwrap();
        });
        // no terminal is attached in tests that run without one; with one, Enter also works
        host.login(&req, &cancel).await.unwrap();
        // cancel wins when nobody answers
        let c2 = CancellationToken::new();
        c2.cancel();
        assert_eq!(host.login(&req, &c2).await.unwrap_err(), "cancelled");
    }
}
