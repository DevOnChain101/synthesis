//! desktop_* tools on top of `synthesis_desktop` (docs/DESKTOP-CONTROL.md). One lazily created
//! `DesktopRuntime` per session lives in `SessionState`; it dies with the session. The app under
//! test is started HERE, through the terminal runtime (own process group, output in a log
//! artifact), so its process tree is known and every agent-desktop call can be scoped to it.
//! Policy is decided by the permission engine before a tool runs; the checks below repeat the
//! ones that must hold even if it was bypassed (mode, sub-agent, hard-deny list).

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use base64::Engine;
use serde_json::{Value, json};
use synthesis_config::Env;
use synthesis_core::{PermissionMode, ToolContext};
use synthesis_desktop::{
    Action, DesktopError, DesktopOptions, DesktopRuntime, Pin, ProcSource, SystemProcs, WaitFor,
};
use synthesis_permissions::desktop::{self, Origin};
use synthesis_terminal::{Signal, SpawnSpec};

use crate::native::{Shared, SessionState, lock};
use crate::util::{int_field, str_field};
use crate::{Block, BoxFuture, Content, ImageSource, ToolOutput};

/// Desktop settings of the host (set with `Registry::with_desktop`). Without it the tools do
/// not exist.
#[derive(Clone, Default)]
pub struct DesktopConfig {
    /// The agent-desktop executable (`synt.desktop.binary`, or next to the `synt` binary).
    pub binary: Option<PathBuf>,
    /// `synt.desktop.allow_unpinned`.
    pub allow_unpinned: bool,
    /// The pin to verify against (`None`: the compiled-in one).
    pub pin: Option<Pin>,
    /// The process environment snapshot: the only source of the display variables and of what
    /// the CLI is allowed to see.
    pub process_env: Option<Env>,
    /// The provider accepts image blocks in tool results.
    pub supports_images: bool,
    pub actions_per_sec: Option<f64>,
    pub max_actions: Option<u32>,
    pub call_timeout: Option<Duration>,
    /// Audit sink (`desktop.*` events).
    pub host: Option<Arc<dyn DesktopHost>>,
    /// Process table source (tests).
    pub procs: Option<Arc<dyn ProcSource>>,
}

impl std::fmt::Debug for DesktopConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DesktopConfig")
            .field("binary", &self.binary)
            .field("allow_unpinned", &self.allow_unpinned)
            .finish_non_exhaustive()
    }
}

/// What the audit trail records. Names and ids only: never a typed value or UI content.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DesktopAudit {
    Launch { app_id: String, pid: u32, name: String, external: bool },
    Act { app_id: String, action: String, r#ref: Option<String>, delivery: String },
    Screenshot { app_id: String, window_id: String, path: String },
    Denied { tool: String, reason: String },
}

pub trait DesktopHost: Send + Sync + std::fmt::Debug {
    fn audit(&self, e: DesktopAudit);
}

/// `value` of `desktop_act` never reaches events, hooks or approval prompts.
pub fn redact_input(tool: &str, input: &Value) -> Value {
    let mut v = input.clone();
    if tool == "desktop_act"
        && let Some(o) = v.as_object_mut()
        && o.contains_key("value")
    {
        o.insert("value".into(), "[REDACTED]".into());
    }
    v
}

fn untrusted(s: impl Into<String>) -> String {
    synthesis_desktop::UntrustedText::new(s).to_string()
}

fn audit(sh: &Shared, e: DesktopAudit) {
    if let Some(h) = &lock(&sh.desktop_cfg).host {
        h.audit(e);
    }
}

fn fail(wire: &str, e: &DesktopError) -> ToolOutput {
    let text = if matches!(e, DesktopError::Tool { .. }) {
        format!("{wire}: {}", untrusted(e.to_string()))
    } else {
        format!("{wire}: {e}")
    };
    ToolOutput::error(text).with_meta(json!({"error": e.kind()}))
}

fn refuse(sh: &Shared, wire: &str, reason: impl Into<String>) -> ToolOutput {
    let reason = reason.into();
    audit(sh, DesktopAudit::Denied { tool: wire.into(), reason: reason.clone() });
    fail(wire, &DesktopError::Refused(reason))
}

/// Sub-agents never drive the desktop; the harness enforces this for peers before any dialog.
fn main_agent_only(sh: &Shared, ctx: &ToolContext, wire: &str) -> Result<(), ToolOutput> {
    if ctx.sub_agent {
        return Err(refuse(sh, wire, "desktop tools are not available to sub-agents"));
    }
    Ok(())
}

/// An app the person approved (outside the project) works in the default mode only.
fn mode_ok(sh: &Shared, ctx: &ToolContext, wire: &str, origin: Origin) -> Result<(), ToolOutput> {
    match desktop::gate(origin, to_mode(ctx.permission_mode)) {
        desktop::Gate::Deny(why) => Err(refuse(sh, wire, why)),
        _ => Ok(()),
    }
}

fn to_mode(m: PermissionMode) -> synthesis_permissions::Mode {
    use synthesis_permissions::Mode;
    match m {
        PermissionMode::Default => Mode::Default,
        PermissionMode::AcceptEdits => Mode::AcceptEdits,
        PermissionMode::Plan => Mode::Plan,
        PermissionMode::Auto => Mode::Auto,
        PermissionMode::DontAsk => Mode::DontAsk,
        PermissionMode::BypassPermissions => Mode::BypassPermissions,
    }
}

async fn runtime(
    sh: &Shared,
    ctx: &ToolContext,
    state: &SessionState,
    wire: &str,
) -> Result<Arc<DesktopRuntime>, ToolOutput> {
    let cfg = lock(&sh.desktop_cfg).clone();
    state
        .desktop
        .get_or_try_init(|| async {
            let binary = cfg.binary.clone().ok_or_else(|| {
                DesktopError::Unavailable(
                    "no agent-desktop binary: set synt.desktop.binary in your user settings".into(),
                )
            })?;
            let env = cfg.process_env.clone().unwrap_or_default();
            let mut o = DesktopOptions::new(binary, ctx.workspace.artifact_root.join("desktop"));
            o.allow_unpinned = cfg.allow_unpinned;
            if let Some(p) = &cfg.pin {
                o.pin = p.clone();
            }
            if let Some(r) = cfg.actions_per_sec {
                o.actions_per_sec = r;
            }
            if let Some(m) = cfg.max_actions {
                o.max_actions = m;
            }
            if let Some(t) = cfg.call_timeout {
                o.call_timeout = t;
            }
            let procs = cfg.procs.clone().unwrap_or_else(|| {
                Arc::new(SystemProcs {
                    system_root: env.get_nonempty("SystemRoot").map(str::to_owned),
                })
            });
            DesktopRuntime::start(o, &env, procs).await.map(Arc::new)
        })
        .await
        .cloned()
        .map_err(|e| fail(wire, &e))
}

/// Common prologue of every tool that names an app.
async fn app_runtime(
    sh: &Shared,
    ctx: &ToolContext,
    input: &Value,
    wire: &str,
) -> Result<(Arc<SessionState>, Arc<DesktopRuntime>, synthesis_desktop::App), ToolOutput> {
    main_agent_only(sh, ctx, wire)?;
    let id = str_field(input, "app_id")
        .ok_or_else(|| ToolOutput::error(format!("{wire}: \"app_id\" is required.")))?;
    let state = sh.session(ctx);
    let rt = runtime(sh, ctx, &state, wire).await?;
    let app = rt.app(id).map_err(|e| fail(wire, &e))?;
    mode_ok(sh, ctx, wire, app.origin)?;
    Ok((state, rt, app))
}

macro_rules! desktop_tool {
    ($name:ident, $wire:literal, $body:ident) => {
        pub(crate) fn $name<'a>(
            sh: &'a Shared,
            input: &'a Value,
            ctx: &'a ToolContext,
        ) -> BoxFuture<'a, ToolOutput> {
            Box::pin(async move {
                tokio::select! {
                    r = $body(sh, input, ctx) => r,
                    () = ctx.cancel.cancelled() => ToolOutput::error(concat!($wire, ": aborted.")),
                }
            })
        }
    };
}
desktop_tool!(launch, "desktop_launch", launch_body);
desktop_tool!(snapshot, "desktop_snapshot", snapshot_body);
desktop_tool!(screenshot, "desktop_screenshot", shot_body);
desktop_tool!(logs, "desktop_logs", logs_body);
desktop_tool!(close, "desktop_close", close_body);
desktop_tool!(act, "desktop_act", act_body);
desktop_tool!(wait, "desktop_wait", wait_body);

// ---- launch ---------------------------------------------------------------------------------

fn quote_posix(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// cmd.exe has no safe quoting for these: refuse instead of guessing.
fn quote_cmd(s: &str) -> Result<String, String> {
    if s.contains(['"', '%', '^', '&', '|', '<', '>', '(', ')', '!', '\r', '\n']) {
        return Err(format!("{s:?} contains a character that cannot be passed safely to a Windows command"));
    }
    Ok(format!("\"{s}\""))
}

fn command_line(posix: bool, target: &str, args: &[String]) -> Result<String, String> {
    if std::iter::once(target).chain(args.iter().map(String::as_str)).any(|a| a.contains('\0')) {
        return Err("arguments cannot contain NUL".into());
    }
    if posix {
        let mut c = format!("exec {}", quote_posix(target));
        for a in args {
            c += &format!(" {}", quote_posix(a));
        }
        Ok(c)
    } else {
        let mut c = quote_cmd(target)?;
        for a in args {
            c += &format!(" {}", quote_cmd(a)?);
        }
        Ok(c)
    }
}

/// Display variables the app needs to open a window (the session env is an allowlist without them).
fn display_env(cfg: &DesktopConfig) -> std::collections::BTreeMap<String, String> {
    let Some(env) = &cfg.process_env else { return Default::default() };
    ["DISPLAY", "WAYLAND_DISPLAY", "XAUTHORITY", "XDG_RUNTIME_DIR", "DBUS_SESSION_BUS_ADDRESS"]
        .iter()
        .filter_map(|k| env.get_nonempty(k).map(|v| ((*k).to_owned(), v.to_owned())))
        .collect()
}

#[cfg(unix)]
fn is_executable(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(p).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}
#[cfg(not(unix))]
fn is_executable(p: &Path) -> bool {
    p.is_file()
}

async fn launch_body(sh: &Shared, input: &Value, ctx: &ToolContext) -> ToolOutput {
    const W: &str = "desktop_launch";
    if let Err(e) = main_agent_only(sh, ctx, W) {
        return e;
    }
    let Some(target) = str_field(input, "target") else {
        return ToolOutput::error("desktop_launch: \"target\" is required.");
    };
    let shown = desktop::sanitize(target);
    if let Some(family) = desktop::hard_denied(target) {
        return refuse(sh, W, format!("{shown} is {family}: desktop control of it is never allowed"));
    }
    if !desktop::is_path_target(target) || !Path::new(target).is_absolute() {
        return refuse(sh, W, format!("{shown}: give the absolute path of the executable; bundle ids and program names are not supported"));
    }
    // Where it came from: inside the workspace (after links) is the project's own app.
    let (path, origin) = match ctx.workspace.resolve(target) {
        Ok(p) => (p, Origin::Project),
        Err(_) => (PathBuf::from(target), Origin::External),
    };
    if let Err(e) = mode_ok(sh, ctx, W, origin) {
        return e;
    }
    // the real file, not a link to something on the deny list
    let real = std::fs::canonicalize(&path).unwrap_or_else(|_| path.clone());
    if let Some(family) = desktop::hard_denied(&real.to_string_lossy()) {
        return refuse(sh, W, format!("{shown} resolves to {family}: desktop control of it is never allowed"));
    }
    if !is_executable(&real) {
        return ToolOutput::error(format!(
            "desktop_launch: {shown} is not an executable file (on macOS give the file inside the bundle, e.g. App.app/Contents/MacOS/App)"
        ));
    }
    let args: Vec<String> = input["args"]
        .as_array()
        .map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_owned)).collect())
        .unwrap_or_default();
    let cwd = match str_field(input, "cwd") {
        Some(c) => match ctx.workspace.resolve(c) {
            Ok(p) => Some(p),
            Err(e) => return ToolOutput::error(format!("desktop_launch: cwd: {e}")),
        },
        None => None,
    };
    let posix = sh.rt.shell().is_posix();
    let line = match command_line(posix, &real.to_string_lossy(), &args) {
        Ok(l) => l,
        Err(e) => return ToolOutput::error(format!("desktop_launch: {e}")),
    };
    let state = sh.session(ctx);
    let rt = match runtime(sh, ctx, &state, W).await {
        Ok(r) => r,
        Err(e) => return e,
    };
    let mut spec = SpawnSpec::new(line);
    spec.name = Some("desktop-app".into());
    spec.cwd = cwd;
    spec.env = display_env(&lock(&sh.desktop_cfg));
    let handle = match sh.rt.spawn(&ctx.session_id, spec).await {
        Ok(h) => h,
        Err(e) => return ToolOutput::error(format!("desktop_launch: {e}")),
    };
    let app = match rt.register_app(handle.pid, origin, &real.to_string_lossy()) {
        Ok(a) => a,
        Err(e) => {
            let _ = sh.rt.signal(&ctx.session_id, &handle.process_id, Signal::Kill);
            return refuse(sh, W, e.to_string());
        }
    };
    lock(&state.desktop_procs).insert(app.id.clone(), handle.process_id.clone());
    audit(
        sh,
        DesktopAudit::Launch {
            app_id: app.id.clone(),
            pid: app.pid,
            name: app.name.clone(),
            external: origin == Origin::External,
        },
    );
    // give the app a moment to show its first window
    let mut windows = vec![];
    for _ in 0..25 {
        if let Ok(w) = rt.windows(&app.id).await {
            windows = w.windows;
        }
        let running = sh.rt.get_status(&ctx.session_id, &handle.process_id).is_ok_and(|s| s.running);
        if !windows.is_empty() || !running {
            break;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    let running = sh.rt.get_status(&ctx.session_id, &handle.process_id).is_ok_and(|s| s.running);
    let head = json!({
        "app_id": app.id, "pid": app.pid, "running": running,
        "origin": if origin == Origin::External { "external" } else { "project" },
        "log_path": handle.log_path.display().to_string(),
    });
    let list: Vec<String> = windows
        .iter()
        .map(|w| format!("{} | {} | {}", w.id, w.app_name, w.title.expose()))
        .collect();
    let mut out = format!("{head}\nwindows ({}):", windows.len());
    if !list.is_empty() {
        out += &format!("\n{}", untrusted(list.join("\n")));
    }
    if !running {
        out += "\nThe app has already exited; read desktop_logs.";
    } else if windows.is_empty() {
        out += "\nNo window yet; use desktop_wait or desktop_logs.";
    }
    ToolOutput::text(out).with_meta(head)
}

// ---- observe --------------------------------------------------------------------------------

async fn snapshot_body(sh: &Shared, input: &Value, ctx: &ToolContext) -> ToolOutput {
    const W: &str = "desktop_snapshot";
    let (_, rt, app) = match app_runtime(sh, ctx, input, W).await {
        Ok(x) => x,
        Err(e) => return e,
    };
    let max = int_field(input, "max_nodes").unwrap_or(300).clamp(1, 2000) as usize;
    let s = match rt
        .snapshot(&app.id, str_field(input, "window_id"), str_field(input, "root_ref"), input["skeleton"] == true, max)
        .await
    {
        Ok(s) => s,
        Err(e) => return fail(W, &e),
    };
    let more = if s.truncated { "\n[truncated: use a larger max_nodes, skeleton or root_ref]" } else { "" };
    ToolOutput::text(format!(
        "window: {}\nsnapshot: {}\n{}{more}",
        s.window_id,
        s.snapshot_id,
        untrusted(format!("title: {}\n\n{}", s.window_title.expose(), s.tree.expose()))
    ))
    .with_meta(json!({"snapshot_id": s.snapshot_id, "window_id": s.window_id, "refs": s.refs.len()}))
}

async fn shot_body(sh: &Shared, input: &Value, ctx: &ToolContext) -> ToolOutput {
    const W: &str = "desktop_screenshot";
    let (_, rt, app) = match app_runtime(sh, ctx, input, W).await {
        Ok(x) => x,
        Err(e) => return e,
    };
    let s = match rt.screenshot(&app.id, str_field(input, "window_id")).await {
        Ok(s) => s,
        Err(e) => return fail(W, &e),
    };
    let path = s.path.display().to_string();
    audit(sh, DesktopAudit::Screenshot { app_id: app.id.clone(), window_id: s.window_id.clone(), path: path.clone() });
    let meta = json!({"artifact_path": path, "window_id": s.window_id});
    let text = format!("Screenshot of window {} saved as artifact {path} (window only; it may show private data).", s.window_id);
    if lock(&sh.desktop_cfg).supports_images && s.bytes <= 4 * 1024 * 1024
        && let Ok(png) = std::fs::read(&s.path)
    {
        return ToolOutput {
            content: Content::Blocks(vec![
                Block::Image {
                    source: ImageSource::Base64 {
                        media_type: "image/png".into(),
                        data: base64::engine::general_purpose::STANDARD.encode(png),
                    },
                },
                Block::Text { text },
            ]),
            is_error: false,
            soft: false,
            meta,
        };
    }
    ToolOutput::text(text).with_meta(meta)
}

async fn logs_body(sh: &Shared, input: &Value, ctx: &ToolContext) -> ToolOutput {
    const W: &str = "desktop_logs";
    let (state, _, app) = match app_runtime(sh, ctx, input, W).await {
        Ok(x) => x,
        Err(e) => return e,
    };
    let Some(pid) = lock(&state.desktop_procs).get(&app.id).cloned() else {
        return ToolOutput::error("desktop_logs: this app has no log.");
    };
    let chunk = match sh.rt.read_logs(&ctx.session_id, &pid, str_field(input, "since"), 30_000) {
        Ok(c) => c,
        Err(e) => return ToolOutput::error(format!("desktop_logs: {e}")),
    };
    let running = sh.rt.get_status(&ctx.session_id, &pid).is_ok_and(|s| s.running);
    let body = if chunk.data.is_empty() {
        "(no new output)".to_string()
    } else {
        untrusted(synthesis_browser::redact_text(&chunk.data))
    };
    ToolOutput::text(format!("{body}\n[cursor {}] [{}]", chunk.cursor, if running { "running" } else { "exited" }))
        .with_meta(json!({"cursor": chunk.cursor, "running": running}))
}

async fn close_body(sh: &Shared, input: &Value, ctx: &ToolContext) -> ToolOutput {
    const W: &str = "desktop_close";
    let (state, rt, app) = match app_runtime(sh, ctx, input, W).await {
        Ok(x) => x,
        Err(e) => return e,
    };
    let proc_id = lock(&state.desktop_procs).remove(&app.id);
    if let Some(pid) = proc_id {
        let _ = sh.rt.signal(&ctx.session_id, &pid, Signal::Kill); // the whole process group
        for _ in 0..20 {
            if !sh.rt.get_status(&ctx.session_id, &pid).is_ok_and(|s| s.running) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }
    rt.forget_app(&app.id);
    ToolOutput::text(format!("Closed {}.", app.id))
}

// ---- act ------------------------------------------------------------------------------------

async fn act_body(sh: &Shared, input: &Value, ctx: &ToolContext) -> ToolOutput {
    const W: &str = "desktop_act";
    let (_, rt, app) = match app_runtime(sh, ctx, input, W).await {
        Ok(x) => x,
        Err(e) => return e,
    };
    let Some(action) = str_field(input, "action").and_then(Action::parse) else {
        return ToolOutput::error("desktop_act: unknown \"action\".");
    };
    let r = str_field(input, "ref");
    let r = rt
        .act(&app.id, action, r, input["value"].as_str(), str_field(input, "window_id"))
        .await;
    let name = str_field(input, "action").unwrap_or_default().to_owned();
    match r {
        Ok(d) => {
            audit(sh, DesktopAudit::Act { app_id: app.id, action: name, r#ref: r_of(input), delivery: d.as_str().into() });
            let out = json!({"delivered": d.delivered(), "verified": d.verified(), "delivery": d.as_str()});
            ToolOutput::text(out.to_string()).with_meta(out)
        }
        Err(e) => {
            // the typed value is not echoed anywhere; the delivery outcome is the audit record
            audit(sh, DesktopAudit::Act { app_id: app.id, action: name, r#ref: r_of(input), delivery: "failed".into() });
            fail(W, &e)
        }
    }
}

fn r_of(input: &Value) -> Option<String> {
    str_field(input, "ref").map(|r| desktop::sanitize(r))
}

async fn wait_body(sh: &Shared, input: &Value, ctx: &ToolContext) -> ToolOutput {
    const W: &str = "desktop_wait";
    let (_, rt, app) = match app_runtime(sh, ctx, input, W).await {
        Ok(x) => x,
        Err(e) => return e,
    };
    let t = int_field(input, "timeout_ms").unwrap_or(10_000).clamp(1, 120_000) as u64;
    let win = str_field(input, "window_id");
    let what = match (input["predicate"].as_str(), str_field(input, "text"), str_field(input, "ref")) {
        (Some("text"), Some(s), _) => WaitFor::Text(s.into()),
        (Some("element"), _, Some(r)) => WaitFor::Element(r.into()),
        (Some("window_gone"), ..) => WaitFor::WindowGone(win.map(Into::into)),
        _ => return ToolOutput::error("desktop_wait: text needs \"text\", element needs \"ref\"."),
    };
    match rt.wait(&app.id, what, win, Duration::from_millis(t)).await {
        Ok(d) => ToolOutput::text(format!("Done waiting ({} ms).", d.as_millis())),
        Err(e) => fail(W, &e),
    }
}
