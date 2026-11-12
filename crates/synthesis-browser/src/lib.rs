//! Browser runtime (R6): a headless Chrome-family browser driven over a minimal own CDP client.
//! One [`BrowserRuntime`] per Synthesis session: its own process, temp profile and browser context.
//! Everything derived from a page is returned as [`UntrustedText`]. See SPIKE.md for the design.

mod capture;
mod cdp;
mod discover;
mod error;
mod policy;
pub mod profile;
mod redact;
pub mod secret;
mod snapshot;
mod untrusted;

pub use capture::{ConsoleMessage, JsException, NetworkRequest};
pub use discover::{Os, candidates, discover};
pub use error::{BrowserError, Result};
pub use policy::{Resolver, UrlPolicy};
pub use redact::{MASK_MIN, mask_secret, redact_text, redact_url};
pub use untrusted::UntrustedText;

use base64::Engine;
use capture::Logs;
use cdp::{Cdp, CdpEvent};
use process_wrap::tokio::{ChildWrapper, CommandWrap};
use serde_json::{Value, json};
use snapshot::RefTable;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};
use synthesis_config::Env;
use tokio::sync::broadcast;
use tokio::task::JoinHandle;

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

#[derive(Clone, Debug)]
pub struct BrowserOptions {
    /// Where screenshots are written (kept after `close`, unlike the profile).
    pub artifact_dir: PathBuf,
    pub policy: UrlPolicy,
    /// Passes `--no-sandbox`. Only for containers where Chrome's sandbox cannot start (for
    /// example running as root in Docker). Off by default; never enable it for untrusted sites
    /// on a normal workstation.
    pub no_sandbox: bool,
    pub viewport: (u32, u32),
    pub transport: Transport,
    pub launch_timeout: Duration,
    pub navigation_timeout: Duration,
    /// Extra Chrome command-line flags (tests: `--host-resolver-rules`). Host-controlled, never
    /// from the agent; flags such as `--disable-web-security` would void the policy.
    pub chrome_args: Vec<String>,
    /// Capacity of the CDP event broadcast. If a page floods events past it, the session is
    /// torn down (fail closed); lowering it is for tests.
    pub event_capacity: usize,
    /// `false` opens a visible window (the login hand-off to a person). Default: headless.
    pub headless: bool,
    /// A persistent user-data-dir from `profile::create`/`existing` instead of a temp profile.
    /// The default browser context is used (cookies survive), the directory is locked while the
    /// browser runs and is never deleted.
    pub profile: Option<PathBuf>,
}

/// How the CDP connection is made.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Transport {
    /// `--remote-debugging-pipe`: no TCP port exists, only the two pipes held by this process.
    /// Unix only (Chrome on Windows wants raw handles, which needs `unsafe` to hand over).
    Pipe,
    /// `--remote-debugging-port=0` on 127.0.0.1: any local process can reach the port.
    Tcp,
}

impl Default for Transport {
    fn default() -> Self {
        if cfg!(unix) { Self::Pipe } else { Self::Tcp }
    }
}

impl BrowserOptions {
    pub fn new(artifact_dir: impl Into<PathBuf>) -> Self {
        Self {
            artifact_dir: artifact_dir.into(),
            policy: UrlPolicy::default(),
            no_sandbox: false,
            viewport: (1280, 800),
            transport: Transport::default(),
            launch_timeout: Duration::from_secs(60), // cold Chrome on a loaded CI runner is slow
            navigation_timeout: Duration::from_secs(30),
            chrome_args: vec![],
            event_capacity: 8192,
            headless: true,
            profile: None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WaitUntil {
    DomContentLoaded,
    Load,
    NetworkIdle,
}

#[derive(Clone, Debug)]
pub enum WaitFor {
    Text(String),
    Selector(String),
    NetworkIdle,
    Sleep(Duration),
}

#[derive(Clone, Debug)]
pub struct PageInfo {
    pub url: UntrustedText,
    pub title: UntrustedText,
}

#[derive(Clone, Debug)]
pub struct PageSnapshot {
    pub page: PageInfo,
    /// Compact accessibility tree; interactive nodes carry `[ref=eN]`.
    pub tree: UntrustedText,
    pub truncated: bool,
}

#[derive(Clone, Debug)]
pub struct TabInfo {
    pub index: usize,
    pub page: PageInfo,
    pub active: bool,
}

#[derive(Clone, Debug)]
pub struct Screenshot {
    pub png: Vec<u8>,
    pub path: PathBuf,
}

/// Where an action landed: a point in CSS pixels of the viewport, and the viewport size (`w` x `h`) it is
/// measured in. Position only; it never carries text.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Spot {
    pub x: f64,
    pub y: f64,
    pub w: u32,
    pub h: u32,
}

struct Tab {
    session: String,
    refs: Mutex<RefTable>,
}

#[derive(Default)]
struct Tabs {
    attached: HashMap<String, Arc<Tab>>,
    current: Option<String>,
    /// Page target ids in creation order. `Target.getTargets` order is not stable, so tab
    /// indices are derived from this list (popups are appended when first seen).
    order: Vec<String>,
}

pub struct BrowserRuntime {
    cdp: Arc<Cdp>,
    opts: BrowserOptions,
    context_id: String,
    logs: Arc<Mutex<Logs>>,
    tabs: Mutex<Tabs>,
    child: tokio::sync::Mutex<Option<Box<dyn ChildWrapper>>>,
    profile: Mutex<Option<tempfile::TempDir>>,
    events: JoinHandle<()>,
    /// Set when events were dropped (`Lagged`): the post-connect IP check can no longer be
    /// trusted, so the context is destroyed and the runtime refuses further use.
    compromised: Arc<AtomicBool>,
    pid: u32,
    shots: AtomicU32,
    /// Held while a persistent profile is in use (released on drop or close).
    profile_lock: Mutex<Option<profile::ProfileLock>>,
    /// Where `fill_secret` last typed: (tab session, document id, value too short to mask).
    /// Armed capture guards: (tab session, document id) -> a value too short to mask was typed.
    /// Armed BEFORE the fill is sent and kept whatever happens to the call.
    filled: Mutex<HashMap<(String, String), bool>>,
}

impl Drop for BrowserRuntime {
    fn drop(&mut self) {
        self.events.abort(); // the child itself is killed by KillOnDrop, the profile by TempDir
    }
}

fn spawn_events(
    cdp: &Arc<Cdp>,
    logs: Arc<Mutex<Logs>>,
    policy: UrlPolicy,
    context_id: String,
    compromised: Arc<AtomicBool>,
) -> JoinHandle<()> {
    let mut rx = cdp.subscribe();
    let weak = Arc::downgrade(cdp);
    tokio::spawn(async move {
        loop {
            let ev = match rx.recv().await {
                Ok(ev) => ev,
                Err(broadcast::error::RecvError::Lagged(_)) => {
                    // A dropped `Network.responseReceived` is a dropped DNS-rebinding check.
                    // Fail closed: destroy every page and worker of the context (that also
                    // stops in-flight loads), and kill the browser if even that fails.
                    compromised.store(true, Ordering::SeqCst);
                    if let Some(cdp) = weak.upgrade() {
                        let ctx = context_id.clone();
                        tokio::spawn(async move {
                            let r = cdp
                                .send(
                                    None,
                                    "Target.disposeBrowserContext",
                                    json!({"browserContextId": ctx}),
                                )
                                .await;
                            if r.is_err() {
                                let _ = cdp.send(None, "Browser.close", json!({})).await;
                            }
                        });
                    }
                    continue;
                }
                Err(broadcast::error::RecvError::Closed) => break,
            };
            if ev.method == "Target.attachedToTarget" {
                if let Some(cdp) = weak.upgrade() {
                    tokio::spawn(secure_child(cdp, ev.params));
                }
                continue;
            }
            if ev.method == "Network.responseReceived" {
                let remote = ev.params["response"]["remoteIPAddress"].as_str();
                let url = ev.params["response"]["url"].as_str().unwrap_or("");
                if remote.is_some_and(|ip| policy.remote_ip_denied(ip, url)) {
                    // Chrome connected to a private address although our DNS check passed
                    // (rebinding / own resolver). The request was already sent; drop the page.
                    if let (Some(cdp), Some(session)) = (weak.upgrade(), ev.session_id.clone()) {
                        let frame = ev.params["frameId"].clone();
                        let document = ev.params["type"] == "Document";
                        tokio::spawn(async move {
                            let s = Some(session.as_str());
                            let _ = cdp.send(s, "Page.stopLoading", json!({})).await;
                            if document {
                                let _ = cdp
                                    .send(
                                        s,
                                        "Page.navigate",
                                        json!({"url": "about:blank", "frameId": frame}),
                                    )
                                    .await;
                            }
                        });
                    }
                }
            }
            if ev.method != "Fetch.requestPaused" {
                lock(&logs).handle(&ev);
                continue;
            }
            // Every request (redirect hops, iframes, subresources) passes the URL policy.
            let p = &ev.params;
            let (Some(rid), Some(url)) = (p["requestId"].as_str(), p["request"]["url"].as_str())
            else {
                continue;
            };
            let document = p["resourceType"] == "Document";
            if let (Some(cdp), Some(session)) = (weak.upgrade(), ev.session_id.clone()) {
                let (rid, url, policy) = (rid.to_owned(), url.to_owned(), policy.clone());
                tokio::spawn(async move {
                    let verdict = policy.check_resolved(&url, document).await;
                    let _ = match verdict {
                        Ok(()) => {
                            cdp.send(
                                Some(&session),
                                "Fetch.continueRequest",
                                json!({"requestId": rid}),
                            )
                            .await
                        }
                        Err(_) => {
                            cdp.send(
                                Some(&session),
                                "Fetch.failRequest",
                                json!({"requestId": rid, "errorReason": "BlockedByClient"}),
                            )
                            .await
                        }
                    };
                });
            }
        }
    })
}

/// Schemes the policy refuses outright, blocked at the network layer in every session.
/// Neither this nor `Fetch` sees WebSocket handshakes in Chrome 156 (verified: the handshake
/// reaches the server with `ws://*` blocked), hence `NO_SOCKETS` as well.
const BLOCKED_URLS: [&str; 4] = ["ws://*", "wss://*", "ftp://*", "blob:*"];

/// WebSocket/WebTransport constructors are replaced in every document and worker. shortcut:
/// script-level, not network-level; a Chrome that exposes handshakes to Fetch should replace it.
const NO_SOCKETS: &str = "(()=>{for(const k of['WebSocket','WebTransport'])if(k in globalThis)Object.defineProperty(globalThis,k,{value:function(){throw new DOMException('blocked by URL policy','SecurityError')},writable:false,configurable:false})})()";

/// Installs the URL policy hooks on one session and makes it hand out new children paused.
async fn enable_policy(cdp: &Cdp, session: &str, evaluate: bool) -> Result<()> {
    let s = Some(session);
    cdp.send(
        s,
        "Fetch.enable",
        json!({"patterns": [{"urlPattern": "*", "requestStage": "Request"}]}),
    )
    .await?;
    let patterns: Vec<Value> = BLOCKED_URLS
        .iter()
        .map(|u| json!({"urlPattern": u, "block": true}))
        .collect();
    cdp.send(
        s,
        "Network.setBlockedURLs",
        json!({"urlPatterns": patterns}),
    )
    .await?;
    cdp.send(
        s,
        "Target.setAutoAttach",
        json!({"autoAttach": true, "waitForDebuggerOnStart": true, "flatten": true}),
    )
    .await?;
    if !evaluate {
        // without Page.enable the new-document script is not applied to a paused frame (a test caught the leak)
        cdp.send(s, "Page.enable", json!({})).await?;
    }
    let script = cdp.send(
        s,
        "Page.addScriptToEvaluateOnNewDocument",
        json!({"source": NO_SOCKETS, "runImmediately": true}),
    );
    // For a paused frame this is the only WebSocket guard, so it must succeed. Workers have no Page
    // domain and the main tab's context may not exist yet: both fall back to `Runtime.evaluate`.
    if evaluate {
        let _ = script.await;
    } else {
        script.await?;
    }
    // Not for a paused frame: its Runtime.evaluate is only answered after resume (the page load then
    // waits on this very call), and the new-document script above already covers it.
    if evaluate {
        let _ = cdp
            .send(s, "Runtime.evaluate", json!({"expression": NO_SOCKETS}))
            .await;
    }
    Ok(())
}

/// A popup, cross-site iframe or worker appeared: it starts paused, gets the policy hooks, and
/// only then resumes. If the hooks cannot be installed the target is closed, never resumed.
async fn secure_child(cdp: Arc<Cdp>, params: Value) {
    let Some(session) = params["sessionId"].as_str() else {
        return;
    };
    let frame = matches!(
        params["targetInfo"]["type"].as_str(),
        Some("page" | "iframe")
    );
    // shortcut: no Network.enable here, so the responseReceived rebinding check does not see child
    // sessions (only the pre-send DNS check in Fetch does). A child can be a second session on a
    // tab's own target, and enabling Network twice duplicates its log events (breaks the network log).
    if enable_policy(&cdp, session, !frame).await.is_err() {
        if let Some(t) = params["targetInfo"]["targetId"].as_str() {
            let _ = cdp
                .send(None, "Target.closeTarget", json!({"targetId": t}))
                .await;
        }
        return;
    }
    let _ = cdp
        .send(Some(session), "Runtime.runIfWaitingForDebugger", json!({}))
        .await;
}

/// Creates `dir` (0700, only when missing; an existing directory keeps its mode) and writes
/// `path` as 0600. shortcut: Windows relies on the default per-user ACLs.
fn write_private(
    dir: &std::path::Path,
    path: &std::path::Path,
    data: &[u8],
) -> std::io::Result<()> {
    use std::io::Write;
    let mut b = std::fs::DirBuilder::new();
    b.recursive(true);
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut b, 0o700);
    b.create(dir)?;
    let mut o = std::fs::OpenOptions::new();
    o.write(true).create(true).truncate(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut o, 0o600);
    let mut f = o.open(path)?;
    #[cfg(unix)]
    f.set_permissions(std::os::unix::fs::PermissionsExt::from_mode(0o600))?; // file pre-existed
    f.write_all(data)
}

/// (key, code, windows virtual key code, text)
fn key_def(key: &str) -> Option<(String, String, u32, Option<String>)> {
    let named = |k: &str, vk: u32, text: Option<&str>| {
        Some((k.to_owned(), k.to_owned(), vk, text.map(str::to_owned)))
    };
    match key {
        "Enter" => named("Enter", 13, Some("\r")),
        "Tab" => named("Tab", 9, None),
        "Escape" => named("Escape", 27, None),
        "Backspace" => named("Backspace", 8, None),
        "Delete" => named("Delete", 46, None),
        "ArrowLeft" => named("ArrowLeft", 37, None),
        "ArrowUp" => named("ArrowUp", 38, None),
        "ArrowRight" => named("ArrowRight", 39, None),
        "ArrowDown" => named("ArrowDown", 40, None),
        "Home" => named("Home", 36, None),
        "End" => named("End", 35, None),
        "PageUp" => named("PageUp", 33, None),
        "PageDown" => named("PageDown", 34, None),
        _ => {
            let mut chars = key.chars();
            let (c, None) = (chars.next()?, chars.next()) else {
                return None;
            };
            // shortcut: no modifier chords (Ctrl+A ...), add when a tool needs them.
            let code = match c {
                'a'..='z' | 'A'..='Z' => format!("Key{}", c.to_ascii_uppercase()),
                '0'..='9' => format!("Digit{c}"),
                ' ' => "Space".to_owned(),
                _ => String::new(),
            };
            let vk = if c.is_ascii_alphanumeric() {
                c.to_ascii_uppercase() as u32
            } else {
                c as u32
            };
            Some((c.to_string(), code, vk, Some(c.to_string())))
        }
    }
}

/// A launch failure that says what happened to the browser: its exit status (or that it is still
/// running) and the last 2 KiB of its stderr, so a CI log alone explains why it did not come up.
async fn launch_failure(
    child: &mut dyn ChildWrapper,
    stderr_path: &Path,
    exe: &Path,
    what: &str,
) -> BrowserError {
    // The pipe/socket closes a moment before the exit is reapable: give a dying browser time to be seen.
    for _ in 0..100 {
        if !matches!(child.try_wait(), Ok(None)) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let status = match child.try_wait() {
        Ok(Some(s)) => format!("exited ({s})"),
        Ok(None) => "still running".to_owned(),
        Err(e) => format!("status unknown ({e})"),
    };
    let bytes = std::fs::read(stderr_path).unwrap_or_default();
    let tail = String::from_utf8_lossy(&bytes[bytes.len().saturating_sub(2048)..]).into_owned();
    BrowserError::Launch(format!(
        "{what}; browser {} {status}; in a container or as root try the explicit no_sandbox option. stderr (last 2 KiB): {tail}",
        exe.display()
    ))
}

/// `ws://...` from the `DevTools listening on` line Chrome logs to stderr.
fn listening_url(stderr_path: &Path) -> Option<String> {
    let text = std::fs::read_to_string(stderr_path).ok()?;
    let rest = text.split("DevTools listening on ").nth(1)?;
    let url = rest.split_whitespace().next()?;
    url.starts_with("ws://127.0.0.1:").then(|| url.to_owned())
}

fn dir_listing(dir: &Path) -> String {
    let names: Vec<String> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .take(40)
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    if names.is_empty() {
        "(empty or unreadable)".into()
    } else {
        names.join(", ")
    }
}

/// Windows system variables Chrome and its helpers expect; `child_env` strips them. Non-secret.
/// A visible window needs the display the person is on; `child_env` (an allowlist) lacks it.
fn display_env(env: &Env, headless: bool) -> Vec<(String, String)> {
    if headless {
        return vec![];
    }
    ["DISPLAY", "WAYLAND_DISPLAY", "XAUTHORITY"]
        .iter()
        .filter_map(|k| env.get_nonempty(k).map(|v| ((*k).to_owned(), v.to_owned())))
        .collect()
}

fn windows_chrome_env(env: &Env) -> Vec<(String, String)> {
    if !cfg!(windows) {
        return vec![];
    }
    [
        "SystemDrive",
        "windir",
        "ProgramData",
        "ProgramFiles",
        "ProgramFiles(x86)",
        "ProgramW6432",
        "ALLUSERSPROFILE",
        "USERNAME",
        "USERDOMAIN",
        "COMPUTERNAME",
        "NUMBER_OF_PROCESSORS",
        "PROCESSOR_ARCHITECTURE",
        "CommonProgramFiles",
    ]
    .iter()
    .filter_map(|k| env.get(k).map(|v| ((*k).to_owned(), v.to_owned())))
    .collect()
}

/// The default browser context and its first page (a persistent profile's cookie jar).
async fn default_context(cdp: &Cdp) -> Result<(String, Option<String>)> {
    for _ in 0..100 {
        let r = cdp.send(None, "Target.getTargets", json!({})).await?;
        let first = r["targetInfos"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|t| t["type"] == "page");
        if let Some(t) = first {
            let ctx = t["browserContextId"]
                .as_str()
                .unwrap_or_default()
                .to_owned();
            return Ok((ctx, t["targetId"].as_str().map(str::to_owned)));
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    Err(BrowserError::Launch("the browser opened no page".into()))
}

impl BrowserRuntime {
    /// Discovers a browser (`SYNT_BROWSER_PATH` first), launches it headless with an isolated
    /// temp profile and opens one tab in a fresh browser context.
    pub async fn launch(opts: BrowserOptions, env: &Env) -> Result<Self> {
        if (opts.profile.is_some() || !opts.headless) && opts.transport == Transport::Tcp {
            return Err(BrowserError::Launch(
                "persistent profiles and the visible login window are unsupported with the TCP DevTools transport (Windows) until the pipe transport exists".into(),
            ));
        }
        let exe = discover(env, Os::current(), &|p| p.is_file())?;
        let profile_lock = opts.profile.as_deref().map(profile::lock).transpose()?;
        let profile = tempfile::Builder::new()
            .prefix("synthesis-browser-")
            .tempdir()?;
        let data_dir = opts
            .profile
            .clone()
            .unwrap_or_else(|| profile.path().join("data"));
        let _ = std::fs::remove_file(data_dir.join("DevToolsActivePort")); // a stale port of an earlier run
        if opts.profile.is_some() {
            profile::harden_preferences(&data_dir)?; // no password manager, no autofill, no save prompt
        }
        let stderr_path = profile.path().join("stderr.log");
        let stderr = std::fs::File::create(&stderr_path)?;
        let (w, h) = opts.viewport;
        std::fs::create_dir_all(&data_dir)?;
        // Chrome wants a plain `C:\...` path, never the `\\?\` verbatim form.
        let data_arg = data_dir.display().to_string();
        let data_arg = data_arg
            .strip_prefix(r"\\?\")
            .unwrap_or(&data_arg)
            .to_owned();

        let pipe = opts.transport == Transport::Pipe;
        let mut chrome_args = vec![if pipe {
            "--remote-debugging-pipe".to_owned()
        } else {
            "--remote-debugging-port=0".to_owned()
        }];
        if opts.headless {
            chrome_args.push("--headless=new".to_owned());
        }
        chrome_args.extend(
            [
                "--no-first-run",
                "--no-default-browser-check",
                "--disable-extensions",
                "--disable-sync",
                "--disable-background-networking",
                // bfcache would make back/forward skip the load event we wait for
                "--disable-features=BackForwardCache,Translate",
                "--disable-save-password-bubble",
                "--password-store=basic",
                "--use-mock-keychain",
            ]
            .map(str::to_owned),
        );
        chrome_args.push(format!("--user-data-dir={data_arg}"));
        if !pipe {
            // second discovery path: "DevTools listening on ws://..." on stderr
            chrome_args.push("--enable-logging=stderr".into());
        }
        chrome_args.push(format!("--window-size={w},{h}"));
        if opts.no_sandbox {
            chrome_args.push("--no-sandbox".into());
        }
        chrome_args.extend(opts.chrome_args.iter().cloned());
        chrome_args.push("about:blank".into());

        let mut wrap = if pipe {
            // Chrome reads CDP from fd 3 and writes to fd 4. Safe Rust cannot place a pipe on
            // fd 3/4, but the shell can: our child stdin/stdout become 3/4, then 0/1 go to null.
            CommandWrap::with_new("/bin/sh", |c| {
                c.args(["-c", r#"exec "$0" "$@" 3<&0 4>&1 </dev/null >/dev/null"#])
                    .arg(&exe)
                    .args(&chrome_args)
                    .env_clear()
                    .envs(env.child_env().iter())
                    .envs(display_env(env, opts.headless))
                    .stdin(Stdio::piped())
                    .stdout(Stdio::piped())
                    .stderr(stderr);
            })
        } else {
            CommandWrap::with_new(&exe, |c| {
                c.args(&chrome_args)
                    .env_clear()
                    .envs(env.child_env().iter())
                    .envs(display_env(env, opts.headless))
                    .envs(windows_chrome_env(env))
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(stderr);
            })
        };
        #[cfg(unix)]
        wrap.wrap(process_wrap::tokio::ProcessGroup::leader());
        #[cfg(windows)]
        wrap.wrap(process_wrap::tokio::JobObject);
        wrap.wrap(process_wrap::tokio::KillOnDrop);
        let mut child = wrap
            .spawn()
            .map_err(|e| BrowserError::Launch(format!("{}: {e}", exe.display())))?;
        let pid = child.id().unwrap_or(0);

        let cdp = if pipe {
            let (Some(w), Some(r)) = (child.stdin().take(), child.stdout().take()) else {
                return Err(BrowserError::Launch("no DevTools pipes".into()));
            };
            Arc::new(Cdp::from_pipe(r, w, opts.event_capacity))
        } else {
            let deadline = Instant::now() + opts.launch_timeout;
            let ws_url = loop {
                if let Ok(s) = std::fs::read_to_string(data_dir.join("DevToolsActivePort")) {
                    let mut l = s.lines();
                    if let (Some(port), Some(path)) = (l.next(), l.next()) {
                        break format!("ws://127.0.0.1:{}{}", port.trim(), path.trim());
                    }
                }
                if let Some(u) = listening_url(&stderr_path) {
                    break u;
                }
                if child.try_wait()?.is_some() {
                    return Err(launch_failure(
                        &mut *child,
                        &stderr_path,
                        &exe,
                        "browser exited before DevTools was ready",
                    )
                    .await);
                }
                if Instant::now() > deadline {
                    return Err(launch_failure(
                        &mut *child,
                        &stderr_path,
                        &exe,
                        &format!(
                            "browser did not publish its DevTools port in {} s; DevToolsActivePort {}; user-data-dir {} contains: {}",
                            opts.launch_timeout.as_secs(),
                            if data_dir.join("DevToolsActivePort").exists() { "exists" } else { "does not exist" },
                            data_arg,
                            dir_listing(&data_dir),
                        ),
                    )
                    .await);
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            };
            match Cdp::connect(&ws_url, opts.event_capacity).await {
                Ok(c) => Arc::new(c),
                Err(e) => {
                    return Err(
                        launch_failure(&mut *child, &stderr_path, &exe, &e.to_string()).await,
                    );
                }
            }
        };
        // A persistent profile keeps its cookies in the DEFAULT context; temp profiles get a
        // fresh isolated one.
        let ctx = if opts.profile.is_some() {
            default_context(&cdp).await
        } else {
            cdp.send(
                None,
                "Target.createBrowserContext",
                json!({"disposeOnDetach": true}),
            )
            .await
            .map(|c| {
                (
                    c["browserContextId"]
                        .as_str()
                        .unwrap_or_default()
                        .to_owned(),
                    None,
                )
            })
        };
        let (context_id, first_tab) = match ctx {
            Ok(c) => c,
            Err(e) => {
                return Err(launch_failure(&mut *child, &stderr_path, &exe, &e.to_string()).await);
            }
        };
        // Downloads are denied outright; there is no download size limit to enforce.
        cdp.send(
            None,
            "Browser.setDownloadBehavior",
            json!({"behavior": "deny", "browserContextId": context_id}),
        )
        .await?;
        let mut l = Logs::default();
        l.deny_private =
            !opts.policy.allow_private_network && opts.policy.loopback_ports.is_empty();
        let logs = Arc::new(Mutex::new(l));
        let compromised = Arc::new(AtomicBool::new(false));
        let events = spawn_events(
            &cdp,
            logs.clone(),
            opts.policy.clone(),
            context_id.clone(),
            compromised.clone(),
        );
        // Popups, workers and cross-site iframes are separate targets: pause each one until
        // `secure_child` has installed the policy hooks.
        cdp.send(
            None,
            "Target.setAutoAttach",
            json!({"autoAttach": true, "waitForDebuggerOnStart": true, "flatten": true}),
        )
        .await?;
        let rt = Self {
            cdp,
            opts,
            context_id,
            logs,
            tabs: Mutex::default(),
            child: tokio::sync::Mutex::new(Some(child)),
            profile: Mutex::new(Some(profile)),
            events,
            compromised,
            pid,
            shots: AtomicU32::new(0),
            profile_lock: Mutex::new(profile_lock),
            filled: Mutex::default(),
        };
        let first = match first_tab {
            Some(id) => rt.adopt(&id).await,
            None => rt.new_tab(None).await.map(|_| ()),
        };
        if let Err(e) = first {
            if let Some(c) = rt.child.lock().await.as_mut() {
                return Err(launch_failure(&mut **c, &stderr_path, &exe, &e.to_string()).await);
            }
            return Err(e);
        }
        Ok(rt)
    }

    /// Makes an existing page target (the default context's first tab) the current tab.
    async fn adopt(&self, id: &str) -> Result<()> {
        self.attach(id).await?;
        let mut t = lock(&self.tabs);
        t.order.push(id.to_owned());
        t.current = Some(id.to_owned());
        Ok(())
    }

    /// The temp profile directory; `None` after `close`.
    pub fn profile_dir(&self) -> Option<PathBuf> {
        lock(&self.profile).as_ref().map(|p| p.path().to_owned())
    }

    /// Pid of the browser process (group leader).
    pub fn pid(&self) -> u32 {
        self.pid
    }

    async fn call(&self, tab: &Tab, method: &str, params: Value) -> Result<Value> {
        self.cdp.send(Some(&tab.session), method, params).await
    }

    fn live(&self) -> Result<()> {
        if self.compromised.load(Ordering::SeqCst) {
            return Err(BrowserError::Compromised);
        }
        Ok(())
    }

    fn current(&self) -> Result<Arc<Tab>> {
        self.live()?;
        let t = lock(&self.tabs);
        t.current
            .as_ref()
            .and_then(|id| t.attached.get(id).cloned())
            .ok_or_else(|| BrowserError::Page("no open tab; open one with new_tab".into()))
    }

    async fn attach(&self, target_id: &str) -> Result<Arc<Tab>> {
        if let Some(t) = lock(&self.tabs).attached.get(target_id) {
            return Ok(t.clone());
        }
        let r = self
            .cdp
            .send(
                None,
                "Target.attachToTarget",
                json!({"targetId": target_id, "flatten": true}),
            )
            .await?;
        let session = r["sessionId"]
            .as_str()
            .ok_or(BrowserError::Closed)?
            .to_owned();
        let tab = Arc::new(Tab {
            session,
            refs: Mutex::default(),
        });
        for m in [
            "Page.enable",
            "Runtime.enable",
            "Network.enable",
            "Log.enable",
            "Accessibility.enable",
        ] {
            self.call(&tab, m, json!({})).await?;
        }
        enable_policy(&self.cdp, &tab.session, true).await?;
        if self.opts.headless {
            let (w, h) = self.opts.viewport;
            let m = json!({"width": w, "height": h, "deviceScaleFactor": 1, "mobile": false});
            self.call(&tab, "Emulation.setDeviceMetricsOverride", m)
                .await?;
        }
        lock(&self.tabs)
            .attached
            .insert(target_id.to_owned(), tab.clone());
        Ok(tab)
    }

    async fn eval(&self, tab: &Tab, expr: &str) -> Result<Value> {
        let r = self
            .call(
                tab,
                "Runtime.evaluate",
                json!({"expression": expr, "returnByValue": true}),
            )
            .await?;
        if let Some(d) = r.get("exceptionDetails") {
            return Err(BrowserError::Page(
                d["exception"]["description"]
                    .as_str()
                    .unwrap_or("evaluation failed")
                    .to_owned(),
            ));
        }
        Ok(r["result"]["value"].clone())
    }

    async fn info(&self, tab: &Tab) -> Result<PageInfo> {
        let v = self.eval(tab, "[location.href, document.title]").await?;
        Ok(PageInfo {
            url: UntrustedText::new(redact_url(v[0].as_str().unwrap_or(""))),
            title: UntrustedText::new(redact_text(v[1].as_str().unwrap_or(""))),
        })
    }

    async fn wait_event(
        &self,
        rx: &mut broadcast::Receiver<CdpEvent>,
        tab: &Tab,
        methods: &[&str],
    ) -> Result<()> {
        let deadline = tokio::time::Instant::now() + self.opts.navigation_timeout;
        loop {
            match tokio::time::timeout_at(deadline, rx.recv()).await {
                Err(_) => return Err(BrowserError::Timeout("page did not finish loading".into())),
                Ok(Err(broadcast::error::RecvError::Lagged(_))) => self.live()?,
                Ok(Err(_)) => return Err(BrowserError::Closed),
                Ok(Ok(ev)) => {
                    if ev.session_id.as_deref() == Some(&tab.session)
                        && methods.contains(&ev.method.as_str())
                    {
                        return Ok(());
                    }
                }
            }
        }
    }

    async fn wait_idle(&self, timeout: Duration) -> Result<()> {
        let deadline = Instant::now() + timeout;
        loop {
            {
                let l = lock(&self.logs);
                if l.in_flight == 0 && l.last_activity.elapsed() >= Duration::from_millis(500) {
                    return Ok(());
                }
            }
            if Instant::now() > deadline {
                return Err(BrowserError::Timeout("network did not become idle".into()));
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    async fn finish_load(
        &self,
        rx: &mut broadcast::Receiver<CdpEvent>,
        tab: &Tab,
        until: WaitUntil,
    ) -> Result<PageInfo> {
        let ev: &[&str] = match until {
            WaitUntil::DomContentLoaded => {
                &["Page.domContentEventFired", "Page.navigatedWithinDocument"]
            }
            _ => &["Page.loadEventFired", "Page.navigatedWithinDocument"],
        };
        self.wait_event(rx, tab, ev).await?;
        if until == WaitUntil::NetworkIdle {
            self.wait_idle(self.opts.navigation_timeout).await?;
        }
        self.info(tab).await
    }

    pub async fn navigate(&self, url: &str, until: WaitUntil) -> Result<PageInfo> {
        self.opts
            .policy
            .check_navigation(url)
            .map_err(BrowserError::Policy)?;
        let tab = self.current()?;
        let mut rx = self.cdp.subscribe();
        let r = self
            .call(&tab, "Page.navigate", json!({"url": url}))
            .await?;
        if let Some(e) = r["errorText"].as_str() {
            return Err(BrowserError::Page(e.to_owned()));
        }
        if r.get("loaderId").is_none() {
            return self.info(&tab).await; // same-document navigation: no load event
        }
        self.finish_load(&mut rx, &tab, until).await
    }

    async fn history(&self, delta: i64) -> Result<PageInfo> {
        let tab = self.current()?;
        let h = self
            .call(&tab, "Page.getNavigationHistory", json!({}))
            .await?;
        let i = h["currentIndex"].as_i64().unwrap_or(0) + delta;
        let entry = usize::try_from(i)
            .ok()
            .and_then(|i| h["entries"].get(i))
            .ok_or_else(|| BrowserError::Page("no history entry in that direction".into()))?;
        self.opts
            .policy
            .check_navigation(entry["url"].as_str().unwrap_or(""))
            .map_err(BrowserError::Policy)?;
        let mut rx = self.cdp.subscribe();
        self.call(
            &tab,
            "Page.navigateToHistoryEntry",
            json!({"entryId": entry["id"]}),
        )
        .await?;
        self.finish_load(&mut rx, &tab, WaitUntil::Load).await
    }

    pub async fn back(&self) -> Result<PageInfo> {
        self.history(-1).await
    }

    pub async fn forward(&self) -> Result<PageInfo> {
        self.history(1).await
    }

    /// This context's page targets, oldest first.
    async fn page_targets(&self) -> Result<Vec<Value>> {
        self.live()?;
        let r = self.cdp.send(None, "Target.getTargets", json!({})).await?;
        let mut pages: Vec<Value> = r["targetInfos"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|t| t["type"] == "page" && t["browserContextId"] == self.context_id.as_str())
            .cloned()
            .collect();
        let mut t = lock(&self.tabs);
        let live = |id: &String| pages.iter().any(|p| p["targetId"] == id.as_str());
        let mut order = std::mem::take(&mut t.order);
        order.retain(live);
        for p in &pages {
            let id = p["targetId"].as_str().unwrap_or_default();
            if !order.iter().any(|o| o == id) {
                order.push(id.to_owned());
            }
        }
        pages.sort_by_key(|p| order.iter().position(|o| p["targetId"] == o.as_str()));
        t.order = order;
        Ok(pages)
    }

    pub async fn tabs(&self) -> Result<Vec<TabInfo>> {
        let pages = self.page_targets().await?;
        let cur = lock(&self.tabs).current.clone();
        Ok(pages
            .iter()
            .enumerate()
            .map(|(index, t)| TabInfo {
                index,
                active: cur.as_deref() == t["targetId"].as_str(),
                page: PageInfo {
                    url: UntrustedText::new(redact_url(t["url"].as_str().unwrap_or(""))),
                    title: UntrustedText::new(redact_text(t["title"].as_str().unwrap_or(""))),
                },
            })
            .collect())
    }

    async fn target_at(&self, index: usize) -> Result<String> {
        self.page_targets()
            .await?
            .get(index)
            .and_then(|t| t["targetId"].as_str().map(str::to_owned))
            .ok_or_else(|| BrowserError::Page(format!("no tab at index {index}")))
    }

    /// Opens a tab in this session's context and makes it current; navigates when `url` is given.
    pub async fn new_tab(&self, url: Option<&str>) -> Result<PageInfo> {
        self.live()?;
        let r = self
            .cdp
            .send(
                None,
                "Target.createTarget",
                json!({"url": "about:blank", "browserContextId": self.context_id}),
            )
            .await?;
        let id = r["targetId"]
            .as_str()
            .ok_or(BrowserError::Closed)?
            .to_owned();
        let tab = self.attach(&id).await?;
        {
            let mut t = lock(&self.tabs);
            if !t.order.contains(&id) {
                t.order.push(id.clone());
            }
            t.current = Some(id);
        }
        match url {
            Some(u) => self.navigate(u, WaitUntil::Load).await,
            None => self.info(&tab).await,
        }
    }

    pub async fn select_tab(&self, index: usize) -> Result<()> {
        let id = self.target_at(index).await?;
        self.attach(&id).await?;
        self.cdp
            .send(None, "Target.activateTarget", json!({"targetId": id}))
            .await?;
        lock(&self.tabs).current = Some(id);
        Ok(())
    }

    pub async fn close_tab(&self, index: usize) -> Result<()> {
        let id = self.target_at(index).await?;
        self.cdp
            .send(None, "Target.closeTarget", json!({"targetId": id}))
            .await?;
        // closeTarget returns before the target is gone from getTargets.
        for _ in 0..40 {
            if self.target_at(index).await.ok().as_deref() != Some(id.as_str()) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        let mut t = lock(&self.tabs);
        t.attached.remove(&id);
        if t.current.as_deref() == Some(&id) {
            t.current = t
                .order
                .iter()
                .find(|o| t.attached.contains_key(*o))
                .cloned();
        }
        Ok(())
    }

    /// Accessibility snapshot of the current tab. Refs persist per element across snapshots.
    pub async fn snapshot(&self, max_nodes: usize) -> Result<PageSnapshot> {
        let tab = self.current()?;
        self.guard_capture(&tab, true).await?;
        let r = self
            .call(&tab, "Accessibility.getFullAXTree", json!({}))
            .await?;
        let nodes = r["nodes"].as_array().cloned().unwrap_or_default();
        let rendered = snapshot::render(&nodes, &mut lock(&tab.refs), max_nodes);
        Ok(PageSnapshot {
            page: self.info(&tab).await?,
            tree: UntrustedText::new(redact_text(&rendered.text)),
            truncated: rendered.truncated,
        })
    }

    /// Resolves a ref to a remote object id.
    async fn object_for(&self, tab: &Tab, r: &str) -> Result<String> {
        let backend = lock(&tab.refs)
            .resolve(r)
            .ok_or_else(|| BrowserError::StaleRef(r.to_owned()))?;
        let o = self
            .call(tab, "DOM.resolveNode", json!({"backendNodeId": backend}))
            .await;
        o.ok()
            .and_then(|o| o["object"]["objectId"].as_str().map(str::to_owned))
            .ok_or_else(|| BrowserError::StaleRef(r.to_owned()))
    }

    /// The viewport size in CSS pixels (the configured one when the browser does not say).
    async fn viewport(&self, tab: &Tab) -> (u32, u32) {
        let m = self
            .call(tab, "Page.getLayoutMetrics", json!({}))
            .await
            .ok();
        let n = |k: &str| {
            m.as_ref()
                .and_then(|m| m["cssVisualViewport"][k].as_f64())
                .map(|v| v.round() as u32)
        };
        match (n("clientWidth"), n("clientHeight")) {
            (Some(w), Some(h)) if w > 0 && h > 0 => (w, h),
            _ => self.opts.viewport,
        }
    }

    /// The centre of the element behind `r`, from the DOM box model (no script runs in the page), for the
    /// cursor of a viewer. Best effort: `None` when the element is gone or has no box.
    pub async fn spot_of(&self, r: &str) -> Option<Spot> {
        let tab = self.current().ok()?;
        let backend = lock(&tab.refs).resolve(r)?;
        let m = self
            .call(&tab, "DOM.getBoxModel", json!({"backendNodeId": backend}))
            .await
            .ok()?;
        let q: Vec<f64> = m["model"]["content"]
            .as_array()?
            .iter()
            .filter_map(Value::as_f64)
            .collect();
        if q.len() != 8 {
            return None;
        }
        let (x, y) = (
            (q[0] + q[2] + q[4] + q[6]) / 4.0,
            (q[1] + q[3] + q[5] + q[7]) / 4.0,
        );
        let (w, h) = self.viewport(&tab).await;
        Some(Spot { x, y, w, h })
    }

    /// Clicks the element behind `r` and returns where the pointer went.
    pub async fn click(&self, r: &str) -> Result<Spot> {
        let tab = self.current()?;
        let oid = self.object_for(&tab, r).await?;
        let f = "function(){this.scrollIntoView({block:'center',inline:'center'});const r=this.getBoundingClientRect();return [r.x+r.width/2,r.y+r.height/2,r.width*r.height]}";
        let v = self
            .call(
                &tab,
                "Runtime.callFunctionOn",
                json!({"objectId": oid, "functionDeclaration": f, "returnByValue": true}),
            )
            .await?;
        let p = &v["result"]["value"];
        let (x, y) = (p[0].as_f64().unwrap_or(0.0), p[1].as_f64().unwrap_or(0.0));
        if p[2].as_f64().unwrap_or(0.0) <= 0.0 {
            return Err(BrowserError::Page(format!("element {r} is not visible")));
        }
        for (t, extra) in [("mouseMoved", 0), ("mousePressed", 1), ("mouseReleased", 1)] {
            let mut ev = json!({"type": t, "x": x, "y": y, "button": "left", "clickCount": extra});
            if t == "mouseMoved" {
                ev["button"] = json!("none");
            }
            self.call(&tab, "Input.dispatchMouseEvent", ev).await?;
        }
        let (w, h) = self.viewport(&tab).await;
        Ok(Spot { x, y, w, h })
    }

    /// Focuses the element, replaces its content with `text` and fires input events.
    pub async fn fill(&self, r: &str, text: &str) -> Result<()> {
        let tab = self.current()?;
        let oid = self.object_for(&tab, r).await?;
        let f = "function(){this.focus();if(typeof this.select==='function')this.select();else if(this.isContentEditable)document.getSelection().selectAllChildren(this)}";
        self.call(
            &tab,
            "Runtime.callFunctionOn",
            json!({"objectId": oid, "functionDeclaration": f}),
        )
        .await?;
        self.call(&tab, "Input.insertText", json!({"text": text}))
            .await?;
        Ok(())
    }

    /// The element behind `r` as an object of an ISOLATED world of its frame: the page's own
    /// scripts cannot have replaced the prototypes or DOM getters the fill code relies on.
    async fn isolated_object(&self, tab: &Tab, r: &str) -> Result<String> {
        let backend = lock(&tab.refs)
            .resolve(r)
            .ok_or_else(|| BrowserError::StaleRef(r.to_owned()))?;
        let stale = || BrowserError::StaleRef(r.to_owned());
        // Refs come from the main frame's accessibility tree. An element of a child frame cannot be
        // resolved in the main frame's world: that fails below, which is the safe outcome.
        let tree = self.call(tab, "Page.getFrameTree", json!({})).await?;
        let frame = tree["frameTree"]["frame"]["id"]
            .as_str()
            .ok_or_else(stale)?
            .to_owned();
        let world = self
            .call(
                tab,
                "Page.createIsolatedWorld",
                json!({"frameId": frame, "worldName": "synthesis-secret"}),
            )
            .await?;
        let ctx = world["executionContextId"].as_i64().ok_or_else(stale)?;
        let o = self
            .call(
                tab,
                "DOM.resolveNode",
                json!({"backendNodeId": backend, "executionContextId": ctx}),
            )
            .await
            .map_err(|_| stale())?;
        o["object"]["objectId"]
            .as_str()
            .map(str::to_owned)
            .ok_or_else(stale)
    }

    /// Types a secret into the element behind `r` and nothing else. One in-page call, in an
    /// isolated world, verifies (page and frame origin, form targets, field type, visibility,
    /// not covered, not clipped) and writes, so nothing can change in between
    /// (`secret::FILL_JS`). The value is registered for masking first and never returned. The
    /// capture guard is armed BEFORE the call is sent, so a failed follow-up or a cancelled call
    /// cannot leave a typed value unguarded. `Ok(true)` = typed, but the page broke the
    /// `input`/`change` events ("maybe typed").
    pub async fn fill_secret(
        &self,
        r: &str,
        spec: &secret::SecretSpec,
        value: &str,
    ) -> Result<bool> {
        let short = !mask_secret(value);
        let tab = self.current()?;
        let oid = self.isolated_object(&tab, r).await?;
        let kind = match spec.field_kind {
            Some(secret::FieldKind::Password) => "password",
            Some(secret::FieldKind::Username) => "username",
            Some(secret::FieldKind::Totp) => "totp",
            None => "",
        };
        let doc = self.eval(&tab, "String(performance.timeOrigin)").await?;
        let key = (
            tab.session.clone(),
            doc.as_str().unwrap_or_default().to_owned(),
        );
        let fresh = {
            let mut f = lock(&self.filled);
            let was = f.get(&key).copied();
            f.insert(key.clone(), short || was == Some(true));
            was.is_none()
        };
        let v = self
            .call(
                &tab,
                "Runtime.callFunctionOn",
                json!({
                    "objectId": oid,
                    "functionDeclaration": secret::FILL_JS,
                    "arguments": [{"value": spec.origins}, {"value": kind}, {"value": value}],
                    "returnByValue": true,
                }),
            )
            .await?;
        let out = &v["result"]["value"];
        if out["ok"] == false {
            if fresh {
                lock(&self.filled).remove(&key); // the page refused before touching the field
            }
            let why = out["error"]
                .as_str()
                .unwrap_or("the page did not accept the fill");
            let why: String = redact_text(why).chars().take(200).collect();
            return Err(BrowserError::Refused(format!("{why}; nothing was typed")));
        }
        if out["ok"] != true {
            return Err(BrowserError::Page(
                "the fill raised an error in the page; the value may have been typed".into(),
            ));
        }
        Ok(out["maybe"] == true)
    }

    /// Screenshots, and snapshots/console/network/tab text after a value too short to mask, are
    /// refused while a document that received a secret is still the current one of its tab.
    /// `text_only`: only short values matter (longer ones are masked in text, not in pixels).
    async fn guard_capture(&self, tab: &Tab, text_only: bool) -> Result<()> {
        if !lock(&self.filled).keys().any(|(s, _)| *s == tab.session) {
            return Ok(());
        }
        let now = self.eval(tab, "String(performance.timeOrigin)").await?;
        let now = now.as_str().unwrap_or_default();
        let mut f = lock(&self.filled);
        f.retain(|(s, d), _| *s != tab.session || d == now); // a new document releases its tab
        if f.iter()
            .any(|((s, _), short)| *s == tab.session && (!text_only || *short))
        {
            return Err(BrowserError::Refused(
                "a secret was typed into this page: screenshots (and text from the page after a short secret) are refused until the page navigates".into(),
            ));
        }
        Ok(())
    }

    /// Fails while the current tab holds a short secret in its document: its console, network
    /// and tab listings, and page text, could echo a value the masking cannot recognise.
    pub async fn guard_text(&self) -> Result<()> {
        let tab = self.current()?;
        self.guard_capture(&tab, true).await
    }

    /// `scheme://host[:port]` of the current page, as the page itself reports it.
    pub async fn page_origin(&self) -> Result<String> {
        let tab = self.current()?;
        let o = self.eval(&tab, "location.origin").await?;
        Ok(o.as_str().unwrap_or_default().to_owned())
    }

    /// How many cookies the browser holds for `origin`. Only the count leaves this function.
    pub async fn cookie_count(&self, origin: &str) -> Result<usize> {
        let tab = self.current()?;
        let r = self
            .call(&tab, "Network.getCookies", json!({"urls": [origin]}))
            .await?;
        Ok(r["cookies"].as_array().map_or(0, Vec::len))
    }

    pub async fn press_key(&self, key: &str) -> Result<()> {
        let (k, code, vk, text) =
            key_def(key).ok_or_else(|| BrowserError::Page(format!("unsupported key `{key}`")))?;
        let tab = self.current()?;
        let mut down = json!({"type": if text.is_some() { "keyDown" } else { "rawKeyDown" }, "key": k, "code": code, "windowsVirtualKeyCode": vk});
        if let Some(t) = &text {
            down["text"] = json!(t);
        }
        self.call(&tab, "Input.dispatchKeyEvent", down).await?;
        let up = json!({"type": "keyUp", "key": k, "code": code, "windowsVirtualKeyCode": vk});
        self.call(&tab, "Input.dispatchKeyEvent", up).await?;
        Ok(())
    }

    pub async fn wait_for(&self, what: WaitFor, timeout: Duration) -> Result<()> {
        let expr = match &what {
            WaitFor::Text(t) => format!(
                "!!document.body && document.body.innerText.includes({})",
                json!(t)
            ),
            WaitFor::Selector(s) => format!("!!document.querySelector({})", json!(s)),
            WaitFor::NetworkIdle => return self.wait_idle(timeout).await,
            WaitFor::Sleep(d) => {
                tokio::time::sleep((*d).min(timeout)).await;
                return Ok(());
            }
        };
        let deadline = Instant::now() + timeout;
        loop {
            let tab = self.current()?;
            // An error here is usually a navigation tearing down the context: keep polling.
            if self.eval(&tab, &expr).await.is_ok_and(|v| v == true) {
                return Ok(());
            }
            if Instant::now() > deadline {
                return Err(BrowserError::Timeout(format!("wait_for {what:?}")));
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    pub async fn screenshot(&self, full_page: bool) -> Result<Screenshot> {
        let tab = self.current()?;
        self.guard_capture(&tab, false).await?;
        let mut params = json!({"format": "png"});
        if full_page {
            let m = self.call(&tab, "Page.getLayoutMetrics", json!({})).await?;
            let (w, h) = (
                m["cssContentSize"]["width"].as_f64().unwrap_or(0.0),
                m["cssContentSize"]["height"].as_f64().unwrap_or(0.0),
            );
            params["captureBeyondViewport"] = json!(true);
            // Chrome fails on huge captures, so cap the height.
            params["clip"] =
                json!({"x": 0, "y": 0, "width": w, "height": h.min(16384.0), "scale": 1});
        }
        let r = self.call(&tab, "Page.captureScreenshot", params).await?;
        let png = base64::engine::general_purpose::STANDARD
            .decode(r["data"].as_str().unwrap_or(""))
            .map_err(|e| BrowserError::Page(format!("bad screenshot data: {e}")))?;
        let n = self.shots.fetch_add(1, Ordering::Relaxed) + 1;
        let path = self.opts.artifact_dir.join(format!("screenshot-{n}.png"));
        write_private(&self.opts.artifact_dir, &path, &png)?;
        Ok(Screenshot { png, path })
    }

    /// Console messages from `cursor`, optionally one level. Returns the next cursor.
    pub fn console_messages(
        &self,
        cursor: usize,
        level: Option<&str>,
    ) -> (Vec<ConsoleMessage>, usize) {
        let l = lock(&self.logs);
        let out = l
            .console
            .iter()
            .skip(cursor)
            .filter(|m| level.is_none_or(|v| m.level == v))
            .cloned()
            .collect();
        (out, l.console.len())
    }

    pub fn exceptions(&self) -> Vec<JsException> {
        lock(&self.logs).exceptions.clone()
    }

    pub fn network_requests(
        &self,
        cursor: usize,
        failed_only: bool,
    ) -> (Vec<NetworkRequest>, usize) {
        let l = lock(&self.logs);
        let out = l
            .network
            .iter()
            .skip(cursor)
            .filter(|r| !failed_only || r.is_failure())
            .cloned()
            .collect();
        (out, l.network.len())
    }

    /// Kills the whole browser process tree and removes the profile. Idempotent.
    /// `Browser.close` first, so Chrome flushes the profile's cookies to disk; then `close`.
    pub async fn close_graceful(&self) -> Result<()> {
        let _ = tokio::time::timeout(
            Duration::from_secs(3),
            self.cdp.send(None, "Browser.close", json!({})),
        )
        .await;
        if let Some(child) = self.child.lock().await.as_mut() {
            let _ = tokio::time::timeout(Duration::from_secs(8), child.wait()).await;
        }
        self.close().await
    }

    pub async fn close(&self) -> Result<()> {
        self.events.abort();
        if let Some(mut child) = self.child.lock().await.take() {
            let _ = child.start_kill(); // errors mean "already gone"
            // Bounded: a Windows job-object wait must never hang a caller (CI sat 15 min).
            let _ = tokio::time::timeout(Duration::from_secs(10), child.wait()).await;
            // SIGKILL is asynchronous and the leader may already be reaped (graceful exit), so its
            // helpers (crashpad, zygote) can outlive `wait`: hold until the group is gone.
            #[cfg(unix)]
            {
                let mut left = 50;
                while group_alive(self.pid) {
                    left -= 1;
                    if left == 0 {
                        eprintln!(
                            "synthesis-browser: process group {} still has members 5 s after kill",
                            self.pid
                        );
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            }
        }
        lock(&self.profile_lock).take(); // the profile is free only once the browser is gone
        let profile = lock(&self.profile).take();
        if let Some(p) = profile {
            // Windows keeps files locked until every process of the job has exited: retry with a
            // bounded backoff, then leave the directory (it is under the OS temp dir) instead of failing.
            let path = p.keep();
            for attempt in 0..20 {
                match std::fs::remove_dir_all(&path) {
                    Ok(()) => return Ok(()),
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
                    Err(e) if attempt == 19 => {
                        eprintln!(
                            "synthesis-browser: could not remove profile {}: {e}",
                            path.display()
                        );
                    }
                    Err(_) => tokio::time::sleep(Duration::from_millis(100)).await,
                }
            }
        }
        Ok(())
    }
}

/// kill(-pgid, 0): does the group still have a member (a zombie counts)? pid 0 is never a group.
#[cfg(unix)]
fn group_alive(pgid: u32) -> bool {
    i32::try_from(pgid)
        .ok()
        .and_then(rustix::process::Pid::from_raw)
        .is_some_and(|p| rustix::process::test_kill_process_group(p).is_ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn group_alive_follows_a_real_group_and_never_pid_zero() {
        use std::os::unix::process::CommandExt;
        let mut c = std::process::Command::new("sleep");
        let mut child = c.arg("30").process_group(0).spawn().unwrap();
        assert!(group_alive(child.id()));
        child.kill().unwrap();
        child.wait().unwrap();
        assert!(!group_alive(child.id()));
        assert!(!group_alive(0));
    }

    #[tokio::test]
    async fn tcp_transport_refuses_persistent_profiles_and_the_login_window() {
        let env = Env::default();
        for (headless, profile) in [(true, Some(PathBuf::from("/p"))), (false, None)] {
            let mut o = BrowserOptions::new("/a");
            o.transport = Transport::Tcp;
            o.headless = headless;
            o.profile = profile;
            let e = BrowserRuntime::launch(o, &env)
                .await
                .err()
                .unwrap()
                .to_string();
            assert!(
                e.contains("unsupported") && e.contains("pipe transport"),
                "{e}"
            );
        }
    }

    #[test]
    fn stderr_listening_line_gives_the_ws_url() {
        let d = tempfile::tempdir().unwrap();
        let f = d.path().join("stderr.log");
        std::fs::write(
            &f,
            "[1:2:INFO] x\nDevTools listening on ws://127.0.0.1:5555/devtools/browser/abc\n",
        )
        .unwrap();
        assert_eq!(
            listening_url(&f).as_deref(),
            Some("ws://127.0.0.1:5555/devtools/browser/abc")
        );
        std::fs::write(&f, "DevTools listening on ws://evil.example:1/x\n").unwrap();
        assert_eq!(listening_url(&f), None);
    }

    #[test]
    fn key_table() {
        assert_eq!(key_def("Enter").unwrap().3.as_deref(), Some("\r"));
        assert_eq!(key_def("Tab").unwrap().2, 9);
        let (k, code, vk, text) = key_def("a").unwrap();
        assert_eq!(
            (k.as_str(), code.as_str(), vk, text.as_deref()),
            ("a", "KeyA", 65, Some("a"))
        );
        assert!(key_def("Ctrl+A").is_none());
        assert!(key_def("").is_none());
    }
}
