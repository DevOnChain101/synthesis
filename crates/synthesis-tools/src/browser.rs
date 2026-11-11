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
use synthesis_browser::profile::{self, DONE_FILE};
use synthesis_browser::secret::{self, SecretSpec};
use synthesis_browser::{
    BrowserError, BrowserOptions, BrowserRuntime, Os, PageInfo, UntrustedText, UrlPolicy, WaitFor,
    WaitUntil,
};
use synthesis_config::Env;
use synthesis_core::ToolContext;
use tokio_util::sync::CancellationToken;

use crate::native::{Shared, lock};
use crate::util::{int_field, str_field};
use crate::{Block, BoxFuture, Content, ImageSource, ToolOutput};

/// Browser settings of the host (set with `Registry::with_browser`).
#[derive(Debug, Clone, Default)]
pub struct BrowserConfig {
    pub policy: UrlPolicy,
    /// Only for containers where Chrome's own sandbox cannot start.
    pub no_sandbox: bool,
    /// The provider accepts image blocks in tool results: `browser_screenshot` returns the PNG.
    pub supports_images: bool,
    /// Synthesis home: login profiles live in `<home>/browser-profiles`. `None` disables
    /// `browser_login` and the `profile` option.
    pub home: Option<PathBuf>,
    /// `synt.browser.secrets` of the USER settings (`browser_fill_secret`).
    pub secrets: BTreeMap<String, SecretSpec>,
    /// User-widened domain lists per profile (`synt.browser.profiles.<name>.allow_domains`).
    pub profile_domains: BTreeMap<String, Vec<String>>,
    /// How long a login waits for the person (default 10 minutes).
    pub login_timeout: Option<Duration>,
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

/// The seam to the person (a TUI dialog, the headless terminal prompt). Both calls are in
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
            let mut o = BrowserOptions::new(ctx.workspace.artifact_root.join("browser"));
            o.policy = cfg.policy.clone();
            o.no_sandbox = cfg.no_sandbox;
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
            BrowserRuntime::launch(o, &launch_env(&cfg, ctx))
                .await
                .map(Arc::new)
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
                let r = tokio::select! {
                    r = $body(&rt, input, sh) => r,
                    () = ctx.cancel.cancelled() => Err("aborted.".into()),
                };
                r.unwrap_or_else(|e| fail($wire, e))
            })
        }
    };
}
browser_tool!(navigate, "browser_navigate", nav);
browser_tool!(snapshot, "browser_snapshot", snap);
browser_tool!(click, "browser_click", click_body);
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
        let r = tokio::select! {
            r = fill_secret_body(&rt, input, sh, ctx) => r,
            () = ctx.cancel.cancelled() => Err("aborted.".into()),
        };
        r.unwrap_or_else(|e| fail("browser_fill_secret", e))
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

async fn shot(rt: &BrowserRuntime, input: &Value, sh: &Shared) -> Res {
    if input.get("ref").is_some() {
        return Err("element screenshots (ref) are not supported yet.".into());
    }
    let s = rt.screenshot(input["full_page"] == true).await?;
    let path = s.path.display().to_string();
    let (w, h) = png_size(&s.png);
    let frame = json!({"url": rt.page_origin().await.unwrap_or_default(), "w": w, "h": h, "bytes": s.png.len()});
    let meta = json!({"artifact_path": path, "frame": frame});
    if lock(&sh.browser_cfg).supports_images {
        return Ok(ToolOutput {
            content: Content::Blocks(vec![
                Block::Image {
                    source: ImageSource::Base64 {
                        media_type: "image/png".into(),
                        data: base64::engine::general_purpose::STANDARD.encode(&s.png),
                    },
                },
                Block::Text {
                    text: format!("Screenshot saved as artifact {path}"),
                },
            ]),
            is_error: false,
            soft: false,
            meta,
        });
    }
    let t = rt.snapshot(80).await?;
    Ok(ToolOutput::text(format!(
        "Screenshot saved as artifact {path} (images are not supported by this model; page summary follows).\n{}",
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
