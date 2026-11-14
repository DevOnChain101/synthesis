//! Browser login hand-off and stored secrets for `synt run` (docs/BROWSER-LOGIN.md): the
//! headless implementation of `BrowserHost` (prompts on the terminal), the settings parsing for
//! `synt.browser`, `synt browser done` and the `synt doctor` lines. A front-end with its
//! own dialogs implements `BrowserHost` instead.
//!
//! Terminal rules: ONE reader thread owns the terminal and prompts are serialized, so two
//! prompts can never answer each other; typed-ahead input is flushed before each prompt; a login
//! can only be finished by the person at the terminal (Enter) or with the per-wait token that is
//! printed only there.

use std::collections::{BTreeMap, BTreeSet};
use std::io::{BufRead, IsTerminal, Write};
use std::path::Path;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use serde_json::{Map, Value};
use synthesis_browser::profile::{self, DONE_FILE, WAITING_FILE};
use synthesis_browser::secret::{SecretSpec, Source, origin_allowed, parse_secrets, resolve};
use synthesis_config::Env;
use synthesis_events::Event;
use synthesis_harness::{
    Approval, ApprovalDecision, ApprovalRequest, Approver, EventSink, Scope, sanitize_for_display,
};
use synthesis_tools::{BrowserConfig, BrowserHost, LoginRequest};
use tokio::sync::{Mutex, mpsc};
use tokio_util::sync::CancellationToken;

use crate::{Homes, homes};

/// What `synt.browser` of the user settings declares.
#[derive(Default)]
pub struct BrowserSettings {
    pub secrets: BTreeMap<String, SecretSpec>,
    pub profile_domains: BTreeMap<String, Vec<String>>,
    pub login_timeout: Option<Duration>,
    pub allow_unsandboxed: bool,
    pub allow_with_mcp: bool,
    /// `synt.browser.headed`; unset: the front-end's default (visible in the interactive app).
    pub headed: Option<bool>,
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
                    "synt.browser.profiles.{name}: not a valid profile name, skipped"
                ));
            }
        }
    }
    if let Some(sec) = raw.get("login_timeout_sec").and_then(Value::as_u64) {
        s.login_timeout = Some(Duration::from_secs(sec.clamp(10, 3600)));
    }
    s.allow_unsandboxed = raw.get("allow_unsandboxed") == Some(&Value::Bool(true));
    s.allow_with_mcp = raw.get("allow_with_mcp") == Some(&Value::Bool(true));
    s.headed = raw.get("headed").and_then(Value::as_bool);
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
                    "synt.browser.approved.profiles: an entry is not a valid profile name, skipped"
                        .into(),
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
                    "synt.browser.approved.secrets: an entry names no declared secret/bound origin, skipped".into(),
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

    /// Whether anything credential-like exists that commands outside the sandbox could read.
    pub fn holds_credentials(&self, home: &Path) -> bool {
        profile_count(home) > 0 || !self.secret_files().is_empty()
    }

    /// `sandboxed`: the agent's commands are confined (`--sandbox enforce` with a working
    /// backend). `mcp_stdio`: names of the stdio MCP servers of the run (`stdio_servers`).
    pub fn config(
        self,
        base: BrowserConfig,
        home: &Path,
        env: &Env,
        host: Arc<dyn BrowserHost>,
        sandboxed: bool,
        mcp_stdio: Vec<String>,
    ) -> BrowserConfig {
        BrowserConfig {
            home: Some(home.to_owned()),
            secrets: self.secrets,
            profile_domains: self.profile_domains,
            login_timeout: self.login_timeout,
            process_env: Some(env.clone()),
            sandboxed,
            allow_unsandboxed: self.allow_unsandboxed,
            mcp_stdio,
            allow_with_mcp: self.allow_with_mcp,
            headed: self.headed == Some(true),
            pre_approved_profiles: self.approved_profiles,
            pre_approved_secrets: self.approved_secrets,
            host: Some(host),
            ..base
        }
    }
}

/// Saved logins on disk (a profile directory with a record).
pub fn profile_count(home: &Path) -> usize {
    std::fs::read_dir(profile::profiles_root(home)).map_or(0, |d| {
        d.filter_map(Result::ok)
            .filter(|e| e.path().join(profile::META_FILE).is_file())
            .count()
    })
}

/// Names of the enabled stdio MCP servers: ordinary child processes, outside the sandbox.
pub fn stdio_servers(servers: &[synthesis_mcp::McpServerConfig]) -> Vec<String> {
    use synthesis_mcp::TransportKind;
    servers
        .iter()
        .filter(|s| !s.disabled)
        .filter(|s| match s.transport {
            Some(t) => t == TransportKind::Stdio,
            None => s.url.is_none(),
        })
        .map(|s| s.name.clone())
        .collect()
}

/// Warnings about credentials the agent's side could reach. Printed at `synt run` start and by
/// `synt doctor`; a front-end with its own start-up (a CLI) calls this too.
pub fn exposure_warnings(
    home: &Path,
    b: &BrowserSettings,
    mcp_stdio: &[String],
    sandbox_enforced: bool,
) -> Vec<String> {
    let mut w = vec![];
    let n = profile_count(home);
    if n > 0 && !sandbox_enforced {
        w.push(format!(
            "browser: {n} saved login(s) in {} but the sandbox is not enforce: commands the agent runs can read them. Use --sandbox enforce; the credential browser tools refuse otherwise",
            profile::profiles_root(home).display()
        ));
    }
    if b.holds_credentials(home) && !mcp_stdio.is_empty() && !b.allow_with_mcp {
        w.push(format!(
            "browser: stdio MCP server(s) {} run outside the sandbox and could read your saved logins and secret files; browser_login, browser_fill_secret and saved logins are refused while they are configured (remove them or set synt.browser.allow_with_mcp to true)",
            mcp_stdio.join(", ")
        ));
    }
    w
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

/// The terminal can be written to (the one-time code is shown there and nowhere else).
fn tty_writable() -> bool {
    let w = if cfg!(windows) { "CONOUT$" } else { tty_path() };
    std::fs::OpenOptions::new().write(true).open(w).is_ok()
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
    let q = format!("synt: {question} [y/N] ");
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

/// What a signal file said. A wrong code is counted and ignored: the wait goes on (deleting the
/// file keeps one bad write from being counted twice).
#[derive(Debug, PartialEq, Eq)]
enum Signal {
    None,
    Right,
    Wrong,
}

/// The file holds exactly the token: consumed on a match. A symlink is never followed.
fn check_signal(file: &Path, token: &str) -> Signal {
    use std::io::Read;
    let mut o = std::fs::OpenOptions::new();
    let Ok(f) = open_nofollow(o.read(true)).open(file) else {
        return Signal::None;
    };
    let mut s = String::new();
    let ok = f.take(128).read_to_string(&mut s).is_ok() && s.trim() == token;
    let _ = std::fs::remove_file(file);
    if ok { Signal::Right } else { Signal::Wrong }
}

/// Ends the wait on the right code, or on the `MAX_WRONG_CODES`th wrong one.
fn judge(sig: Signal, wrong: &AtomicU8) -> bool {
    match sig {
        Signal::Right => true,
        Signal::Wrong => wrong.fetch_add(1, Ordering::SeqCst) + 1 >= MAX_WRONG_CODES,
        Signal::None => false,
    }
}

/// Wrong codes tolerated before the login is cancelled (something is guessing).
const MAX_WRONG_CODES: u8 = 5;

/// Ok when a login waits for this profile (marker present; directory and marker not symlinks).
fn login_waiting(dir: &Path) -> Result<(), String> {
    let plain = |p: &Path| {
        std::fs::symlink_metadata(p)
            .is_ok_and(|m| !m.file_type().is_symlink() && (m.is_dir() || m.is_file()))
    };
    if plain(dir) && plain(&dir.join(WAITING_FILE)) {
        Ok(())
    } else {
        Err("no login is waiting for this profile".into())
    }
}

/// One line from `r`, within `timeout`; EOF before any text is an error, never an empty code.
fn read_code(r: impl std::io::Read + Send + 'static, timeout: Duration) -> Result<String, String> {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut line = String::new();
        let _ = tx.send(
            std::io::BufReader::new(r)
                .read_line(&mut line)
                .map(|_| line)
                .map_err(|e| e.to_string()),
        );
    });
    let line = rx
        .recv_timeout(timeout)
        .map_err(|_| format!("no code was entered within {} s", timeout.as_secs()))??;
    match line.trim() {
        "" => Err("no code was given on stdin (the waiting synt prints it)".into()),
        t => Ok(t.to_string()),
    }
}

/// Writes the token for a waiting login. Refused when no login waits (no marker) or when the
/// profile directory or either file is a symlink.
fn signal_done(dir: &Path, token: &str) -> Result<(), String> {
    login_waiting(dir)?;
    let mut o = std::fs::OpenOptions::new();
    let mut f = open_nofollow(o.write(true).create(true).truncate(true))
        .open(dir.join(DONE_FILE))
        .map_err(|e| format!("cannot write the signal file: {e}"))?;
    f.write_all(token.as_bytes()).map_err(|e| e.to_string())
}

/// The wait marker of ONE login per profile. Created exclusively (a second login on the same
/// profile fails instead of clobbering the first), it carries a unique id, and `Drop` removes
/// only a marker that still holds that id, so one login never deletes another's files.
struct Waiting {
    marker: std::path::PathBuf,
    done: std::path::PathBuf,
    id: String,
}

impl Waiting {
    fn new(req: &LoginRequest) -> Result<Self, String> {
        let dir = req.done_file.parent().unwrap_or(Path::new("."));
        let marker = dir.join(WAITING_FILE);
        // a marker left by a crashed run is stale once the login timeout (plus slack) has passed
        if std::fs::metadata(&marker)
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.elapsed().ok())
            .is_some_and(|age| age > req.timeout + Duration::from_secs(60))
        {
            let _ = std::fs::remove_file(&marker);
        }
        let id = uuid::Uuid::new_v4().simple().to_string();
        let mut o = std::fs::OpenOptions::new();
        let mut f = open_nofollow(o.write(true).create_new(true))
            .open(&marker)
            .map_err(|e| match e.kind() {
                std::io::ErrorKind::AlreadyExists => format!(
                    "another login is already waiting on profile {}; finish or cancel it first",
                    req.profile
                ),
                _ => format!("cannot mark the wait: {e}"),
            })?;
        f.write_all(id.as_bytes())
            .map_err(|e| format!("cannot mark the wait: {e}"))?;
        let _ = std::fs::remove_file(&req.done_file); // ours now: nothing earlier counts
        Ok(Self {
            marker,
            done: req.done_file.clone(),
            id,
        })
    }
}

impl Drop for Waiting {
    fn drop(&mut self) {
        if std::fs::read_to_string(&self.marker).is_ok_and(|c| c == self.id) {
            let _ = std::fs::remove_file(&self.done);
            let _ = std::fs::remove_file(&self.marker);
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
            // The one-time code is shown on the terminal and nowhere else (not stderr, which a
            // wrapper may log into the workspace). Without a terminal the login cannot be
            // confirmed by the person, so it does not start.
            let Some(t) = tty().filter(|_| tty_writable()) else {
                return Err("no terminal to ask on: the login cannot be confirmed here".into());
            };
            emit_login_required(&self.sink, req);
            let _wait = Waiting::new(req)?;
            let token = uuid::Uuid::new_v4().simple().to_string();
            let text = format!(
                "synt: log in to {} in the browser window that just opened (profile {}).\nsynt: press Enter here when you are done, or run `synt browser done {}` in another terminal and enter this code: {token}\nsynt: Ctrl-C cancels.\n",
                sanitize_for_display(&req.origin),
                req.profile,
                req.profile
            );
            let say = || {
                let _ = say_on_tty(&text);
            };
            let wrong = AtomicU8::new(0);
            let tick = || judge(check_signal(&req.done_file, &token), &wrong);
            let ended = t.ask(say, cancel, tick).await;
            if wrong.load(Ordering::SeqCst) >= MAX_WRONG_CODES {
                return Err(format!(
                    "{MAX_WRONG_CODES} wrong codes were written to the signal file; the login was cancelled"
                ));
            }
            ended.map(|_| ()).ok_or_else(|| "cancelled".to_string())
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

fn emit_login_required(sink: &EventSink, req: &LoginRequest) {
    sink.emit(
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
}

/// The interactive app's host (the CLI): the three questions are dialogs of the app's own
/// [`Approver`], in its permission-dialog style. They are NOT harness approvals (no approval
/// events, no "allow always"), and the permission mode cannot answer them. The login wait is
/// Enter/Esc in the dialog only: no one-time code, no signal file.
pub struct DialogHost {
    pub approver: Arc<dyn Approver>,
    pub sink: EventSink,
}

impl std::fmt::Debug for DialogHost {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("DialogHost")
    }
}

impl DialogHost {
    async fn ask(
        &self,
        tool: &str,
        tool_call_id: &str,
        input: Value,
        reason: String,
        cancel: &CancellationToken,
    ) -> bool {
        let req = ApprovalRequest {
            approval_id: uuid::Uuid::new_v4().to_string(),
            agent_id: None,
            tool_call_id: tool_call_id.to_owned(),
            tool: tool.to_owned(),
            input,
            dialog: "permission",
            prompt: reason.clone(),
            reason,
            suggestions: vec![],
            origin: None,
        };
        let a: Approval = self.approver.approve(req, cancel).await;
        a.decision == ApprovalDecision::Allow
    }
}

impl BrowserHost for DialogHost {
    fn login<'a>(
        &'a self,
        req: &'a LoginRequest,
        cancel: &'a CancellationToken,
    ) -> synthesis_tools::BoxFuture<'a, Result<(), String>> {
        Box::pin(async move {
            // The person at the dialog is the only channel: no code is shown and no signal file is
            // read (anything that can write the profile directory or capture the screen could
            // otherwise finish the login). `synt browser done` belongs to the headless host.
            emit_login_required(&self.sink, req);
            let reason = format!(
                "Log in to {} in the opened browser, then confirm. Enter = I am logged in, Esc = cancel.",
                sanitize_for_display(&req.origin)
            );
            let dialog = self.ask(
                "browser_login",
                &req.tool_call_id,
                serde_json::json!({"origin": req.origin, "profile": req.profile}),
                reason,
                cancel,
            );
            if dialog.await {
                Ok(())
            } else {
                Err("cancelled".to_string())
            }
        })
    }

    fn confirm_profile<'a>(
        &'a self,
        profile: &'a str,
        domains: &'a [String],
        cancel: &'a CancellationToken,
    ) -> synthesis_tools::BoxFuture<'a, bool> {
        Box::pin(async move {
            self.ask(
                "browser_navigate",
                "browser-host",
                serde_json::json!({"saved_login": profile, "logged_in_to": domains}),
                format!(
                    "The main agent asks to browse with saved login \"{}\" (logged in to: {}). Asked once per session; requests from sub-agents and peer sessions are refused before this dialog.",
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
            self.ask(
                "browser_fill_secret",
                "browser-host",
                serde_json::json!({"secret": secret, "origin": origin}),
                format!(
                    "Type stored secret \"{}\" into {}? The value is never shown. Asked once per session.",
                    sanitize_for_display(secret),
                    sanitize_for_display(origin)
                ),
                cancel,
            )
            .await
        })
    }
}

/// `synt browser done PROFILE`: ends the wait of a `browser_login` in another terminal. The
/// code printed in the terminal that waits is read from stdin (never from argv, which `ps` shows).
pub fn done_cmd(env: &Env, name: &str) -> Result<u8, String> {
    let dir =
        profile::profile_path(homes(env)?.synthesis.as_path(), name).map_err(|e| e.to_string())?;
    login_waiting(&dir)?; // before asking for anything
    if std::io::stdin().is_terminal() {
        eprint!("code shown by the waiting synt: ");
    }
    let token = read_code(std::io::stdin(), Duration::from_secs(120))?;
    signal_done(&dir, &token)?;
    out!("signalled {name}");
    Ok(0)
}

/// Lines for `synt doctor`.
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
            "secrets": {"ok": {"source": "{env:SYNT_A_PASSWORD}", "origins": ["https://a.com"]}, "bad": {}},
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
        assert_eq!(
            parse_settings(json!({"headed": false}).as_object().unwrap()).headed,
            Some(false)
        );
        assert_eq!(parse_settings(&Map::new()).headed, None);
        assert!(s.allow_unsandboxed);
        assert_eq!(s.approved_profiles, BTreeSet::from(["acme".to_string()]));
        assert_eq!(
            s.approved_secrets,
            BTreeSet::from([("ok".to_string(), "https://a.com".to_string())])
        );
        assert_eq!(s.warnings.len(), 5, "{:?}", s.warnings);
        let e = Env::from_pairs([("SYNT_A_PASSWORD", "hunter2-xyz")]);
        assert_eq!(s.values(&e), ["hunter2-xyz"]);
        let short = Env::from_pairs([("SYNT_A_PASSWORD", "abc")]);
        assert!(s.values(&short).is_empty(), "too short to mask by content");
        assert!(s.values(&Env::default()).is_empty());
        assert!(!parse_settings(&Map::new()).allow_unsandboxed);
    }

    #[test]
    fn secret_file_paths_are_listed_for_the_deny_list() {
        // a unix path is not absolute on Windows
        let pw = if cfg!(windows) {
            r"C:\Users\me\pw"
        } else {
            "/home/me/.config/pw"
        };
        let raw = json!({"secrets": {
            "f": {"source": format!("{{file:{pw}}}"), "origins": ["https://a.com"]},
            "e": {"source": "{env:SYNT_A_PASSWORD}", "origins": ["https://a.com"]}}});
        let s = parse_settings(raw.as_object().unwrap());
        assert_eq!(s.secret_files(), [std::path::PathBuf::from(pw)]);
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
        let raw = json!({"secrets": {"pw": {"source": "{env:SYNT_A_PASSWORD}", "origins": ["https://a.com"]}}});
        let s = parse_settings(raw.as_object().unwrap());
        let env = Env::from_pairs([("SYNT_A_PASSWORD", "Zq9-hunter2/S3CRET")]);
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

    /// The app's dialog, scripted: records every request, answers `answer` (`None` = waits until
    /// its dialog is withdrawn, like a person who has not decided yet).
    struct Dialogs {
        answer: Option<bool>,
        seen: std::sync::Mutex<Vec<ApprovalRequest>>,
        withdrawn: std::sync::atomic::AtomicBool,
    }

    impl Approver for Dialogs {
        fn approve<'a>(
            &'a self,
            req: ApprovalRequest,
            cancel: &'a CancellationToken,
        ) -> synthesis_harness::BoxFut<'a, Approval> {
            self.seen.lock().unwrap().push(req);
            Box::pin(async move {
                // like the real dialog: dropping the future (cancel, answered elsewhere) withdraws it
                struct Withdraw<'w>(&'w std::sync::atomic::AtomicBool);
                impl Drop for Withdraw<'_> {
                    fn drop(&mut self) {
                        self.0.store(true, Ordering::SeqCst);
                    }
                }
                let _w = (self.answer.is_none()).then(|| Withdraw(&self.withdrawn));
                match self.answer {
                    Some(true) => Approval::allow(),
                    Some(false) => Approval::deny("no"),
                    None => {
                        cancel.cancelled().await;
                        Approval::deny("withdrawn")
                    }
                }
            })
        }
    }

    fn dialogs(answer: Option<bool>) -> (Arc<Dialogs>, DialogHost, Buf) {
        let d = Arc::new(Dialogs {
            answer,
            seen: Default::default(),
            withdrawn: Default::default(),
        });
        let out = Buf::default();
        let host = DialogHost {
            approver: d.clone(),
            sink: EventSink::new(out.clone(), "s", Vec::<String>::new()),
        };
        (d, host, out)
    }

    fn login_req(dir: &Path) -> LoginRequest {
        LoginRequest {
            origin: "https://app.example.com".into(),
            profile: "acme".into(),
            timeout: Duration::from_secs(5),
            task_id: "t".into(),
            turn_id: "u".into(),
            tool_call_id: "c".into(),
            done_file: dir.join(DONE_FILE),
        }
    }

    #[tokio::test]
    async fn the_tui_dialogs_show_names_never_values_and_are_not_auto_answered() {
        let c = CancellationToken::new();
        let (d, host, _) = dialogs(Some(true));
        assert!(
            host.confirm_secret("acme-pw", "https://app.example.com", &c)
                .await
        );
        assert!(
            host.confirm_profile("acme", &["example.com".to_string()], &c)
                .await
        );
        {
            let seen = d.seen.lock().unwrap();
            assert_eq!(seen.len(), 2);
            let s = &seen[0];
            assert_eq!(
                (s.tool.as_str(), s.dialog),
                ("browser_fill_secret", "permission")
            );
            assert_eq!(
                s.input,
                serde_json::json!({"secret": "acme-pw", "origin": "https://app.example.com"})
            );
            assert!(s.reason.contains("acme-pw") && s.reason.contains("app.example.com"));
            assert!(s.suggestions.is_empty(), "no allow-always for a secret");
            assert!(seen[1].reason.contains("acme") && seen[1].reason.contains("example.com"));
        }
        // a person who says no (or Esc) is a no
        let (_, no, _) = dialogs(Some(false));
        assert!(
            !no.confirm_secret("acme-pw", "https://app.example.com", &c)
                .await
        );
        assert!(!no.confirm_profile("acme", &[], &c).await);
    }

    #[tokio::test]
    async fn the_tui_login_wait_is_enter_or_esc_with_no_code_and_no_signal_file() {
        let d = tempfile::tempdir().unwrap();
        let (dlg, host, out) = dialogs(Some(true));
        host.login(&login_req(d.path()), &CancellationToken::new())
            .await
            .unwrap();
        let reason = dlg.seen.lock().unwrap()[0].reason.clone();
        assert_eq!(
            reason,
            "Log in to https://app.example.com in the opened browser, then confirm. Enter = I am logged in, Esc = cancel."
        );
        assert!(
            !reason.contains("code") && !reason.contains("browser done"),
            "{reason}"
        );
        let ev = String::from_utf8(out.0.lock().unwrap().clone()).unwrap();
        assert!(
            ev.contains("browser.login_required") && ev.contains("acme"),
            "{ev}"
        );
        // nothing the dialog could be bypassed with was ever created
        assert!(!d.path().join(WAITING_FILE).exists() && !d.path().join(DONE_FILE).exists());
        // Esc / No cancels
        let (_, no, _) = dialogs(Some(false));
        assert_eq!(
            no.login(&login_req(d.path()), &CancellationToken::new())
                .await
                .unwrap_err(),
            "cancelled"
        );
        // a cancelled tool call withdraws the dialog
        let (pending, host, _) = dialogs(None);
        let c = CancellationToken::new();
        let c2 = c.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(100)).await;
            c2.cancel();
        });
        assert_eq!(
            host.login(&login_req(d.path()), &c).await.unwrap_err(),
            "cancelled"
        );
        assert!(
            pending.withdrawn.load(Ordering::SeqCst),
            "the dialog was withdrawn"
        );
    }

    #[tokio::test]
    async fn a_signal_file_cannot_finish_a_tui_login() {
        let d = tempfile::tempdir().unwrap();
        let (_dlg, host, _) = dialogs(None); // the person has not answered
        let dir = d.path().to_path_buf();
        let writer = tokio::spawn(async move {
            for _ in 0..8 {
                tokio::time::sleep(Duration::from_millis(100)).await;
                // whatever a process that can write the profile dir tries
                let _ = std::fs::write(dir.join(DONE_FILE), "guess");
                let _ = std::fs::write(dir.join(WAITING_FILE), "x");
            }
        });
        let r = tokio::time::timeout(
            Duration::from_secs(1),
            host.login(&login_req(d.path()), &CancellationToken::new()),
        )
        .await;
        assert!(r.is_err(), "still waiting for the person: {r:?}");
        writer.abort();
    }

    #[test]
    fn two_logins_on_one_profile_do_not_clobber_each_other() {
        let d = tempfile::tempdir().unwrap();
        let req = login_req(d.path());
        let first = Waiting::new(&req).unwrap();
        let e = Waiting::new(&req).err().unwrap();
        assert!(
            e.contains("another login is already waiting") && e.contains("acme"),
            "{e}"
        );
        // the refused login left the first one's files alone, and its drop removes nothing
        assert!(d.path().join(WAITING_FILE).exists());
        std::fs::write(&req.done_file, "tok").unwrap();
        drop(Waiting::new(&req).err());
        assert!(d.path().join(WAITING_FILE).exists() && req.done_file.exists());
        // a marker that is not this login's (replaced by another) is never removed by its Drop
        let marker = d.path().join(WAITING_FILE);
        std::fs::write(&marker, "someone-else").unwrap();
        drop(first);
        assert_eq!(std::fs::read_to_string(&marker).unwrap(), "someone-else");
        // ... but a crashed run's stale marker does not block forever
        let old = std::time::SystemTime::now() - Duration::from_secs(3 * 3600);
        std::fs::File::options()
            .write(true)
            .open(&marker)
            .unwrap()
            .set_modified(old)
            .unwrap();
        let second = Waiting::new(&req).unwrap();
        drop(second);
        assert!(!marker.exists());
    }

    #[test]
    fn wrong_codes_are_counted_and_the_fifth_cancels_the_login() {
        let n = AtomicU8::new(0);
        for i in 1..MAX_WRONG_CODES {
            assert!(!judge(Signal::Wrong, &n), "wrong code {i} is ignored");
            assert!(!judge(Signal::None, &n));
        }
        assert!(judge(Signal::Wrong, &n), "the fifth cancels");
        assert!(judge(Signal::Right, &AtomicU8::new(0)));
    }

    #[test]
    fn exposure_warnings_name_the_risk_and_the_servers() {
        let h = tempfile::tempdir().unwrap();
        let none = parse_settings(&Map::new());
        assert!(
            exposure_warnings(h.path(), &none, &["fs".into()], false).is_empty(),
            "nothing stored, nothing to warn about"
        );
        let dir = profile::create(h.path(), "acme").unwrap();
        std::fs::write(dir.join(profile::META_FILE), "{}").unwrap(); // a saved login
        let w = exposure_warnings(h.path(), &none, &["fs".into(), "git".into()], false);
        assert_eq!(w.len(), 2, "{w:?}");
        assert!(w[0].contains("1 saved login") && w[0].contains("--sandbox enforce"));
        assert!(w[1].contains("fs, git") && w[1].contains("allow_with_mcp"));
        // enforce silences the first; the MCP one needs the user's explicit setting
        assert_eq!(
            exposure_warnings(h.path(), &none, &["fs".into()], true).len(),
            1
        );
        let ok = parse_settings(json!({"allow_with_mcp": true}).as_object().unwrap());
        assert!(exposure_warnings(h.path(), &ok, &["fs".into()], true).is_empty());
        assert!(exposure_warnings(h.path(), &none, &[], true).is_empty());
    }

    #[test]
    fn stdio_servers_are_the_enabled_command_ones() {
        use synthesis_mcp::{McpServerConfig, from_opencode_shape};
        let mk = |n: &str, cfg: serde_json::Value, off: bool| -> McpServerConfig {
            let mut c = from_opencode_shape(n, &cfg, None);
            c.disabled = off;
            c
        };
        let all = [
            mk("a", json!({"type": "local", "command": ["x"]}), false),
            mk("b", json!({"type": "remote", "url": "https://x"}), false),
            mk("c", json!({"type": "local", "command": ["y"]}), true),
            mk("d", json!({"type": "local", "command": ["z"]}), false),
        ];
        assert_eq!(stdio_servers(&all), ["a", "d"]);
    }

    #[test]
    fn done_reads_a_bounded_code_and_eof_is_an_error() {
        let t = Duration::from_secs(5);
        assert_eq!(read_code(&b"abc\n"[..], t).unwrap(), "abc");
        assert!(read_code(&b""[..], t).unwrap_err().contains("no code"));
        assert!(read_code(&b"\n"[..], t).is_err());
        // a pipe that never writes nor closes: times out instead of hanging
        let (rd, _wr) = std::io::pipe().unwrap();
        let e = read_code(rd, Duration::from_millis(200)).unwrap_err();
        assert!(e.contains("within"), "{e}");
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
        assert_eq!(check_signal(&file, "tok"), Signal::Wrong);
        assert!(
            !file.exists(),
            "a wrong token is discarded, the marker stays"
        );
        assert!(d.path().join(WAITING_FILE).exists());
        assert_eq!(check_signal(&file, "tok"), Signal::None);
        signal_done(d.path(), "tok").unwrap();
        assert_eq!(check_signal(&file, "tok"), Signal::Right);
        assert!(!file.exists(), "consumed");
        #[cfg(unix)]
        {
            // a symlink planted as the signal file is never followed (neither read nor written)
            let secret = d.path().join("elsewhere");
            std::fs::write(&secret, "tok").unwrap();
            std::os::unix::fs::symlink(&secret, &file).unwrap();
            assert_eq!(
                check_signal(&file, "tok"),
                Signal::None,
                "symlink not followed"
            );
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
        // nobody to ask: refused, and no code was printed anywhere (stderr is never used for it)
        let e = host.login(&req, &CancellationToken::new()).await;
        assert!(e.unwrap_err().contains("no terminal"));
        assert!(!d.path().join(WAITING_FILE).exists(), "no wait was started");
    }
}
