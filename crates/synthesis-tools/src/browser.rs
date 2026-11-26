//! browser_* tools on top of `synthesis_browser` (tools.md, doc §5.9). One lazily launched
//! `BrowserRuntime` per session lives in `SessionState`; it dies with the session. Every string
//! that came from a page is wrapped as `UntrustedText` so the harness can label it as data.

use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use base64::Engine;
use serde_json::{Value, json};
use synthesis_browser::payment::{self, Kind, PayTarget, Probe};
use synthesis_browser::profile::{self, DONE_FILE};
use synthesis_browser::secret::{self, SecretSpec};
use synthesis_browser::{
    BrowserError, BrowserOptions, BrowserRuntime, MouseOp, MouseTarget, Os, PageInfo,
    UntrustedText, UrlPolicy, WaitFor, WaitUntil,
};
use synthesis_config::Env;
use synthesis_core::ToolContext;
use tokio_util::sync::CancellationToken;

use crate::native::{Shared, lock};
use crate::util::{int_field, str_field};
use crate::{Block, BoxFuture, Content, ImageSource, ToolOutput};

type Launcher =
    Arc<dyn Fn() -> BoxFuture<'static, Result<BrowserRuntime, BrowserError>> + Send + Sync>;

/// Where a window that shows the session's browser (the desktop pane) finds it. Whichever of the agent or the window
/// launches the browser first registers it here, held weakly so the window never keeps Chrome alive past the session
/// (a browser the window started is also held by the tap itself, which lives and dies with the session).
#[derive(Default)]
pub struct BrowserTap {
    weak: std::sync::Mutex<std::sync::Weak<BrowserRuntime>>,
    started: std::sync::Mutex<Option<Arc<BrowserRuntime>>>,
    launcher: std::sync::Mutex<Option<Launcher>>,
    starting: tokio::sync::Mutex<()>,
}

impl std::fmt::Debug for BrowserTap {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("BrowserTap")
    }
}

impl BrowserTap {
    /// A view-only handle on the session's running browser, `None` before one is launched and after the
    /// session ends. It reaches the live view, navigation and tabs only, never the agent's capture APIs.
    pub fn view(&self) -> Option<synthesis_browser::PageView> {
        lock(&self.weak)
            .upgrade()
            .map(synthesis_browser::PageView::new)
    }

    /// The window may start the browser itself (`start`): headless, with the agent's URL policy and caps.
    pub fn enable_start(&self, cfg: BrowserConfig, artifact_dir: PathBuf, env: Env) {
        let env = cfg.process_env.clone().unwrap_or(env);
        *lock(&self.launcher) = Some(Arc::new(move || {
            let mut o = BrowserOptions::new(artifact_dir.clone());
            o.policy = cfg.policy.clone();
            o.no_sandbox = cfg.no_sandbox;
            let env = env.clone();
            Box::pin(async move { BrowserRuntime::launch(o, &env).await })
        }));
    }

    /// A launcher is set (not so in remote mode and in runs without browser tools).
    pub fn can_start(&self) -> bool {
        lock(&self.launcher).is_some()
    }

    /// Launches the session's browser unless one runs. The agent's tools then use this same browser and its strict
    /// URL policy.
    pub async fn start(&self) -> Result<(), String> {
        let _one_at_a_time = self.starting.lock().await;
        if self.view().is_some() {
            return Ok(());
        }
        let launch = lock(&self.launcher)
            .clone()
            .ok_or("this session cannot start a browser")?;
        let rt = Arc::new(launch().await.map_err(|e| e.to_string())?);
        *lock(&self.weak) = Arc::downgrade(&rt);
        *lock(&self.started) = Some(rt);
        Ok(())
    }
}

/// Browser settings of the host (set with `Registry::with_browser`).
#[derive(Debug, Clone, Default)]
pub struct BrowserConfig {
    /// A window shows this session's browser (the desktop): the runtime is registered here when the main agent
    /// launches it. The host also sets `headed` false then (a second visible window would take the person's clicks).
    pub view: Option<Arc<BrowserTap>>,
    pub policy: UrlPolicy,
    /// Only for containers where Chrome's own sandbox cannot start.
    pub no_sandbox: bool,
    /// The provider accepts image blocks in tool results: `browser_screenshot` returns the PNG.
    pub supports_images: bool,
    /// `synt.browser.screenshots` is `allow`: the PNG goes to the model even when the page may show
    /// credentials. Pixels cannot be masked, so the result text says so.
    pub allow_screenshots: bool,
    /// Synthesis home: login profiles live in `<home>/browser-profiles`. `None` disables
    /// `browser_login` and the `profile` option.
    pub home: Option<PathBuf>,
    /// `synt.browser.secrets` of the USER settings (`browser_fill_secret`).
    pub secrets: BTreeMap<String, SecretSpec>,
    /// User-widened domain lists per profile (`synt.browser.profiles.<name>.allow_domains`).
    pub profile_domains: BTreeMap<String, Vec<String>>,
    /// How long a login waits for the person (default 10 minutes).
    pub login_timeout: Option<Duration>,
    /// `synt.browser.headed`: the agent's own browser opens a VISIBLE window with a cursor overlay, when a display
    /// exists (else it stays headless). The profile, URL policy, downloads and credential rules do not change.
    pub headed: bool,
    /// The process environment snapshot. Used to resolve `{env:SYNT_...}` secret sources
    /// (the tool env is an allowlist that never carries secret-shaped names) and, when set, as
    /// the ONLY source for finding and starting the browser: a project's settings `env` cannot
    /// redirect `SYNT_BROWSER_PATH` to a binary that then receives a login or a secret.
    /// `None` (embedders, tests): secrets cannot be resolved and the tool env is used.
    pub process_env: Option<Env>,
    /// The commands the agent runs are confined (sandbox `enforce`). Without it, stored logins
    /// and secret files are readable by the agent's own tools, so the credential tools refuse.
    pub sandboxed: bool,
    /// User setting `synt.browser.allow_unsandboxed`: use the credential tools anyway.
    pub allow_unsandboxed: bool,
    /// Names of the stdio MCP servers of this run (they run outside the sandbox).
    pub mcp_stdio: Vec<String>,
    /// User setting `synt.browser.allow_with_mcp`: use the credential tools anyway.
    pub allow_with_mcp: bool,
    /// Profiles the person pre-approved in user settings (for runs nobody can answer).
    pub pre_approved_profiles: BTreeSet<String>,
    /// (secret, origin) pairs pre-approved in user settings.
    pub pre_approved_secrets: BTreeSet<(String, String)>,
    /// Who can be asked. `None`: `browser_login` and `browser_fill_secret` refuse.
    pub host: Option<Arc<dyn BrowserHost>>,
}

/// What a login hand-off tells the front-end.
#[derive(Debug, Clone)]
pub struct LoginRequest {
    pub origin: String,
    pub profile: String,
    pub timeout: Duration,
    pub task_id: String,
    pub turn_id: String,
    pub tool_call_id: String,
    /// `synt browser done` creates this file to end the wait when there is no terminal.
    pub done_file: PathBuf,
}

/// What a payment approval shows. All text came from the page or the model: the front-end sanitizes it for display.
#[derive(Debug, Clone)]
pub struct PaymentRequest {
    /// The tool and what it does ("browser_click click").
    pub action: String,
    pub origin: String,
    /// The target's label, clipped; empty when the page gave none.
    pub label: String,
    /// An amount found near the target, if any.
    pub amount: Option<String>,
    pub reason: &'static str,
}

/// The seam to the person (a CLI dialog, the headless terminal prompt). Both calls are in
/// addition to, never instead of, the permission engine, and neither may be auto-answered.
pub trait BrowserHost: Send + Sync + std::fmt::Debug {
    /// The visible browser is open on `req.origin`. Emit the `browser.login_required` event,
    /// tell the person ("Log in to ORIGIN in the opened browser, then confirm") and return
    /// `Ok` when they confirm. Dropping the future cancels the wait.
    fn login<'a>(
        &'a self,
        req: &'a LoginRequest,
        cancel: &'a CancellationToken,
    ) -> BoxFuture<'a, Result<(), String>>;

    /// First use of the saved login `profile` in this session: show its name and the domains it
    /// holds a login for; return whether the person agrees to browse with it.
    fn confirm_profile<'a>(
        &'a self,
        profile: &'a str,
        domains: &'a [String],
        cancel: &'a CancellationToken,
    ) -> BoxFuture<'a, bool>;

    /// The agent wants to browse (or, `page` being the same origin, to read the open page at) the loopback address
    /// `target` (`localhost:3000`) while the tab shows `page` (an origin, or empty): return whether the person agrees.
    /// Asked once per exact host:port and session. The default (no way to ask) refuses.
    fn confirm_loopback<'a>(
        &'a self,
        _target: &'a str,
        _page: &'a str,
        _cancel: &'a CancellationToken,
    ) -> BoxFuture<'a, bool> {
        Box::pin(async { false })
    }

    /// The agent's browser action may pay or commit money or consent (checkout, place order, card fields ...). Asked
    /// for EVERY such action, never remembered, never covered by an allow rule, "always allow" or a permission mode.
    /// The default (no way to ask) refuses.
    fn confirm_payment<'a>(
        &'a self,
        _req: &'a PaymentRequest,
        _cancel: &'a CancellationToken,
    ) -> BoxFuture<'a, bool> {
        Box::pin(async { false })
    }

    /// First use of `secret` on `origin` in this session: show the NAME and the origin (never the
    /// value) and return whether the person agrees.
    fn confirm_secret<'a>(
        &'a self,
        secret: &'a str,
        origin: &'a str,
        cancel: &'a CancellationToken,
    ) -> BoxFuture<'a, bool>;
}

type Res = Result<ToolOutput, Box<dyn Error + Send + Sync>>;

/// Input of a tool call with every secret removed, safe for events and logs. Today that is the
/// `value` of `browser_fill` with `sensitive: true`.
pub fn redact_input(tool: &str, input: &Value) -> Value {
    let mut v = input.clone();
    if tool == "browser_fill"
        && v["sensitive"] == true
        && let Some(o) = v.as_object_mut()
    {
        o.insert("value".into(), "[REDACTED]".into());
    }
    v
}

fn unavailable(tool: &str, e: impl std::fmt::Display) -> ToolOutput {
    ToolOutput::error(format!("{tool}: {e}")).with_meta(json!({"error": "browser_unavailable"}))
}

fn tool_env(ctx: &ToolContext) -> Env {
    Env::from_pairs(ctx.env.iter().map(|(k, v)| (k.clone(), v.clone())))
}

/// The environment the browser is found and started with (see `BrowserConfig::process_env`).
fn launch_env(cfg: &BrowserConfig, ctx: &ToolContext) -> Env {
    cfg.process_env.clone().unwrap_or_else(|| tool_env(ctx))
}

fn require_sandbox(cfg: &BrowserConfig) -> Result<(), BrowserError> {
    // MCP stdio servers are ordinary child processes of the session, outside the sandbox: they can
    // read the saved profiles and secret files whatever `sandboxed` says.
    if !cfg.mcp_stdio.is_empty() && !cfg.allow_with_mcp {
        return Err(BrowserError::Refused(format!(
            "stdio MCP server(s) {} run outside the sandbox and could read the saved logins and secret files; remove them or set synt.browser.allow_with_mcp to true in your user settings",
            cfg.mcp_stdio.join(", ")
        )));
    }
    if cfg.sandboxed || cfg.allow_unsandboxed {
        return Ok(());
    }
    Err(BrowserError::Refused("stored logins and secrets need the sandbox: run with --sandbox enforce (commands the agent runs could otherwise read the saved profile or secret files), or set synt.browser.allow_unsandboxed to true in your user settings".into()))
}

/// A person who said no twice to the same question is not asked a third time in the session.
const MAX_REFUSALS: u8 = 2;
/// Payment refusals of one origin (any path) after which nobody is asked again, and the most keys the table holds.
const MAX_PAYMENT_REFUSALS_PER_ORIGIN: u8 = 5;
const MAX_REFUSAL_KEYS: usize = 256;

fn refused_too_often(state: &crate::native::SessionState, key: &str) -> bool {
    lock(&state.refusals).get(key).copied().unwrap_or(0) >= MAX_REFUSALS
}

fn note_refusal(state: &crate::native::SessionState, key: &str) {
    *lock(&state.refusals).entry(key.to_owned()).or_default() += 1;
}

/// `profile`: a saved login (see `browser_login`). It can only be chosen when the session's
/// browser starts, and then limits top-level navigation to the domains the person logged in to.
async fn runtime(
    sh: &Shared,
    ctx: &ToolContext,
    tool: &str,
    profile: Option<&str>,
) -> Result<Arc<BrowserRuntime>, ToolOutput> {
    let cfg = lock(&sh.browser_cfg).clone();
    let state = sh.session(ctx);
    let rt = state
        .browser
        .get_or_try_init(|| async {
            // one launch at a time with the window's Start: two Chromes cannot both register
            let _one = match &cfg.view {
                Some(t) => Some(t.starting.lock().await),
                None => None,
            };
            // the person started it from the window: the session uses that browser
            if let Some(rt) = cfg.view.as_ref().and_then(|t| lock(&t.started).clone()) {
                return Ok(rt);
            }
            if let Some(rt) = cfg.view.as_ref().and_then(|t| lock(&t.weak).upgrade()) {
                return Ok(rt);
            }
            let mut o = BrowserOptions::new(ctx.workspace.artifact_root.join("browser"));
            o.policy = cfg.policy.clone();
            o.no_sandbox = cfg.no_sandbox;
            // visible only where a display exists and the pipe transport can carry a window (not Windows' TCP one)
            o.headless = headless_for(
                cfg.headed,
                o.transport,
                profile::display_available(&launch_env(&cfg, ctx), Os::current()).is_ok(),
            );
            if let Some(name) = profile {
                // choosing a saved login is the local person's decision, like logging in
                if ctx.sub_agent {
                    return Err(BrowserError::Refused(
                        "saved logins are not available to sub-agents".into(),
                    ));
                }
                require_sandbox(&cfg)?;
                let home = cfg.home.as_deref().ok_or_else(|| {
                    BrowserError::Launch("browser profiles are not available here".into())
                })?;
                let dir = profile::existing(home, name)?;
                let meta = profile::read_meta(&dir);
                let mut domains = meta.domains;
                domains.extend(cfg.profile_domains.get(name).cloned().unwrap_or_default());
                if domains.is_empty() {
                    return Err(BrowserError::Launch(format!(
                        "browser profile {name:?} has no recorded domain"
                    )));
                }
                // The person decides, per session, whether the agent may browse as this login.
                if !cfg.pre_approved_profiles.contains(name) {
                    let Some(host) = &cfg.host else {
                        return Err(BrowserError::Refused("nobody can approve using a saved login here".into()));
                    };
                    let key = format!("profile:{name}");
                    if refused_too_often(&state, &key) {
                        return Err(BrowserError::Refused(format!(
                            "the person already refused saved login {name:?} twice in this session; not asking again"
                        )));
                    }
                    if !host.confirm_profile(name, &domains, &ctx.cancel).await {
                        note_refusal(&state, &key);
                        return Err(BrowserError::Refused(format!(
                            "the person did not approve browsing with saved login {name:?}"
                        )));
                    }
                }
                o.policy.loopback_ports = meta.loopback_ports;
                o.policy.document_allow_domains = domains;
                o.profile = Some(dir);
                *lock(&state.browser_profile) = Some(name.to_owned());
            }
            let rt = BrowserRuntime::launch(o, &launch_env(&cfg, ctx))
                .await
                .map(Arc::new)?;
            // the runtime is the SESSION's (a sub-agent shares it with the main agent), so whichever call wins the
            // launch registers it
            if let Some(tap) = &cfg.view {
                *lock(&tap.weak) = Arc::downgrade(&rt);
            }
            Ok(rt)
        })
        .await
        .cloned()
        .map_err(|e| unavailable(tool, e))?;
    if let Some(want) = profile
        && lock(&state.browser_profile).as_deref() != Some(want)
    {
        return Err(unavailable(
            tool,
            format!(
                "this session's browser is already running without profile {want:?}; a profile can only be chosen by the first browser call"
            ),
        ));
    }
    Ok(rt)
}

/// Visible only when asked for, a display exists and the pipe transport can carry a window (not Windows' TCP one).
fn headless_for(headed: bool, transport: synthesis_browser::Transport, display: bool) -> bool {
    !(headed && display && transport == synthesis_browser::Transport::Pipe)
}

#[cfg(test)]
mod headed_tests {
    use super::headless_for;
    use synthesis_browser::Transport::{Pipe, Tcp};

    #[test]
    fn the_window_is_visible_only_when_asked_for_and_possible() {
        assert!(!headless_for(true, Pipe, true));
        assert!(headless_for(false, Pipe, true), "not asked for");
        assert!(headless_for(true, Pipe, false), "no display");
        assert!(headless_for(true, Tcp, true), "Windows TCP transport");
    }
}

/// Tool error for a failed browser call.
fn fail(wire: &str, e: Box<dyn Error + Send + Sync>) -> ToolOutput {
    let kind = match e.downcast_ref::<BrowserError>() {
        Some(BrowserError::Policy(_)) => "url_policy",
        Some(BrowserError::StaleRef(_)) => "stale_ref",
        Some(BrowserError::Timeout(_)) => "timeout",
        Some(BrowserError::Refused(_)) => "refused",
        _ => "browser_error",
    };
    ToolOutput::error(format!("{wire}: {e}")).with_meta(json!({"error": kind}))
}

fn untrusted(s: String) -> String {
    UntrustedText::new(s).to_string()
}

fn page_text(p: &PageInfo) -> String {
    format!("url: {}\ntitle: {}", p.url.expose(), p.title.expose())
}

macro_rules! browser_tool {
    ($name:ident, $wire:literal, $body:ident) => {
        pub(crate) fn $name<'a>(
            sh: &'a Shared,
            input: &'a Value,
            ctx: &'a ToolContext,
        ) -> BoxFuture<'a, ToolOutput> {
            Box::pin(async move {
                let rt = match runtime(sh, ctx, $wire, str_field(input, "profile")).await {
                    Ok(rt) => rt,
                    Err(e) => return e,
                };
                let _busy = rt.agent_busy(); // the person's input to the page waits for the agent's call
                // a loopback page the person opened is not the agent's to read or act on until the person approves it
                if $wire != "browser_close"
                    && let Err(e) =
                        approve_loopback(sh, ctx, &rt, current_page(&rt).await.as_deref()).await
                {
                    return fail($wire, e.into());
                }
                // console, network and tab data are browser-wide: every loopback page the person had shown needs approval
                if matches!(
                    $wire,
                    "browser_console" | "browser_network" | "browser_tabs"
                ) && let Err(e) = approve_taint(sh, ctx, &rt).await
                {
                    return fail($wire, e.into());
                }
                let mut unchecked = false;
                let mut checked: Option<Value> = None;
                if matches!(
                    $wire,
                    "browser_click" | "browser_mouse" | "browser_fill" | "browser_press_key"
                ) {
                    match payment_gate(sh, ctx, &rt, $wire, input).await {
                        Ok((u, r)) => (unchecked, checked) = (u, r),
                        Err(e) => return fail($wire, e.into()),
                    }
                }
                // the body acts on the element the guard checked
                let input = checked.as_ref().unwrap_or(input);
                let r = tokio::select! {
                    r = $body(&rt, input, sh) => r,
                    () = ctx.cancel.cancelled() => Err("aborted.".into()),
                };
                note_unchecked(r.unwrap_or_else(|e| fail($wire, e)), unchecked)
            })
        }
    };
}

/// Records in the result that the payment check could not run (so a reviewer sees the action went unchecked).
fn note_unchecked(mut o: ToolOutput, unchecked: bool) -> ToolOutput {
    if unchecked {
        if o.meta.is_null() {
            o.meta = json!({});
        }
        o.meta["payment_check"] = json!("unavailable");
    }
    o
}

/// What the guard reads and asks about, from the tool input.
struct PayPlan {
    target: PayTarget,
    kind: Kind,
    what: String,
    /// The input with a role+name target replaced by the ref the guard resolved, so the body acts on the very
    /// element that was checked (a page that changes between the two resolutions cannot swap it).
    resolved: Option<Value>,
}

/// `None` for actions that pay nothing (hover, wheel, a printable key outside a card field's reach) and for input the
/// tool itself rejects. A role+name that does not resolve is an error here: the body would fail the same way.
async fn pay_plan(
    rt: &BrowserRuntime,
    wire: &str,
    input: &Value,
) -> Result<Option<PayPlan>, BrowserError> {
    let plan = |target, kind, what: &str| {
        Ok(Some(PayPlan {
            target,
            kind,
            what: what.to_owned(),
            resolved: None,
        }))
    };
    // role+name becomes a ref in a copy of the input
    let resolve = async |a: Aim<'_>| -> Result<(PayTarget, Option<Value>), BrowserError> {
        Ok(match a {
            Aim::Ref(r) => (PayTarget::Ref(r.to_owned()), None),
            Aim::Role(role, name) => {
                let r = ref_by_role(rt, role, name)
                    .await
                    .map_err(BrowserError::Page)?;
                let mut v = input.clone();
                if let Some(o) = v.as_object_mut() {
                    o.remove("role");
                    o.remove("name");
                    o.insert("ref".into(), json!(r));
                }
                (PayTarget::Ref(r), Some(v))
            }
            Aim::Point(x, y) => (PayTarget::Point(x, y), None),
        })
    };
    match wire {
        "browser_click" => {
            let a = match (
                str_field(input, "ref"),
                str_field(input, "role"),
                str_field(input, "name"),
            ) {
                (Some(r), ..) => Aim::Ref(r),
                (None, Some(role), Some(name)) => Aim::Role(role, name),
                _ => return Ok(None),
            };
            let (target, resolved) = resolve(a).await?;
            Ok(Some(PayPlan {
                target,
                kind: Kind::Click,
                what: "click".into(),
                resolved,
            }))
        }
        "browser_mouse" => {
            let Some(act) = str_field(input, "action") else {
                return Ok(None);
            };
            if !matches!(act, "click" | "double_click" | "right_click" | "drag") {
                return Ok(None);
            }
            let Some(a) = aim(input, "").ok().flatten() else {
                return Ok(None);
            };
            let (target, resolved) = resolve(a).await?;
            // a point, a double click or a drag cannot be vetted by its label
            let blind =
                matches!(target, PayTarget::Point(..)) || matches!(act, "double_click" | "drag");
            Ok(Some(PayPlan {
                target,
                kind: if blind { Kind::Blind } else { Kind::Click },
                what: act.into(),
                resolved,
            }))
        }
        "browser_fill" | "browser_fill_secret" => match str_field(input, "ref") {
            Some(r) => plan(PayTarget::Ref(r.to_owned()), Kind::Fill, "type into"),
            None => Ok(None),
        },
        "browser_press_key" => {
            let Some(k) = str_field(input, "key") else {
                return Ok(None);
            };
            match k.rsplit('+').next() {
                Some("Enter" | " ") => plan(PayTarget::Focused, Kind::Key, &format!("press {k}")),
                // a printable key types into the focused element: a card field asks, as `browser_fill` does
                Some(c) if c.chars().count() == 1 => {
                    plan(PayTarget::Focused, Kind::Fill, "type a key into")
                }
                _ => Ok(None),
            }
        }
        _ => Ok(None),
    }
}

/// The payment guard (owner decision 2026-11-20): an action that may pay or commit money or consent needs the
/// person's OWN approval for this action, now. No allow rule, "always allow", permission mode, sub-agent or earlier
/// answer covers it, and nothing is remembered. Nobody to ask: refused. Returns `true` when the check could not run
/// (browsing goes on as before, the result says so).
async fn payment_gate(
    sh: &Shared,
    ctx: &ToolContext,
    rt: &BrowserRuntime,
    wire: &str,
    input: &Value,
) -> Result<(bool, Option<Value>), BrowserError> {
    let Some(PayPlan {
        target,
        kind,
        what,
        resolved,
    }) = pay_plan(rt, wire, input).await?
    else {
        return Ok((false, None));
    };
    // origin and path come from the browser's own tab list, not from the page's answer
    let page = current_page(rt).await;
    let mut unread = false;
    let (probe, reason) = match rt.payment_probe(&target).await {
        Ok(mut p) => {
            if let Some(u) = &page {
                p.href.clone_from(u);
            }
            let r = payment::assess(&p, kind);
            (p, r)
        }
        // detection failed on a page that already looks like a payment page: treat it as one
        Err(_) if page.as_deref().is_some_and(payment::page_hit) => {
            unread = true;
            (
                Probe {
                    href: page.clone().unwrap_or_default(),
                    ..Default::default()
                },
                Some("the target could not be read on a payment page"),
            )
        }
        Err(_) => return Ok((true, None)),
    };
    let Some(reason) = reason else {
        return Ok((false, resolved));
    };
    let origin =
        url::Url::parse(&probe.href).map_or(String::new(), |u| u.origin().ascii_serialization());
    let how = format!(
        "{wire} {what} on {origin}: {reason}. Payment and consent actions need the person's own approval every time (never covered by an allow rule, a mode or an earlier answer)"
    );
    let host = lock(&sh.browser_cfg).host.clone();
    let Some(host) = host.filter(|_| !ctx.sub_agent) else {
        return Err(BrowserError::Refused(format!(
            "{how}; nobody can be asked in this run, so it is refused"
        )));
    };
    let state = sh.session(ctx);
    // per page, not per origin: a refusal on one page does not silence the question on the next
    let path = url::Url::parse(&probe.href).map_or(String::new(), |u| u.path().to_owned());
    let key = format!("payment:{origin}{path}");
    let origin_key = format!("payment:{origin}");
    let flooded = {
        let r = lock(&state.refusals);
        // an origin cannot keep asking by changing its path; the table itself stays small
        r.get(&origin_key).copied().unwrap_or(0) >= MAX_PAYMENT_REFUSALS_PER_ORIGIN
            || r.len() >= MAX_REFUSAL_KEYS
    };
    if flooded || refused_too_often(&state, &key) {
        return Err(BrowserError::Refused(format!(
            "{how}; the person already refused it on this origin in this session, so it is not asked again"
        )));
    }
    let req = PaymentRequest {
        action: format!("{wire} {what}"),
        origin,
        label: payment::clean(&synthesis_browser::redact_text(&probe.label), 80),
        amount: payment::amount(&synthesis_browser::redact_text(&probe.amount_text)),
        reason,
    };
    if !host.confirm_payment(&req, &ctx.cancel).await {
        note_refusal(&state, &key);
        note_refusal(&state, &origin_key);
        return Err(BrowserError::Refused(format!(
            "{how}; the person did not approve it"
        )));
    }
    // one-shot and bound to what was shown: a page that changed its target meanwhile is not approved
    let mut now = rt.payment_probe(&target).await;
    if let (Ok(n), Some(u)) = (now.as_mut(), current_page(rt).await) {
        n.href = u;
    }
    match now {
        // an unreadable target cannot be compared: the person's answer stands for this one action
        Err(_) if unread => Ok((false, resolved)),
        Ok(now) if payment::same_target(&probe, &now) => Ok((false, resolved)),
        _ => Err(BrowserError::Refused(format!(
            "{how}; the target changed after the person approved, so nothing was done. Look at the page again"
        ))),
    }
}
/// `browser_navigate`: like the other browser tools, plus the person's approval for a loopback address (a dev server).
pub(crate) fn navigate<'a>(
    sh: &'a Shared,
    input: &'a Value,
    ctx: &'a ToolContext,
) -> BoxFuture<'a, ToolOutput> {
    Box::pin(async move {
        let wire = "browser_navigate";
        let rt = match runtime(sh, ctx, wire, str_field(input, "profile")).await {
            Ok(rt) => rt,
            Err(e) => return e,
        };
        let _busy = rt.agent_busy();
        if let Err(e) = approve_loopback(sh, ctx, &rt, str_field(input, "url")).await {
            return fail(wire, e.into());
        }
        let r = tokio::select! {
            r = nav(&rt, input, sh) => r,
            () = ctx.cancel.cancelled() => Err("aborted.".into()),
        };
        r.unwrap_or_else(|e| fail(wire, e))
    })
}

/// Approval for every loopback target a person-origin page has shown in this browser that the agent has no approval
/// for yet; the first refusal fails the call (nothing is redacted silently).
async fn approve_taint(
    sh: &Shared,
    ctx: &ToolContext,
    rt: &BrowserRuntime,
) -> Result<(), BrowserError> {
    for t in rt.policy().unapproved_taint() {
        approve_loopback(sh, ctx, rt, Some(&format!("http://{t}/"))).await?;
    }
    Ok(())
}

/// Distinct loopback targets the person may refuse in one session before nobody is asked again.
const MAX_LOOPBACK_REFUSED: usize = 3;

/// URL of the tab the agent would read or act on.
async fn current_page(rt: &BrowserRuntime) -> Option<String> {
    let tabs = rt.tabs().await.ok()?;
    tabs.iter()
        .find(|t| t.active)
        .map(|t| t.page.url.expose().to_owned())
}

/// The agent's URL policy refuses loopback until the person approves that exact host:port (remembered for the
/// browser's life). LAN, link-local and metadata addresses have no approval path; nobody to ask (`synt run` without a
/// terminal) keeps it blocked.
async fn approve_loopback(
    sh: &Shared,
    ctx: &ToolContext,
    rt: &BrowserRuntime,
    url: Option<&str>,
) -> Result<(), BrowserError> {
    let Some(url) = url else { return Ok(()) };
    let Some(target) = synthesis_browser::loopback_target(url) else {
        return Ok(());
    };
    if rt.policy().check_navigation(url).is_ok() {
        return Ok(());
    }
    // refused for another reason (deny list, profile domains): that reason, and nobody is asked
    rt.policy()
        .check_navigation_if_approved(url)
        .map_err(BrowserError::Policy)?;
    let key = format!("loopback:{target}");
    let state = sh.session(ctx);
    let host = lock(&sh.browser_cfg).host.clone();
    // the person is not asked again and again about new targets
    let refused_targets = lock(&state.refusals)
        .iter()
        .filter(|(k, n)| k.starts_with("loopback:") && **n > 0)
        .count();
    let how = format!(
        "{target} is a loopback address: the agent may browse it only after the person approves it (asked once per session and exact host:port)"
    );
    let Some(host) = host else {
        return Err(BrowserError::Policy(format!(
            "{how}; nobody can be asked in this run, so it stays blocked"
        )));
    };
    if refused_too_often(&state, &key) || refused_targets >= MAX_LOOPBACK_REFUSED {
        return Err(BrowserError::Policy(format!(
            "{how}; the person already refused it (or several loopback addresses) in this session, so it is not asked again"
        )));
    }
    let page = current_page(rt)
        .await
        .and_then(|u| url::Url::parse(&u).ok())
        .map_or(String::new(), |u| u.origin().ascii_serialization());
    if !host.confirm_loopback(&target, &page, &ctx.cancel).await {
        note_refusal(&state, &key);
        return Err(BrowserError::Policy(format!(
            "{how}; the person did not approve it. Ask the person to approve, then retry"
        )));
    }
    rt.policy()
        .approve_loopback(url)
        .map_err(BrowserError::Policy)
}
browser_tool!(snapshot, "browser_snapshot", snap);
browser_tool!(click, "browser_click", click_body);
browser_tool!(mouse, "browser_mouse", mouse_body);
browser_tool!(fill, "browser_fill", fill_body);
browser_tool!(press_key, "browser_press_key", key);
browser_tool!(wait_for, "browser_wait_for", wait);
browser_tool!(screenshot, "browser_screenshot", shot);
browser_tool!(console, "browser_console", console_body);
browser_tool!(network, "browser_network", network_body);
browser_tool!(tabs, "browser_tabs", tabs_body);
browser_tool!(close, "browser_close", close_body);

const LOGIN_TIMEOUT: Duration = Duration::from_secs(600);

/// `browser_login`: a visible browser on a dedicated persistent profile; the person logs in.
/// Only counts leave this function, never cookie values or page content.
pub(crate) fn login<'a>(
    sh: &'a Shared,
    input: &'a Value,
    ctx: &'a ToolContext,
) -> BoxFuture<'a, ToolOutput> {
    Box::pin(async move {
        login_body(sh, input, ctx)
            .await
            .unwrap_or_else(|e| fail("browser_login", e))
    })
}

async fn login_body(sh: &Shared, input: &Value, ctx: &ToolContext) -> Res {
    if ctx.sub_agent {
        return Err(
            BrowserError::Refused("browser_login is not available to sub-agents".into()).into(),
        );
    }
    let cfg = lock(&sh.browser_cfg).clone();
    let origin = profile::validate_login_origin(
        str_field(input, "origin").ok_or("\"origin\" is required.")?,
    )?;
    let name = match str_field(input, "profile") {
        Some(n) => {
            profile::validate_name(n)?;
            n.to_owned()
        }
        None => profile::default_name(&origin),
    };
    require_sandbox(&cfg)?;
    let env = launch_env(&cfg, ctx);
    profile::display_available(&env, Os::current())
        .map_err(|e| BrowserError::Refused(format!("browser_login: {e}")))?;
    let (Some(host), Some(home)) = (cfg.host.clone(), cfg.home.clone()) else {
        return Err(BrowserError::Refused("this front-end cannot ask a person to log in. Use browser_fill_secret with a secret declared in the user settings (synt.browser.secrets)".into()).into());
    };
    let origin_str = profile::origin_of(&origin).ok_or("origin has no host")?;
    let dir = profile::create(&home, &name)?;
    let _ = std::fs::remove_file(dir.join(DONE_FILE));
    let timeout = cfg.login_timeout.unwrap_or(LOGIN_TIMEOUT);
    let mut o = BrowserOptions::new(ctx.workspace.artifact_root.join("browser"));
    o.headless = false;
    o.profile = Some(dir.clone());
    o.no_sandbox = cfg.no_sandbox;
    o.policy = cfg.policy.clone();
    // a loopback login opens that one port of loopback, nothing else of the private network
    if profile::is_loopback(&origin) {
        o.policy.loopback_ports = origin.port_or_known_default().into_iter().collect();
    }
    let rt = BrowserRuntime::launch(o, &env).await?;
    let req = LoginRequest {
        origin: origin_str.clone(),
        profile: name.clone(),
        timeout,
        task_id: ctx.task_id.clone(),
        turn_id: ctx.turn_id.clone(),
        tool_call_id: ctx.tool_call_id.clone(),
        done_file: dir.join(DONE_FILE),
    };
    let wait = async {
        rt.navigate(origin.as_str(), WaitUntil::Load).await?;
        await_person(host.as_ref(), &req, &ctx.cancel).await?;
        rt.cookie_count(&origin_str).await
    };
    match wait.await {
        Ok(n) => {
            rt.close_graceful().await?; // flushes the cookies to the profile
            // No cookie, no login: do not record the site, or a later session could browse it
            // with a profile that holds nothing for it.
            if n > 0 {
                profile::add_domain(&dir, &origin)?;
            }
            let out = json!({"ok": n > 0, "origin": origin_str, "profile": name, "cookie_count_for_origin": n});
            Ok(ToolOutput::text(out.to_string()).with_meta(out))
        }
        Err(e) => {
            let _ = rt.close().await; // kills the whole browser process tree
            Err(e.into())
        }
    }
}

/// Waits for the person's confirmation, the timeout or a cancel, whichever comes first.
async fn await_person(
    host: &dyn BrowserHost,
    req: &LoginRequest,
    cancel: &CancellationToken,
) -> Result<(), BrowserError> {
    tokio::select! {
        r = host.login(req, cancel) => r.map_err(BrowserError::Refused),
        () = tokio::time::sleep(req.timeout) => Err(BrowserError::Timeout(format!(
            "no confirmation from the person after {} s; the browser was closed",
            req.timeout.as_secs()
        ))),
        () = cancel.cancelled() => Err(BrowserError::Refused("cancelled; the browser was closed".into())),
    }
}

/// `browser_fill_secret`: types a user-declared secret into a field. The model never sees it.
pub(crate) fn fill_secret<'a>(
    sh: &'a Shared,
    input: &'a Value,
    ctx: &'a ToolContext,
) -> BoxFuture<'a, ToolOutput> {
    Box::pin(async move {
        if ctx.sub_agent {
            return fail(
                "browser_fill_secret",
                BrowserError::Refused("not available to sub-agents".into()).into(),
            );
        }
        if let Err(e) = require_sandbox(&lock(&sh.browser_cfg)) {
            return fail("browser_fill_secret", e.into());
        }
        let rt = match runtime(sh, ctx, "browser_fill_secret", None).await {
            Ok(rt) => rt,
            Err(e) => return e,
        };
        let _busy = rt.agent_busy();
        if let Err(e) = approve_loopback(sh, ctx, &rt, current_page(&rt).await.as_deref()).await {
            return fail("browser_fill_secret", e.into());
        }
        let unchecked = match payment_gate(sh, ctx, &rt, "browser_fill_secret", input).await {
            Ok((u, _)) => u,
            Err(e) => return fail("browser_fill_secret", e.into()),
        };
        let r = tokio::select! {
            r = fill_secret_body(&rt, input, sh, ctx) => r,
            () = ctx.cancel.cancelled() => Err("aborted.".into()),
        };
        note_unchecked(
            r.unwrap_or_else(|e| fail("browser_fill_secret", e)),
            unchecked,
        )
    })
}

async fn fill_secret_body(
    rt: &BrowserRuntime,
    input: &Value,
    sh: &Shared,
    ctx: &ToolContext,
) -> Res {
    let r = str_field(input, "ref").ok_or("\"ref\" is required.")?;
    let name = str_field(input, "secret").ok_or("\"secret\" is required.")?;
    let cfg = lock(&sh.browser_cfg).clone();
    require_sandbox(&cfg)?;
    let refuse = |m: String| Box::<dyn Error + Send + Sync>::from(BrowserError::Refused(m));
    let spec = cfg.secrets.get(name).ok_or_else(|| {
        refuse(format!(
            "no secret named {name:?} in the user settings (synt.browser.secrets)"
        ))
    })?;
    let origin = rt.page_origin().await?;
    if !secret::origin_allowed(&origin, spec) {
        return Err(refuse(format!(
            "the page origin {origin} is not one secret {name:?} is bound to"
        )));
    }
    let env = cfg
        .process_env
        .as_ref()
        .ok_or_else(|| refuse("no process environment is configured for secrets".into()))?;
    let value = secret::resolve(name, spec, env)?; // fail before bothering the person
    let state = sh.session(ctx);
    let key = (name.to_owned(), origin.clone());
    if !lock(&state.secret_ok).contains(&key) && !cfg.pre_approved_secrets.contains(&key) {
        let Some(host) = &cfg.host else {
            return Err(refuse("nobody can approve the use of a secret here".into()));
        };
        let rk = format!("secret:{name}@{origin}");
        if refused_too_often(&state, &rk) {
            return Err(refuse(format!(
                "the person already refused secret {name:?} on {origin} twice in this session; not asking again"
            )));
        }
        if !host.confirm_secret(name, &origin, &ctx.cancel).await {
            note_refusal(&state, &rk);
            return Err(refuse(format!(
                "the person did not approve using secret {name:?} on {origin}"
            )));
        }
        lock(&state.secret_ok).insert(key);
    }
    // Basic-auth style echoes (`base64(user:pass)`) of two declared secrets are masked as well
    for (other, osp) in cfg.secrets.iter().filter(|(n, _)| n.as_str() != name) {
        if let Ok(ov) = secret::resolve(other, osp, env) {
            synthesis_browser::mask_secret(&format!("{value}:{ov}"));
            synthesis_browser::mask_secret(&format!("{ov}:{value}"));
        }
    }
    let maybe = rt.fill_secret(r, spec, &value).await?;
    let out = json!({"ok": true, "filled": if maybe { json!("maybe") } else { json!(true) }, "secret": name});
    // the position only, and the word "secret" instead of the field or the secret's name
    Ok(ToolOutput::text(out.to_string()).with_meta(action(
        "type",
        rt.spot_of(r).await,
        Some("secret"),
    )))
}

/// Whether `key` (a key name or a `Modifier+Key` chord) ends in a non-printable control key: Enter, Tab, Escape,
/// Backspace, Delete, an arrow, Home, End, PageUp, PageDown or F1-F24, after only modifier names. Only such a
/// press is reported by name in `browser.action`; a printable key or a chord ending in one (a character of a
/// password, `Control+A`) never is.
pub fn is_control_chord(key: &str) -> bool {
    let mut parts: Vec<&str> = key.split('+').collect();
    let Some(last) = parts.pop() else {
        return false;
    };
    const MODS: [&str; 5] = ["Control", "Shift", "Alt", "Meta", "ControlOrMeta"];
    const KEYS: [&str; 12] = [
        "Enter",
        "Tab",
        "Escape",
        "Backspace",
        "Delete",
        "ArrowUp",
        "ArrowDown",
        "ArrowLeft",
        "ArrowRight",
        "Home",
        "End",
        "PageUp",
    ];
    let f_key = last
        .strip_prefix('F')
        .and_then(|n| n.parse::<u8>().ok())
        .is_some_and(|n| (1..=24).contains(&n));
    (KEYS.contains(&last) || last == "PageDown" || f_key) && parts.iter().all(|p| MODS.contains(p))
}

/// `meta.action` of a result: the harness turns it into a `browser.action` event. Only where the action
/// landed and a short target; never the text typed, the key pressed or a secret.
fn action(kind: &str, spot: Option<synthesis_browser::Spot>, target: Option<&str>) -> Value {
    let mut a = json!({"action": kind});
    if let Some(s) = spot {
        a["x"] = json!(s.x.round() as i32);
        a["y"] = json!(s.y.round() as i32);
        a["w"] = json!(s.w);
        a["h"] = json!(s.h);
    }
    if let Some(t) = target {
        a["target"] = json!(t.chars().take(80).collect::<String>());
    }
    json!({"action": a})
}

async fn nav(rt: &BrowserRuntime, input: &Value, _: &Shared) -> Res {
    let url = str_field(input, "url").ok_or("\"url\" is required.")?;
    let until = match input["wait_until"].as_str() {
        Some("domcontentloaded") => WaitUntil::DomContentLoaded,
        Some("networkidle") => WaitUntil::NetworkIdle,
        _ => WaitUntil::Load,
    };
    let go = rt.navigate(url, until);
    let page = match int_field(input, "timeout_ms") {
        Some(t) => tokio::time::timeout(Duration::from_millis(t.clamp(1, 600_000) as u64), go)
            .await
            .map_err(|_| BrowserError::Timeout(format!("navigation to {url}")))??,
        None => go.await?,
    };
    // a same-document navigation keeps a typed short secret on the page: withhold its text
    if rt.guard_text().await.is_err() {
        return Ok(ToolOutput::text(
            "Navigated. (page text withheld: a secret was typed into this document)",
        )
        .with_meta(action("navigate", None, None)));
    }
    Ok(
        ToolOutput::text(format!("Navigated.\n{}", untrusted(page_text(&page))))
            .with_meta(action("navigate", None, None)),
    )
}

async fn snap(rt: &BrowserRuntime, input: &Value, _: &Shared) -> Res {
    if input.get("ref").is_some() {
        return Err("subtree snapshots (ref) are not supported yet.".into());
    }
    let max = int_field(input, "max_nodes").unwrap_or(200).clamp(1, 2000) as usize;
    let s = rt.snapshot(max).await?;
    let more = if s.truncated {
        "\n[truncated: pass a larger max_nodes]"
    } else {
        ""
    };
    Ok(ToolOutput::text(format!(
        "{}{more}",
        untrusted(format!("{}\n\n{}", page_text(&s.page), s.tree.expose()))
    )))
}

/// The single ref whose line in the snapshot is `- <role> "<name>"`.
async fn ref_by_role(rt: &BrowserRuntime, role: &str, name: &str) -> Result<String, String> {
    let tree = rt.snapshot(2000).await.map_err(|e| e.to_string())?;
    let name = name.split_whitespace().collect::<Vec<_>>().join(" ");
    let head = format!("- {role} {}", json!(name));
    let refs: Vec<&str> = tree
        .tree
        .expose()
        .lines()
        .filter(|l| l.trim_start().starts_with(&head))
        .filter_map(|l| l.split("[ref=").nth(1)?.split(']').next())
        .collect();
    match refs[..] {
        [r] => Ok(r.to_string()),
        [] => Err(format!("no element with role {role:?} and name {name:?}.")),
        _ => Err(format!(
            "role {role:?} and name {name:?} match {} elements ({}); use a ref.",
            refs.len(),
            refs.join(", ")
        )),
    }
}

async fn click_body(rt: &BrowserRuntime, input: &Value, _: &Shared) -> Res {
    let mut label = None;
    let r = match (
        str_field(input, "ref"),
        str_field(input, "role"),
        str_field(input, "name"),
    ) {
        (Some(r), ..) => r.to_string(),
        (None, Some(role), Some(name)) => {
            label = Some(format!("{role} {name}"));
            ref_by_role(rt, role, name).await?
        }
        _ if input.get("selector").is_some() => {
            return Err("selectors are not supported yet; use ref or role+name.".into());
        }
        _ => return Err("give ref, or role and name.".into()),
    };
    let spot = rt.click(&r).await?;
    Ok(ToolOutput::text(format!("Clicked {r}.")).with_meta(action(
        "click",
        Some(spot),
        Some(label.as_deref().unwrap_or(&r)),
    )))
}

/// A mouse target as the model gave it (`prefix` is "" or "to_"): ref, role+name or x+y.
#[derive(Debug, PartialEq)]
enum Aim<'a> {
    Ref(&'a str),
    Role(&'a str, &'a str),
    Point(f64, f64),
}

fn aim<'a>(input: &'a Value, prefix: &str) -> Result<Option<Aim<'a>>, String> {
    let f = |k: &str| input.get(format!("{prefix}{k}"));
    let s = |k: &str| f(k).and_then(Value::as_str).filter(|v| !v.is_empty());
    let n = |k: &str| match f(k) {
        None | Some(Value::Null) => Ok(None),
        Some(v) => v
            .as_f64()
            .filter(|v| v.is_finite())
            .map(Some)
            .ok_or(format!("\"{prefix}{k}\" must be a finite number.")),
    };
    let (x, y) = (n("x")?, n("y")?);
    let given = [
        s("ref").is_some(),
        s("role").is_some() || s("name").is_some(),
        x.is_some() || y.is_some(),
    ];
    match (given, s("ref"), s("role"), s("name"), x, y) {
        ([false, false, false], ..) => Ok(None),
        ([true, false, false], Some(r), ..) => Ok(Some(Aim::Ref(r))),
        ([false, true, false], _, Some(role), Some(name), ..) => Ok(Some(Aim::Role(role, name))),
        ([false, false, true], _, _, _, Some(x), Some(y)) => Ok(Some(Aim::Point(x, y))),
        _ => Err(format!(
            "give exactly one of: {p}ref, {p}role with {p}name, or {p}x with {p}y.",
            p = prefix
        )),
    }
}

async fn target(rt: &BrowserRuntime, a: Aim<'_>) -> Result<MouseTarget, String> {
    Ok(match a {
        Aim::Ref(r) => MouseTarget::Ref(r.to_owned()),
        Aim::Role(role, name) => MouseTarget::Ref(ref_by_role(rt, role, name).await?),
        Aim::Point(x, y) => MouseTarget::Point(x, y),
    })
}

fn shown(t: &MouseTarget) -> String {
    match t {
        MouseTarget::Ref(r) => r.clone(),
        MouseTarget::Point(x, y) => format!("({}, {})", x.round(), y.round()),
    }
}

/// `browser_mouse`: the agent's pointer. Goes through the same gates as `browser_click` (`browser_tool!`).
async fn mouse_body(rt: &BrowserRuntime, input: &Value, _: &Shared) -> Res {
    let act = str_field(input, "action").ok_or("\"action\" is required.")?;
    let from = match aim(input, "")? {
        Some(a) => Some(target(rt, a).await?),
        None => None,
    };
    let need = |t: Option<MouseTarget>| t.ok_or("give ref, role and name, or x and y.");
    let (op, kind) = match act {
        "move" => (MouseOp::Move(need(from.clone())?), "hover"),
        "click" => (MouseOp::Click(need(from.clone())?), "click"),
        "double_click" => (MouseOp::DoubleClick(need(from.clone())?), "double_click"),
        "right_click" => (MouseOp::RightClick(need(from.clone())?), "right_click"),
        "drag" => {
            let to = match aim(input, "to_")? {
                Some(a) => target(rt, a).await?,
                None => {
                    return Err(
                        "drag needs the end: to_ref, to_role and to_name, or to_x and to_y.".into(),
                    );
                }
            };
            (MouseOp::Drag(need(from.clone())?, to), "drag")
        }
        "wheel" => {
            let d = |k: &str| match input.get(k) {
                None | Some(Value::Null) => Ok(0.0),
                Some(v) => v
                    .as_f64()
                    .filter(|v| v.is_finite())
                    .ok_or(format!("\"{k}\" must be a finite number.")),
            };
            let m = synthesis_browser::MAX_WHEEL;
            let (dx, dy) = (d("dx")?.clamp(-m, m), d("dy")?.clamp(-m, m));
            if dx == 0.0 && dy == 0.0 {
                return Err("wheel needs a non-zero dx or dy.".into());
            }
            (MouseOp::Wheel(from.clone(), dx, dy), "scroll")
        }
        _ => {
            return Err(
                "action is one of move, click, double_click, right_click, drag, wheel.".into(),
            );
        }
    };
    let spot = rt.mouse(&op).await?;
    let at = match &op {
        MouseOp::Drag(a, b) => format!("{} to {}", shown(a), shown(b)),
        MouseOp::Wheel(_, dx, dy) => format!("by ({dx}, {dy})"),
        MouseOp::Move(t) | MouseOp::Click(t) | MouseOp::DoubleClick(t) | MouseOp::RightClick(t) => {
            shown(t)
        }
    };
    // a point names no element: say which one is under it and on which origin (what a reviewer needs)
    let by_point = match &op {
        MouseOp::Drag(_, t) => matches!(t, MouseTarget::Point(..)),
        MouseOp::Wheel(None, ..) => true,
        MouseOp::Wheel(Some(t), ..)
        | MouseOp::Move(t)
        | MouseOp::Click(t)
        | MouseOp::DoubleClick(t)
        | MouseOp::RightClick(t) => matches!(t, MouseTarget::Point(..)),
    };
    let label = if by_point {
        let el = rt
            .element_at(spot.x, spot.y)
            .await
            .unwrap_or_else(|| "?".into());
        let origin = current_page(rt)
            .await
            .and_then(|u| url::Url::parse(&u).ok())
            .map_or(String::new(), |u| u.origin().ascii_serialization());
        // the origin first: the 80-character clip of the target keeps it
        format!(
            "{origin} {el} at {}",
            shown(&MouseTarget::Point(spot.x, spot.y))
        )
    } else {
        match &op {
            MouseOp::Drag(_, b) => shown(b),
            _ => from.as_ref().map_or_else(|| act.to_owned(), shown),
        }
    };
    Ok(
        ToolOutput::text(format!("Mouse {act} {at}.")).with_meta(action(
            kind,
            Some(spot),
            Some(&label),
        )),
    )
}

async fn fill_body(rt: &BrowserRuntime, input: &Value, _: &Shared) -> Res {
    let Some(r) = str_field(input, "ref") else {
        return Err(if input.get("selector").is_some() {
            "selectors are not supported yet; use ref."
        } else {
            "\"ref\" is required."
        }
        .into());
    };
    let value = input["value"].as_str().unwrap_or("");
    rt.fill(r, value).await?;
    // The text is never echoed: a sensitive value must not reach the transcript or events.
    let mut meta = action("type", rt.spot_of(r).await, Some(r));
    meta["ref"] = json!(r);
    meta["sensitive"] = json!(input["sensitive"] == true);
    Ok(ToolOutput::text(format!(
        "Filled {r} ({} characters).",
        value.chars().count()
    ))
    .with_meta(meta))
}

async fn key(rt: &BrowserRuntime, input: &Value, _: &Shared) -> Res {
    let k = str_field(input, "key").ok_or("\"key\" is required.")?;
    rt.press_key(k).await?;
    // only a control key is named; a printable one can be a character of a secret
    let named = if is_control_chord(k) { k } else { "key" };
    Ok(ToolOutput::text(format!("Pressed {k}.")).with_meta(action("press", None, Some(named))))
}

async fn wait(rt: &BrowserRuntime, input: &Value, _: &Shared) -> Res {
    let t = int_field(input, "timeout_ms")
        .unwrap_or(10_000)
        .clamp(1, 120_000) as u64;
    let what = match (
        str_field(input, "text"),
        str_field(input, "selector"),
        input["network_idle"] == true,
        int_field(input, "sleep_ms"),
    ) {
        (Some(s), None, false, None) => WaitFor::Text(s.into()),
        (None, Some(s), false, None) => WaitFor::Selector(s.into()),
        (None, None, true, None) => WaitFor::NetworkIdle,
        (None, None, false, Some(ms)) => WaitFor::Sleep(Duration::from_millis(ms.max(0) as u64)),
        _ => return Err("give exactly one of text, selector, network_idle, sleep_ms.".into()),
    };
    rt.wait_for(what, Duration::from_millis(t)).await?;
    Ok(ToolOutput::text("Done waiting."))
}

/// Width and height from the IHDR chunk of a PNG; 0, 0 when the bytes are not one.
fn png_size(png: &[u8]) -> (u32, u32) {
    let be = |i: usize| {
        png.get(i..i + 4)
            .map_or(0, |b| u32::from_be_bytes(b.try_into().unwrap()))
    };
    if png.starts_with(b"\x89PNG") {
        (be(16), be(20))
    } else {
        (0, 0)
    }
}

/// One line for the person reading the tool row (it is also in the model's view of the result).
const SHOT_ALLOWED_NOTE: &str = "; screenshot sent to the model, it may show on-screen secrets (synt.browser.screenshots is allow)";

async fn shot(rt: &BrowserRuntime, input: &Value, sh: &Shared) -> Res {
    if input.get("ref").is_some() {
        return Err("element screenshots (ref) are not supported yet.".into());
    }
    let full = input["full_page"] == true;
    let allow = lock(&sh.browser_cfg).allow_screenshots;
    let s = if allow {
        rt.screenshot_allowed(full).await?
    } else {
        rt.screenshot(full).await?
    };
    let path = s.path.display().to_string();
    let (w, h) = png_size(&s.png);
    let frame = json!({"url": rt.page_origin().await.unwrap_or_default(), "w": w, "h": h, "bytes": s.png.len()});
    let meta = json!({"artifact_path": path, "frame": frame});
    // pixels cannot be masked: unless the person chose `allow`, a page that may show credentials goes to the
    // model as text only
    let (supports_images, allow) = {
        let c = lock(&sh.browser_cfg);
        (c.supports_images, c.allow_screenshots)
    };
    let credentialed = !allow && rt.page_credentialed().await;
    if supports_images && !credentialed {
        let note = if allow { SHOT_ALLOWED_NOTE } else { "" };
        return Ok(ToolOutput {
            content: Content::Blocks(vec![
                Block::Image {
                    source: ImageSource::Base64 {
                        media_type: "image/png".into(),
                        data: base64::engine::general_purpose::STANDARD.encode(&s.png),
                    },
                },
                Block::Text {
                    text: format!("Screenshot saved as artifact {path}{note}"),
                },
            ]),
            is_error: false,
            soft: false,
            meta,
        });
    }
    let t = rt.snapshot(80).await?;
    Ok(ToolOutput::text(format!(
        "Screenshot saved as artifact {path} ({}; page summary follows).\n{}",
        if credentialed {
            "the image is withheld from the model: the page may show credentials"
        } else {
            "images are not sent to this model: it takes none, or synt.browser.screenshots is not allow"
        },
        untrusted(format!("{}\n\n{}", page_text(&t.page), t.tree.expose()))
    ))
    .with_meta(meta))
}

fn cursor(input: &Value) -> Result<usize, Box<dyn Error + Send + Sync>> {
    str_field(input, "cursor")
        .map_or(Ok(0), |c| c.parse())
        .map_err(|_| "invalid cursor.".into())
}

async fn console_body(rt: &BrowserRuntime, input: &Value, _: &Shared) -> Res {
    rt.guard_text().await?;
    let from = cursor(input)?;
    let (msgs, next) = rt.console_messages(from, str_field(input, "level"));
    let mut lines: Vec<String> = msgs
        .iter()
        .map(|m| format!("[{}] {}", m.level, m.text.expose()))
        .collect();
    if from == 0 {
        lines.extend(rt.exceptions().iter().map(|e| {
            format!(
                "[exception] {} ({}:{})",
                e.text.expose(),
                e.url.expose(),
                e.line.unwrap_or(0)
            )
        }));
    }
    let body = if lines.is_empty() {
        "(no console messages)".to_string()
    } else {
        untrusted(lines.join("\n"))
    };
    Ok(ToolOutput::text(format!("{body}\n[cursor {next}]"))
        .with_meta(json!({"cursor": next.to_string()})))
}

async fn network_body(rt: &BrowserRuntime, input: &Value, _: &Shared) -> Res {
    rt.guard_text().await?;
    let (reqs, next) = rt.network_requests(cursor(input)?, input["failed_only"] == true);
    let lines: Vec<String> = reqs
        .iter()
        .map(|r| {
            format!(
                "{} {} -> {} {} {}ms{}",
                r.method,
                r.url.expose(),
                r.status.map_or("-".into(), |s| s.to_string()),
                r.resource_type,
                r.duration_ms.unwrap_or(0),
                r.error.as_ref().map_or(String::new(), |e| format!(" {e}")),
            )
        })
        .collect();
    let body = if lines.is_empty() {
        "(no requests)".to_string()
    } else {
        untrusted(lines.join("\n"))
    };
    Ok(ToolOutput::text(format!("{body}\n[cursor {next}]"))
        .with_meta(json!({"cursor": next.to_string()})))
}

async fn tabs_body(rt: &BrowserRuntime, input: &Value, _: &Shared) -> Res {
    if input["action"] == "select" {
        let i = int_field(input, "index").ok_or("select needs an index.")?;
        rt.select_tab(usize::try_from(i).map_err(|_| "invalid index.")?)
            .await?;
        return Ok(ToolOutput::text(format!("Selected tab {i}.")));
    }
    rt.guard_text().await?; // the listing carries page titles and URLs
    let lines: Vec<String> = rt
        .tabs()
        .await?
        .iter()
        .map(|t| {
            format!(
                "{}[{}] {} | {}",
                if t.active { "*" } else { " " },
                t.index,
                t.page.url.expose(),
                t.page.title.expose()
            )
        })
        .collect();
    Ok(ToolOutput::text(untrusted(lines.join("\n"))))
}

async fn close_body(rt: &BrowserRuntime, input: &Value, _: &Shared) -> Res {
    let i = match int_field(input, "index") {
        Some(i) => usize::try_from(i).map_err(|_| "invalid index.")?,
        None => rt
            .tabs()
            .await?
            .iter()
            .find(|t| t.active)
            .map_or(0, |t| t.index),
    };
    rt.close_tab(i).await?;
    Ok(ToolOutput::text(format!("Closed tab {i}.")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug)]
    struct Hang;
    impl BrowserHost for Hang {
        fn login<'a>(
            &'a self,
            _: &'a LoginRequest,
            _: &'a CancellationToken,
        ) -> BoxFuture<'a, Result<(), String>> {
            Box::pin(std::future::pending())
        }
        fn confirm_secret<'a>(
            &'a self,
            _: &'a str,
            _: &'a str,
            _: &'a CancellationToken,
        ) -> BoxFuture<'a, bool> {
            Box::pin(async { false })
        }
        fn confirm_profile<'a>(
            &'a self,
            _: &'a str,
            _: &'a [String],
            _: &'a CancellationToken,
        ) -> BoxFuture<'a, bool> {
            Box::pin(async { false })
        }
    }

    fn req(timeout: Duration) -> LoginRequest {
        LoginRequest {
            origin: "https://a.com".into(),
            profile: "a".into(),
            timeout,
            task_id: "t".into(),
            turn_id: "u".into(),
            tool_call_id: "c".into(),
            done_file: PathBuf::from("x"),
        }
    }

    #[tokio::test]
    async fn the_wait_ends_on_timeout_or_cancel_so_the_caller_can_kill_the_browser() {
        let c = CancellationToken::new();
        let e = await_person(&Hang, &req(Duration::from_millis(50)), &c).await;
        assert!(matches!(e, Err(BrowserError::Timeout(_))), "{e:?}");
        c.cancel();
        let e = await_person(&Hang, &req(Duration::from_secs(60)), &c).await;
        assert!(matches!(e, Err(BrowserError::Refused(m)) if m.contains("cancelled")));
    }
}

#[cfg(test)]
mod png_tests {
    #[test]
    fn png_size_reads_ihdr() {
        let mut p = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR".to_vec();
        p.extend([0, 0, 5, 0, 0, 0, 2, 208]);
        assert_eq!(super::png_size(&p), (1280, 720));
        assert_eq!(super::png_size(b"nope"), (0, 0));
    }
}

#[cfg(test)]
mod mouse_tests {
    use super::*;

    #[test]
    fn a_target_is_exactly_one_of_ref_role_name_or_a_finite_point() {
        assert_eq!(aim(&json!({}), ""), Ok(None));
        assert_eq!(aim(&json!({"ref": "e1"}), ""), Ok(Some(Aim::Ref("e1"))));
        assert_eq!(
            aim(&json!({"role": "button", "name": "Go"}), ""),
            Ok(Some(Aim::Role("button", "Go")))
        );
        assert_eq!(
            aim(&json!({"x": 1.5, "y": 2}), ""),
            Ok(Some(Aim::Point(1.5, 2.0)))
        );
        assert_eq!(
            aim(&json!({"to_x": 1, "to_y": 2}), "to_"),
            Ok(Some(Aim::Point(1.0, 2.0)))
        );
        for bad in [
            json!({"x": 1}),
            json!({"y": 1}),
            json!({"role": "button"}),
            json!({"ref": "e1", "x": 1, "y": 1}),
            json!({"ref": "e1", "role": "a", "name": "b"}),
            json!({"x": "1", "y": 1}),
            json!({"x": null, "y": 1}),
            json!({"x": true, "y": 1}),
        ] {
            assert!(aim(&bad, "").is_err(), "{bad}");
        }
        // the end of a drag is not read from the start's keys
        assert_eq!(aim(&json!({"x": 1, "y": 2}), "to_"), Ok(None));
    }
}

#[cfg(test)]
mod action_tests {
    use super::*;

    #[test]
    fn the_action_meta_has_a_point_and_a_clipped_target_and_nothing_else() {
        let spot = synthesis_browser::Spot {
            x: 100.4,
            y: 50.6,
            w: 1280,
            h: 800,
        };
        let m = action("type", Some(spot), Some(&"x".repeat(200)));
        let a = m["action"].as_object().unwrap();
        let mut keys: Vec<_> = a.keys().map(String::as_str).collect();
        keys.sort_unstable();
        assert_eq!(keys, ["action", "h", "target", "w", "x", "y"]);
        assert_eq!((a["x"].as_i64(), a["y"].as_i64()), (Some(100), Some(51)));
        assert_eq!(a["target"].as_str().unwrap().len(), 80);
        assert_eq!(
            action("press", None, None)["action"],
            json!({"action": "press"})
        );
    }
}
