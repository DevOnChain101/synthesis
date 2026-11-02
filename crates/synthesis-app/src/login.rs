//! Browser login hand-off and stored secrets for `synth run` (docs/BROWSER-LOGIN.md): the
//! headless implementation of `BrowserHost` (prompts on the terminal), the settings parsing for
//! `synthesis.browser`, `synth browser done` and the `synth doctor` lines. A front-end with its
//! own dialogs implements `BrowserHost` instead.
//!
//! Terminal rules: ONE reader thread owns the terminal and prompts are serialized, so two
//! prompts can never answer each other; typed-ahead input is flushed before each prompt; a login
//! can only be finished by the person at the terminal (Enter) or with the per-wait token that is
//! printed only there.

use std::collections::{BTreeMap, BTreeSet};
use std::io::{BufRead, IsTerminal, Write};
use std::path::Path;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use serde_json::{Map, Value};
use synthesis_browser::profile::{self, DONE_FILE, WAITING_FILE};
use synthesis_browser::secret::{SecretSpec, Source, origin_allowed, parse_secrets, resolve};
use synthesis_config::Env;
use synthesis_events::Event;
use synthesis_harness::{EventSink, Scope, sanitize_for_display};
use synthesis_tools::{BrowserConfig, BrowserHost, LoginRequest};
use tokio::sync::{Mutex, mpsc};
use tokio_util::sync::CancellationToken;

use crate::{Homes, homes};

/// What `synthesis.browser` of the user settings declares.
#[derive(Default)]
pub struct BrowserSettings {
    pub secrets: BTreeMap<String, SecretSpec>,
    pub profile_domains: BTreeMap<String, Vec<String>>,
    pub login_timeout: Option<Duration>,
    pub allow_unsandboxed: bool,
    pub approved_profiles: BTreeSet<String>,
    pub approved_secrets: BTreeSet<(String, String)>,
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
    s.allow_unsandboxed = raw.get("allow_unsandboxed") == Some(&Value::Bool(true));
    // Pre-approval for runs nobody can answer: named profiles, and (secret, origin) pairs that the
    // secret is already bound to.
    if let Some(a) = raw.get("approved").and_then(Value::as_object) {
        for n in a
            .get("profiles")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            match n.as_str().filter(|n| profile::validate_name(n).is_ok()) {
                Some(n) => {
                    s.approved_profiles.insert(n.to_owned());
                }
                None => s.warnings.push(
                    "synthesis.browser.approved.profiles: an entry is not a valid profile name, skipped".into(),
                ),
            }
        }
        for e in a
            .get("secrets")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let pair = (e["secret"].as_str(), e["origin"].as_str());
            let ok = match pair {
                (Some(n), Some(o)) => s
                    .secrets
                    .get(n)
                    .filter(|sp| origin_allowed(o, sp))
                    .and_then(|_| profile::validate_login_origin(o).ok())
                    .and_then(|u| profile::origin_of(&u).map(|o| (n.to_owned(), o))),
                _ => None,
            };
            match ok {
                Some(p) => {
                    s.approved_secrets.insert(p);
                }
                None => s.warnings.push(
                    "synthesis.browser.approved.secrets: an entry names no declared secret/bound origin, skipped".into(),
                ),
            }
        }
    }
    s
}

impl BrowserSettings {
    /// Values of the secrets that resolve right now and are long enough to mask by content, for
    /// exact-value masking in events, the transcript and the session database. A secret that is
    /// unset now is simply not masked there (its value is masked in page captures when typed).
    pub fn values(&self, env: &Env) -> Vec<String> {
        self.secrets
            .iter()
            .filter_map(|(n, s)| resolve(n, s, env).ok())
            .filter(|v| v.len() >= synthesis_browser::MASK_MIN)
            .collect()
    }

    /// Files named by `{file:}` sources: hidden from the agent's tools like Synthesis's own state.
    pub fn secret_files(&self) -> Vec<std::path::PathBuf> {
        self.secrets
            .values()
            .filter_map(|s| match &s.source {
                Source::File(p) => Some(p.clone()),
                Source::Env(_) => None,
            })
            .collect()
    }

    /// `sandboxed`: the agent's commands are confined (`--sandbox enforce` with a working backend).
    pub fn config(
        self,
        base: BrowserConfig,
        home: &Path,
        env: &Env,
        host: Arc<dyn BrowserHost>,
        sandboxed: bool,
    ) -> BrowserConfig {
        BrowserConfig {
            home: Some(home.to_owned()),
            secrets: self.secrets,
            profile_domains: self.profile_domains,
            login_timeout: self.login_timeout,
            process_env: Some(env.clone()),
            sandboxed,
            allow_unsandboxed: self.allow_unsandboxed,
            pre_approved_profiles: self.approved_profiles,
            pre_approved_secrets: self.approved_secrets,
            host: Some(host),
            ..base
        }
    }
}

// ---- the terminal ---------------------------------------------------------------------------

/// The only reader of the terminal. Prompts take `lines` one at a time, drop whatever was typed
/// or flushed before them, print, and wait for the next line.
pub struct Prompter {
    lines: Mutex<mpsc::UnboundedReceiver<String>>,
    flush: Box<dyn Fn() + Send + Sync>,
}

impl Prompter {
    pub fn new(rx: mpsc::UnboundedReceiver<String>, flush: Box<dyn Fn() + Send + Sync>) -> Self {
        Self {
            lines: Mutex::new(rx),
            flush,
        }
    }

    /// `say` prints the prompt AFTER the terminal input was flushed. `tick` runs every 250 ms
    /// and may end the wait (`Some("")`). `None` = cancelled.
    pub async fn ask(
        &self,
        say: impl FnOnce(),
        cancel: &CancellationToken,
        mut tick: impl FnMut() -> bool,
    ) -> Option<String> {
        let mut rx = tokio::select! {
            g = self.lines.lock() => g,
            () = cancel.cancelled() => return None,
        };
        (self.flush)();
        while rx.try_recv().is_ok() {}
        say();
        loop {
            tokio::select! {
                l = rx.recv() => return l,
                () = tokio::time::sleep(Duration::from_millis(250)) => {
                    if tick() {
                        return Some(String::new());
                    }
                }
                () = cancel.cancelled() => return None,
            }
        }
    }
}

fn tty_path() -> &'static str {
    if cfg!(windows) { "CONIN$" } else { "/dev/tty" }
}

/// The terminal the person types on, even when stdin and stdout are pipes.
fn tty() -> Option<&'static Prompter> {
    static T: OnceLock<Option<Prompter>> = OnceLock::new();
    T.get_or_init(|| {
        let f = Arc::new(std::fs::File::open(tty_path()).ok()?);
        let (tx, rx) = mpsc::unbounded_channel();
        let reader = f.clone();
        std::thread::spawn(move || {
            let mut r = std::io::BufReader::new(&*reader);
            loop {
                let mut s = String::new();
                match r.read_line(&mut s) {
                    Ok(0) | Err(_) => return,
                    Ok(_) => {
                        if tx.send(s).is_err() {
                            return;
                        }
                    }
                }
            }
        });
        #[cfg(unix)]
        let flush: Box<dyn Fn() + Send + Sync> = Box::new(move || {
            let _ = rustix::termios::tcflush(&*f, rustix::termios::QueueSelector::IFlush);
        });
        #[cfg(not(unix))]
        let flush: Box<dyn Fn() + Send + Sync> = Box::new(|| {});
        Some(Prompter::new(rx, flush))
    })
    .as_ref()
}

/// Writes to the terminal itself (not stderr: a wrapper may log stderr into the workspace).
/// `false` when there is no terminal.
fn say_on_tty(text: &str) -> bool {
    let w = if cfg!(windows) { "CONOUT$" } else { tty_path() };
    match std::fs::OpenOptions::new().write(true).open(w) {
        Ok(mut f) => f.write_all(text.as_bytes()).is_ok(),
        Err(_) => false,
    }
}

/// `[y/N]` on the terminal. No terminal means no.
pub async fn ask_yes(question: &str, cancel: &CancellationToken) -> bool {
    let Some(t) = tty() else { return false };
    let q = format!("synth: {question} [y/N] ");
    t.ask(
        || {
            if !say_on_tty(&q) {
                eprintln!("{q}");
            }
        },
        cancel,
        || false,
    )
    .await
    .is_some_and(|l| matches!(l.trim(), "y" | "Y" | "yes"))
}

// ---- the signal file (authenticated) --------------------------------------------------------

fn open_nofollow(o: &mut std::fs::OpenOptions) -> &mut std::fs::OpenOptions {
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        o.custom_flags(libc::O_NOFOLLOW).mode(0o600);
    }
    o
}

/// The file holds exactly the token: consumed on a match, deleted on a mismatch. A symlink is
/// never followed.
fn done_matches(file: &Path, token: &str) -> bool {
    use std::io::Read;
    let mut o = std::fs::OpenOptions::new();
    let Ok(f) = open_nofollow(o.read(true)).open(file) else {
        return false;
    };
    let mut s = String::new();
    let ok = f.take(128).read_to_string(&mut s).is_ok() && s.trim() == token;
    let _ = std::fs::remove_file(file);
    ok
}

/// Writes the token for a waiting login. Refused when no login waits (no marker) or when the
/// profile directory or either file is a symlink.
fn signal_done(dir: &Path, token: &str) -> Result<(), String> {
    let plain = |p: &Path| {
        std::fs::symlink_metadata(p)
            .is_ok_and(|m| !m.file_type().is_symlink() && (m.is_dir() || m.is_file()))
    };
    if !plain(dir) || !plain(&dir.join(WAITING_FILE)) {
        return Err("no login is waiting for this profile".into());
    }
    let mut o = std::fs::OpenOptions::new();
    let mut f = open_nofollow(o.write(true).create(true).truncate(true))
        .open(dir.join(DONE_FILE))
        .map_err(|e| format!("cannot write the signal file: {e}"))?;
    f.write_all(token.as_bytes()).map_err(|e| e.to_string())
}

/// Removes the wait markers when the wait ends, however it ends.
struct Waiting<'a>(&'a LoginRequest);

impl<'a> Waiting<'a> {
    fn new(req: &'a LoginRequest) -> Result<Self, String> {
        let dir = req.done_file.parent().unwrap_or(Path::new("."));
        let _ = std::fs::remove_file(&req.done_file);
        let _ = std::fs::remove_file(dir.join(WAITING_FILE));
        let mut o = std::fs::OpenOptions::new();
        open_nofollow(o.write(true).create_new(true))
            .open(dir.join(WAITING_FILE))
            .map_err(|e| format!("cannot mark the wait: {e}"))?;
        Ok(Self(req))
    }
}

impl Drop for Waiting<'_> {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0.done_file);
        if let Some(d) = self.0.done_file.parent() {
            let _ = std::fs::remove_file(d.join(WAITING_FILE));
        }
    }
}

// ---- the host -------------------------------------------------------------------------------

/// Headless host: prompts on the terminal; no terminal means refusal.
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
            // The token goes to the terminal only. Without one the wait could never be answered
            // by the person; do not pretend.
            let t = tty();
            if t.is_none() && !std::io::stderr().is_terminal() {
                return Err("no terminal to ask on: the login cannot be confirmed here".into());
            }
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
            let _wait = Waiting::new(req)?;
            let token = uuid::Uuid::new_v4().simple().to_string();
            let text = format!(
                "synth: log in to {} in the browser window that just opened (profile {}).\nsynth: press Enter here when you are done, or run `synth browser done {}` in another terminal and enter this code: {token}\nsynth: Ctrl-C cancels.\n",
                sanitize_for_display(&req.origin),
                req.profile,
                req.profile
            );
            let say = || {
                if !say_on_tty(&text) {
                    let _ = std::io::stderr().write_all(text.as_bytes());
                }
            };
            let tick = || done_matches(&req.done_file, &token);
            match t {
                Some(t) => t.ask(say, cancel, tick).await.map(|_| ()),
                None => {
                    say();
                    loop {
                        tokio::select! {
                            () = tokio::time::sleep(Duration::from_millis(250)) => if tick() { break Some(()) },
                            () = cancel.cancelled() => break None,
                        }
                    }
                }
            }
            .ok_or_else(|| "cancelled".to_string())
        })
    }

    fn confirm_profile<'a>(
        &'a self,
        profile: &'a str,
        domains: &'a [String],
        cancel: &'a CancellationToken,
    ) -> synthesis_tools::BoxFuture<'a, bool> {
        Box::pin(async move {
            ask_yes(
                &format!(
                    "Browse with saved login \"{}\" (logged in to: {})?",
                    sanitize_for_display(profile),
                    sanitize_for_display(&domains.join(", "))
                ),
                cancel,
            )
            .await
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

/// `synth browser done PROFILE`: ends the wait of a `browser_login` in another terminal. The
/// code printed in the terminal that waits is read from stdin (never from argv, which `ps` shows).
pub fn done_cmd(env: &Env, name: &str) -> Result<u8, String> {
    let dir = profile::profile_path(&homes(env)?.synthesis, name).map_err(|e| e.to_string())?;
    if std::io::stdin().is_terminal() {
        eprint!("code shown by the waiting synth: ");
    }
    let mut token = String::new();
    std::io::stdin()
        .lock()
        .read_line(&mut token)
        .map_err(|e| e.to_string())?;
    signal_done(&dir, token.trim())?;
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
    fn settings_parse_clamp_and_approvals() {
        let raw = json!({
            "secrets": {"ok": {"source": "{env:SYNTHESIS_A_PASSWORD}", "origins": ["https://a.com"]}, "bad": {}},
            "profiles": {"acme": {"allow_domains": ["cdn.acme.com"]}, "Bad Name": {}},
            "login_timeout_sec": 1,
            "allow_unsandboxed": true,
            "approved": {
                "profiles": ["acme", "../x"],
                "secrets": [
                    {"secret": "ok", "origin": "https://a.com"},
                    {"secret": "ok", "origin": "https://evil.com"},
                    {"secret": "nope", "origin": "https://a.com"}
                ]
            },
        });
        let s = parse_settings(raw.as_object().unwrap());
        assert_eq!(s.secrets.len(), 1);
        assert_eq!(s.profile_domains["acme"], ["cdn.acme.com"]);
        assert_eq!(s.login_timeout, Some(Duration::from_secs(10)));
        assert!(s.allow_unsandboxed);
        assert_eq!(s.approved_profiles, BTreeSet::from(["acme".to_string()]));
        assert_eq!(
            s.approved_secrets,
            BTreeSet::from([("ok".to_string(), "https://a.com".to_string())])
        );
        assert_eq!(s.warnings.len(), 5, "{:?}", s.warnings);
        let e = Env::from_pairs([("SYNTHESIS_A_PASSWORD", "hunter2-xyz")]);
        assert_eq!(s.values(&e), ["hunter2-xyz"]);
        let short = Env::from_pairs([("SYNTHESIS_A_PASSWORD", "abc")]);
        assert!(s.values(&short).is_empty(), "too short to mask by content");
        assert!(s.values(&Env::default()).is_empty());
        assert!(!parse_settings(&Map::new()).allow_unsandboxed);
    }

    #[test]
    fn secret_file_paths_are_listed_for_the_deny_list() {
        let raw = json!({"secrets": {
            "f": {"source": "{file:/home/me/.config/pw}", "origins": ["https://a.com"]},
            "e": {"source": "{env:SYNTHESIS_A_PASSWORD}", "origins": ["https://a.com"]}}});
        let s = parse_settings(raw.as_object().unwrap());
        assert_eq!(
            s.secret_files(),
            [std::path::PathBuf::from("/home/me/.config/pw")]
        );
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
    async fn concurrent_prompts_are_serialized_and_cannot_cross_answer() {
        let (tx, rx) = mpsc::unbounded_channel();
        let flushed = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let f2 = flushed.clone();
        let p = Arc::new(Prompter::new(
            rx,
            Box::new(move || {
                f2.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            }),
        ));
        tx.send("y\n".into()).unwrap(); // typed ahead: must not answer anything
        let shown = Arc::new(std::sync::Mutex::new(vec![]));
        let ask = |name: &'static str| {
            let (p, shown) = (p.clone(), shown.clone());
            tokio::spawn(async move {
                p.ask(
                    || shown.lock().unwrap().push(name),
                    &CancellationToken::new(),
                    || false,
                )
                .await
            })
        };
        let a = ask("first");
        tokio::time::sleep(Duration::from_millis(100)).await;
        let b = ask("second");
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(
            *shown.lock().unwrap(),
            ["first"],
            "the second prompt waits its turn"
        );
        tx.send("one\n".into()).unwrap();
        assert_eq!(a.await.unwrap().as_deref(), Some("one\n"));
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(*shown.lock().unwrap(), ["first", "second"]);
        tx.send("two\n".into()).unwrap();
        assert_eq!(b.await.unwrap().as_deref(), Some("two\n"));
        assert_eq!(flushed.load(std::sync::atomic::Ordering::SeqCst), 2);
        // cancelling a waiting prompt returns None and does not hold the terminal
        let c = CancellationToken::new();
        let waiting = {
            let (p, c) = (p.clone(), c.clone());
            tokio::spawn(async move { p.ask(|| {}, &c, || false).await })
        };
        tokio::time::sleep(Duration::from_millis(50)).await;
        c.cancel();
        assert_eq!(waiting.await.unwrap(), None);
    }

    #[test]
    fn done_needs_a_waiting_login_and_the_token_and_ignores_symlinks() {
        let d = tempfile::tempdir().unwrap();
        let file = d.path().join(DONE_FILE);
        // nobody waits: refused
        assert!(
            signal_done(d.path(), "tok")
                .unwrap_err()
                .contains("no login is waiting")
        );
        let req = LoginRequest {
            origin: "https://a.com".into(),
            profile: "a".into(),
            timeout: Duration::from_secs(5),
            task_id: "t".into(),
            turn_id: "u".into(),
            tool_call_id: "c".into(),
            done_file: file.clone(),
        };
        let w = Waiting::new(&req).unwrap();
        // a file written by anyone who does not know the token does not end the wait
        std::fs::write(&file, "guess").unwrap();
        assert!(!done_matches(&file, "tok"));
        assert!(!file.exists(), "a wrong token is discarded");
        signal_done(d.path(), "tok").unwrap();
        assert!(done_matches(&file, "tok"));
        assert!(!file.exists(), "consumed");
        #[cfg(unix)]
        {
            // a symlink planted as the signal file is never followed (neither read nor written)
            let secret = d.path().join("elsewhere");
            std::fs::write(&secret, "tok").unwrap();
            std::os::unix::fs::symlink(&secret, &file).unwrap();
            assert!(!done_matches(&file, "tok"));
            let _ = std::fs::remove_file(&file);
            std::os::unix::fs::symlink(&secret, &file).unwrap();
            assert!(signal_done(d.path(), "other").is_err());
            assert_eq!(
                std::fs::read_to_string(&secret).unwrap(),
                "tok",
                "target untouched"
            );
            let _ = std::fs::remove_file(&file);
        }
        drop(w);
        assert!(
            signal_done(d.path(), "tok").is_err(),
            "the marker goes with the wait"
        );
    }

    #[tokio::test]
    async fn login_without_any_terminal_is_refused_not_pretended() {
        if std::fs::File::open(tty_path()).is_ok() {
            return; // a developer terminal exists: the Enter path would race this test
        }
        let d = tempfile::tempdir().unwrap();
        let sink = EventSink::new(Vec::new(), "s", Vec::<String>::new());
        let host = CliHost { sink };
        let req = LoginRequest {
            origin: "https://app.example.com".into(),
            profile: "acme".into(),
            timeout: Duration::from_secs(5),
            task_id: "t".into(),
            turn_id: "u".into(),
            tool_call_id: "c".into(),
            done_file: d.path().join(DONE_FILE),
        };
        // stderr is not a terminal under the test harness, so there is nobody to ask: refused
        let e = host.login(&req, &CancellationToken::new()).await;
        if !std::io::stderr().is_terminal() {
            assert!(e.unwrap_err().contains("no terminal"));
        }
    }
}
