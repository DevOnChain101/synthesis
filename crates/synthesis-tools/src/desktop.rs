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

use crate::native::{SessionState, Shared, lock};
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
    /// `synt.desktop.allow_press`: let `desktop_act` send key combos. Off by default: whether
    /// agent-desktop posts them to the target process or to the frontmost app is unverified.
    pub allow_press: bool,
    /// TESTS ONLY, never set by the app: skip the "sandbox must be enforce" gate and the
    /// native-executable check so a shell-script fixture can stand in for an app.
    pub unsafe_for_tests: bool,
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
    Launch {
        app_id: String,
        pid: u32,
        name: String,
        external: bool,
    },
    Act {
        app_id: String,
        action: String,
        r#ref: Option<String>,
        delivery: String,
    },
    Screenshot {
        app_id: String,
        window_id: String,
        path: String,
    },
    Denied {
        tool: String,
        reason: String,
    },
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
    audit(
        sh,
        DesktopAudit::Denied {
            tool: wire.into(),
            reason: reason.clone(),
        },
    );
    fail(wire, &DesktopError::Refused(reason))
}

/// Sub-agents never drive the desktop; the harness enforces this for peers before any dialog.
fn main_agent_only(sh: &Shared, ctx: &ToolContext, wire: &str) -> Result<(), ToolOutput> {
    if ctx.sub_agent {
        return Err(refuse(
            sh,
            wire,
            "desktop tools are not available to sub-agents",
        ));
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
    state: &Arc<SessionState>,
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
            let rt = DesktopRuntime::start(o, &env, procs).await?;
            rt.set_guard(Arc::new(Guard {
                rt: sh.rt.clone(),
                session: ctx.session_id.clone(),
                state: Arc::downgrade(state),
            }));
            Ok(Arc::new(rt))
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
) -> Result<
    (
        Arc<SessionState>,
        Arc<DesktopRuntime>,
        synthesis_desktop::App,
    ),
    ToolOutput,
> {
    main_agent_only(sh, ctx, wire)?;
    if ctx.permission_mode == PermissionMode::Plan {
        return Err(refuse(sh, wire, desktop::PLAN_REFUSED));
    }
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

/// Vouches that the process behind an app id is still the one the terminal runtime started.
struct Guard {
    rt: Arc<synthesis_terminal::TerminalRuntime>,
    session: String,
    state: std::sync::Weak<SessionState>,
}

impl synthesis_desktop::AppGuard for Guard {
    fn alive(&self, app_id: &str) -> bool {
        let Some(state) = self.state.upgrade() else {
            return false;
        };
        let pid = lock(&state.desktop_procs).get(app_id).cloned();
        pid.is_some_and(|p| {
            self.rt
                .get_status(&self.session, &p)
                .is_ok_and(|s| s.running)
        })
    }
}

/// Size, modification time and (up to 256 MiB) sha256 of a file.
fn fingerprint(p: &Path) -> Option<(u64, Option<std::time::SystemTime>, Option<String>)> {
    let m = std::fs::metadata(p).ok()?;
    let hash = (m.len() <= 256 * 1024 * 1024)
        .then(|| synthesis_desktop::pin::sha256_file(p).ok())
        .flatten();
    Some((m.len(), m.modified().ok(), hash))
}

fn same_installed_binary(a: &Path, b: &Path) -> bool {
    let (Ok(x), Ok(y)) = (std::fs::metadata(a), std::fs::metadata(b)) else {
        return false;
    };
    if !x.is_file() || !y.is_file() || x.len() != y.len() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if x.dev() == y.dev() && x.ino() == y.ino() {
            return true;
        }
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        if x.volume_serial_number() == y.volume_serial_number() && x.file_index() == y.file_index()
        {
            return true;
        }
    }
    let (Ok(a), Ok(b)) = (
        synthesis_desktop::pin::sha256_file(a),
        synthesis_desktop::pin::sha256_file(b),
    ) else {
        return false;
    };
    a == b
}

fn installed_denied_copy(p: &Path, env: Option<&Env>) -> bool {
    let mut roots = vec![
        PathBuf::from("/Applications"),
        PathBuf::from("/System/Applications"),
        PathBuf::from("/System/Applications/Utilities"),
        PathBuf::from("/System/Library/CoreServices/Applications"),
    ];
    if let Some(e) = env {
        for key in ["ProgramFiles", "ProgramFiles(x86)"] {
            if let Some(v) = e.get_nonempty(key) {
                roots.push(PathBuf::from(v));
            }
        }
    }
    denied_copy_in_roots(p, &roots)
}

fn denied_copy_in_roots(p: &Path, roots: &[PathBuf]) -> bool {
    roots
        .iter()
        .cloned()
        .filter_map(|root| std::fs::read_dir(root).ok())
        .flatten()
        .filter_map(Result::ok)
        .filter(|e| desktop::hard_denied(&e.file_name().to_string_lossy()).is_some())
        .any(|e| {
            let base = e.path();
            if base.is_file() {
                return same_installed_binary(p, &base);
            }
            let macos = base.join("Contents/MacOS");
            std::fs::read_dir(macos)
                .ok()
                .into_iter()
                .flatten()
                .filter_map(Result::ok)
                .any(|child| same_installed_binary(p, &child.path()))
                || std::fs::read_dir(&base)
                    .ok()
                    .into_iter()
                    .flatten()
                    .filter_map(Result::ok)
                    .any(|child| same_installed_binary(p, &child.path()))
        })
}

#[cfg(target_os = "macos")]
fn denied_bundle_identity(p: &Path) -> Result<Option<&'static str>, String> {
    let bundle = p
        .ancestors()
        .find(|a| a.extension().is_some_and(|e| e.eq_ignore_ascii_case("app")));
    let Some(bundle) = bundle else {
        return Ok(None);
    };
    let plist = bundle.join("Contents/Info.plist");
    for key in ["CFBundleIdentifier", "CFBundleName"] {
        let out = std::process::Command::new("/usr/bin/plutil")
            .args(["-extract", key, "raw", "-o", "-"])
            .arg(&plist)
            .output()
            .map_err(|_| "cannot read the app bundle's Info.plist")?;
        if !out.status.success() {
            return Err("cannot read the app bundle's Info.plist".into());
        }
        let value = String::from_utf8(out.stdout).map_err(|_| "invalid app bundle Info.plist")?;
        if let Some(family) = desktop::hard_denied(value.trim()) {
            return Ok(Some(family));
        }
    }
    Ok(None)
}

/// Mach-O (also universal), ELF or PE: a compiled program, not a script.
fn is_native_executable(p: &Path) -> bool {
    use std::io::Read;
    let mut b = [0u8; 4];
    std::fs::File::open(p)
        .and_then(|mut f| f.read_exact(&mut b))
        .is_ok()
        && (b == [0x7f, b'E', b'L', b'F']
            || matches!(
                b,
                [0xfe, 0xed, 0xfa, 0xce | 0xcf]
                    | [0xce | 0xcf, 0xfa, 0xed, 0xfe]
                    | [0xca, 0xfe, 0xba, 0xbe | 0xbf]
            )
            || b[..2] == *b"MZ")
}

/// The system shell or interpreter this file is a byte-for-byte copy of, if any (a copy renamed
/// to look like an app).
fn interpreter_copy(p: &Path, env: Option<&Env>) -> Option<PathBuf> {
    let len = std::fs::metadata(p).ok()?.len();
    let root = env
        .and_then(|e| e.get_nonempty("SystemRoot"))
        .unwrap_or(r"C:\Windows");
    // The real interpreters: well-known names in the usual directories, the PATH of the injected
    // Env and the Homebrew prefixes. A recompiled or modified copy is not recognised: this is a
    // hint, the sandbox and the Bash verdict are the control.
    let mut dirs: Vec<PathBuf> = [
        "/bin",
        "/usr/bin",
        "/usr/local/bin",
        "/opt/homebrew/bin",
        "/opt/local/bin",
        "/snap/bin",
    ]
    .iter()
    .map(PathBuf::from)
    .collect();
    if let Some(path) = env.and_then(|e| e.get_nonempty("PATH")) {
        dirs.extend(std::env::split_paths(path));
    }
    const NAMES: &[&str] = &[
        "sh",
        "bash",
        "zsh",
        "dash",
        "ksh",
        "csh",
        "tcsh",
        "fish",
        "env",
        "osascript",
        "awk",
        "node",
        "nodejs",
        "python3",
        "python",
        "deno",
        "bun",
        "ruby",
        "perl",
        "php",
        "lua",
        "java",
        "busybox",
        "pwsh",
        "tclsh",
        "find",
        "git",
        "vim",
        "nvim",
        "emacs",
        "luajit",
        "Rscript",
        "julia",
        "gawk",
        "sed",
        "xargs",
        "make",
        "cmake",
        "gradle",
        "mvn",
        "npm",
        "npx",
        "pnpm",
        "yarn",
        "cargo",
        "go",
        "dotnet",
    ];
    let mut cands: Vec<PathBuf> = dirs
        .iter()
        .flat_map(|d| {
            NAMES
                .iter()
                .map(move |n| d.join(format!("{n}{}", std::env::consts::EXE_SUFFIX)))
        })
        .collect();
    cands.extend(
        [
            r"System32\cmd.exe",
            r"System32\WindowsPowerShell\v1.0\powershell.exe",
            r"System32\wscript.exe",
            r"System32\cscript.exe",
            r"System32\mshta.exe",
        ]
        .iter()
        .map(|r| PathBuf::from(format!(r"{root}\{r}"))),
    );
    let mine = std::cell::OnceCell::new();
    for c in cands {
        let Ok(real) = std::fs::canonicalize(&c) else {
            continue;
        };
        if std::fs::metadata(&real).ok().map(|m| m.len()) != Some(len) {
            continue;
        }
        let h = mine.get_or_init(|| synthesis_desktop::pin::sha256_file(p).ok());
        if h.is_some() && *h == synthesis_desktop::pin::sha256_file(&real).ok() {
            return Some(c);
        }
    }
    None
}

fn quote_posix(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// cmd.exe has no safe quoting for these: refuse instead of guessing.
fn quote_cmd(s: &str) -> Result<String, String> {
    if s.contains(['"', '%', '^', '&', '|', '<', '>', '(', ')', '!', '\r', '\n']) {
        return Err(format!(
            "{s:?} contains a character that cannot be passed safely to a Windows command"
        ));
    }
    Ok(format!("\"{s}\""))
}

fn command_line(posix: bool, target: &str, args: &[String]) -> Result<String, String> {
    if std::iter::once(target)
        .chain(args.iter().map(String::as_str))
        .any(|a| a.contains('\0'))
    {
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
    let Some(env) = &cfg.process_env else {
        return Default::default();
    };
    // Only DISPLAY. XDG_RUNTIME_DIR, DBUS_SESSION_BUS_ADDRESS and XAUTHORITY would reach the
    // secret-service / session-bus sockets and the X cookie, and the sandbox is not proven to
    // deny them: the app gets them only through a safer design, the CLI gets them regardless.
    ["DISPLAY"]
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

fn approved_launch_matches(ctx: &ToolContext, target: &str, origin: Origin) -> bool {
    let Some(given) = ctx.approval_token.as_deref() else {
        return false;
    };
    desktop::scoped_launch_token(
        &ctx.task_id,
        &ctx.turn_id,
        &ctx.tool_call_id,
        target,
        origin,
        origin == Origin::External,
    )
    .is_ok_and(|expected| given == expected)
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
        return refuse(
            sh,
            W,
            format!("{shown} is {family}: desktop control of it is never allowed"),
        );
    }
    if let Some(why) = desktop::refused_target(target) {
        return refuse(sh, W, format!("{shown}: {why}"));
    }
    let cfg = lock(&sh.desktop_cfg).clone();
    let args: Vec<String> = input["args"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default();
    if args.len() > desktop::MAX_ARGS || args.iter().any(|a| a.len() > 4096) {
        return refuse(
            sh,
            W,
            format!(
                "too many or too long arguments ({} of 4096 bytes at most)",
                desktop::MAX_ARGS
            ),
        );
    }
    if let Some(a) = args.iter().find(|a| desktop::refused_arg(a)) {
        return refuse(
            sh,
            W,
            format!(
                "argument {} runs code through the program (-c, -e, --eval, -m ...)",
                desktop::sanitize(a)
            ),
        );
    }
    if !desktop::is_path_target(target) || !Path::new(target).is_absolute() {
        return refuse(
            sh,
            W,
            format!(
                "{shown}: give the absolute path of the executable; bundle ids and program names are not supported"
            ),
        );
    }
    // What would really run: links resolved, a bundle replaced by its inner executable (which must
    // resolve inside the bundle). Everything below, and the origin, judge THIS path; it is also
    // the path that is executed.
    let real = match desktop::resolve_executable(target) {
        Ok(Some(r)) => r,
        Ok(None) => return ToolOutput::error(format!("desktop_launch: {shown} does not exist.")),
        Err(e) => return refuse(sh, W, format!("{shown}: {e}")),
    };
    // Where it came from: inside the workspace (after links) is the project's own app. The target
    // and the executable it resolves to must agree; a project path that runs something outside
    // the workspace is refused (it cannot be offered to the person as an outside app either).
    let inside = |p: &str| ctx.workspace.resolve(p).is_ok();
    let origin = match (inside(target), inside(&real.to_string_lossy())) {
        (true, true) => Origin::Project,
        (true, false) => {
            return refuse(
                sh,
                W,
                format!(
                    "{shown} runs an executable outside the workspace ({}): refused",
                    desktop::sanitize(&real.to_string_lossy())
                ),
            );
        }
        (false, _) => Origin::External,
    };
    // The harness recorded the exact file and origin it judged. An external launch also needs
    // the evidence minted only after the hard approval was answered Allow.
    if !cfg.unsafe_for_tests && !approved_launch_matches(ctx, target, origin) {
        return refuse(
            sh,
            W,
            "the approved executable, its origin or its identity changed: refused",
        );
    }
    if let Err(e) = mode_ok(sh, ctx, W, origin) {
        return e;
    }
    if let Some(family) = desktop::hard_denied(&real.to_string_lossy()) {
        return refuse(
            sh,
            W,
            format!("{shown} resolves to {family}: desktop control of it is never allowed"),
        );
    }
    #[cfg(target_os = "macos")]
    match denied_bundle_identity(&real) {
        Ok(Some(family)) => return refuse(sh, W, format!("{shown} is {family}: refused")),
        Err(why) => return refuse(sh, W, format!("{shown}: {why}")),
        Ok(None) => {}
    }
    if installed_denied_copy(&real, cfg.process_env.as_ref()) {
        return refuse(
            sh,
            W,
            format!("{shown} has the identity of an installed denied app"),
        );
    }
    #[cfg(unix)]
    if origin == Origin::Project {
        use std::os::unix::fs::MetadataExt;
        if std::fs::metadata(&real).is_ok_and(|m| m.nlink() > 1) {
            return refuse(
                sh,
                W,
                format!("{shown} is a hardlinked executable: refused"),
            );
        }
    }
    if let Some(why) = desktop::refused_target(&real.to_string_lossy()) {
        return refuse(
            sh,
            W,
            format!("{shown} resolves to a program that is refused: {why}"),
        );
    }
    if let Some(sys) = interpreter_copy(&real, cfg.process_env.as_ref()) {
        return refuse(
            sh,
            W,
            format!(
                "{shown} is a copy of the system program {}: refused",
                sys.display()
            ),
        );
    }
    if !cfg.unsafe_for_tests && !is_native_executable(&real) {
        return refuse(
            sh,
            W,
            format!(
                "{shown} is not a compiled executable (a script runs other programs): build the app and launch the binary"
            ),
        );
    }
    // The app runs under the same sandbox as terminal_spawn, enforced, or not at all.
    if !cfg.unsafe_for_tests {
        if sh.rt.mode() != synthesis_terminal::SandboxMode::Enforce {
            return refuse(
                sh,
                W,
                "desktop apps are launched only when the sandbox mode is enforce (best-effort and off can run them unconfined): start synt with --sandbox enforce",
            );
        }
        if sh.policy_for(&ctx.workspace).is_none() {
            return refuse(
                sh,
                W,
                "there is no sandbox policy for this session; the app would run unconfined",
            );
        }
    }
    if let Some(family) = desktop::hard_denied(&real.to_string_lossy()) {
        return refuse(
            sh,
            W,
            format!("{shown} resolves to {family}: desktop control of it is never allowed"),
        );
    }
    if !is_executable(&real) {
        return ToolOutput::error(format!(
            "desktop_launch: {shown} is not an executable file (on macOS give the file inside the bundle, e.g. App.app/Contents/MacOS/App)"
        ));
    }
    let cwd = match str_field(input, "cwd") {
        Some(c) => match ctx.workspace.resolve(c) {
            Ok(p) => Some(p),
            Err(e) => return ToolOutput::error(format!("desktop_launch: cwd: {e}")),
        },
        None => None,
    };
    let before = fingerprint(&real);
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
    // The checks above judged this exact file: if it changed since (or is not what was judged),
    // do not run it. Narrows the check-to-exec window; it cannot close it.
    if fingerprint(&real) != before {
        return refuse(
            sh,
            W,
            format!("{shown} changed while it was being checked: refused"),
        );
    }
    // The runtime startup above can take time. Recheck the *approval* identity at the last
    // point before spawning, not just the file observed by the tool after approval.
    if !cfg.unsafe_for_tests && !approved_launch_matches(ctx, target, origin) {
        return refuse(
            sh,
            W,
            "the approved executable, its origin or its identity changed: refused",
        );
    }
    let mut spec = SpawnSpec::new(line);
    spec.name = Some("desktop-app".into());
    spec.cwd = cwd;
    spec.env = display_env(&cfg);
    let handle = match sh.rt.spawn(&ctx.session_id, spec).await {
        Ok(h) => h,
        Err(e) => return ToolOutput::error(format!("desktop_launch: {e}")),
    };
    // the terminal runtime must really have confined it
    if !cfg.unsafe_for_tests
        && !sh
            .rt
            .get_status(&ctx.session_id, &handle.process_id)
            .is_ok_and(|s| s.sandbox.is_some() && !s.unsandboxed_by_request)
    {
        let _ = sh
            .rt
            .signal(&ctx.session_id, &handle.process_id, Signal::Kill);
        return refuse(
            sh,
            W,
            "the app did not start under the sandbox; it was killed",
        );
    }
    let app = match rt.register_app(handle.pid, origin, &real.to_string_lossy()) {
        Ok(a) => a,
        Err(e) => {
            let _ = sh
                .rt
                .signal(&ctx.session_id, &handle.process_id, Signal::Kill);
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
        let running = sh
            .rt
            .get_status(&ctx.session_id, &handle.process_id)
            .is_ok_and(|s| s.running);
        if !windows.is_empty() || !running {
            break;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    let running = sh
        .rt
        .get_status(&ctx.session_id, &handle.process_id)
        .is_ok_and(|s| s.running);
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
        .snapshot(
            &app.id,
            str_field(input, "window_id"),
            str_field(input, "root_ref"),
            input["skeleton"] == true,
            max,
        )
        .await
    {
        Ok(s) => s,
        Err(e) => return fail(W, &e),
    };
    let more = if s.truncated {
        "\n[truncated: use a larger max_nodes, skeleton or root_ref]"
    } else {
        ""
    };
    ToolOutput::text(format!(
        "window: {}\nsnapshot: {}\n{}{more}",
        s.window_id,
        s.snapshot_id,
        untrusted(format!(
            "title: {}\n\n{}",
            s.window_title.expose(),
            s.tree.expose()
        ))
    ))
    .with_meta(
        json!({"snapshot_id": s.snapshot_id, "window_id": s.window_id, "refs": s.refs.len()}),
    )
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
    audit(
        sh,
        DesktopAudit::Screenshot {
            app_id: app.id.clone(),
            window_id: s.window_id.clone(),
            path: path.clone(),
        },
    );
    let meta = json!({"artifact_path": path, "window_id": s.window_id});
    let text = format!(
        "Screenshot of window {} saved as artifact {path} (window only; it may show private data).",
        s.window_id
    );
    if lock(&sh.desktop_cfg).supports_images
        && s.bytes <= 4 * 1024 * 1024
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
    let chunk = match sh
        .rt
        .read_logs(&ctx.session_id, &pid, str_field(input, "since"), 30_000)
    {
        Ok(c) => c,
        Err(e) => return ToolOutput::error(format!("desktop_logs: {e}")),
    };
    let running = sh
        .rt
        .get_status(&ctx.session_id, &pid)
        .is_ok_and(|s| s.running);
    let body = if chunk.data.is_empty() {
        "(no new output)".to_string()
    } else {
        untrusted(synthesis_browser::redact_text(&chunk.data))
    };
    ToolOutput::text(format!(
        "{body}\n[cursor {}] [{}]",
        chunk.cursor,
        if running { "running" } else { "exited" }
    ))
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
            if !sh
                .rt
                .get_status(&ctx.session_id, &pid)
                .is_ok_and(|s| s.running)
            {
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
    if action == Action::Press && !lock(&sh.desktop_cfg).allow_press {
        return refuse(
            sh,
            W,
            "press is off: set synt.desktop.allow_press to true in your user settings once you have checked that agent-desktop sends keys only to the target window (docs/DESKTOP-CONTROL.md)",
        );
    }
    let r = str_field(input, "ref");
    let r = rt
        .act(
            &app.id,
            action,
            r,
            input["value"].as_str(),
            str_field(input, "window_id"),
        )
        .await;
    let name = str_field(input, "action").unwrap_or_default().to_owned();
    match r {
        Ok(d) => {
            audit(
                sh,
                DesktopAudit::Act {
                    app_id: app.id,
                    action: name,
                    r#ref: r_of(input),
                    delivery: d.as_str().into(),
                },
            );
            let out = json!({"delivered": d.delivered(), "verified": d.verified(), "delivery": d.as_str()});
            ToolOutput::text(out.to_string()).with_meta(out)
        }
        Err(e) => {
            // the typed value is not echoed anywhere; the delivery outcome is the audit record
            audit(
                sh,
                DesktopAudit::Act {
                    app_id: app.id,
                    action: name,
                    r#ref: r_of(input),
                    delivery: "failed".into(),
                },
            );
            fail(W, &e)
        }
    }
}

fn r_of(input: &Value) -> Option<String> {
    str_field(input, "ref").map(desktop::sanitize)
}

async fn wait_body(sh: &Shared, input: &Value, ctx: &ToolContext) -> ToolOutput {
    const W: &str = "desktop_wait";
    let (_, rt, app) = match app_runtime(sh, ctx, input, W).await {
        Ok(x) => x,
        Err(e) => return e,
    };
    let t = int_field(input, "timeout_ms")
        .unwrap_or(10_000)
        .clamp(1, 120_000) as u64;
    let win = str_field(input, "window_id");
    let what = match (
        input["predicate"].as_str(),
        str_field(input, "text"),
        str_field(input, "ref"),
    ) {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_app_gets_the_display_and_nothing_that_reaches_the_session_bus_or_the_x_cookie() {
        let env = Env::from_pairs([
            ("DISPLAY", ":0"),
            ("WAYLAND_DISPLAY", "wayland-0"),
            ("XAUTHORITY", "/home/u/.Xauthority"),
            ("XDG_RUNTIME_DIR", "/run/user/1000"),
            ("DBUS_SESSION_BUS_ADDRESS", "unix:path=/run/user/1000/bus"),
            ("ANTHROPIC_API_KEY", "sk-leak"),
        ]);
        let cfg = DesktopConfig {
            process_env: Some(env),
            ..Default::default()
        };
        let got = display_env(&cfg);
        assert_eq!(
            got.keys().map(String::as_str).collect::<Vec<_>>(),
            ["DISPLAY"]
        );
    }

    #[test]
    fn a_changed_file_has_another_fingerprint() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("app");
        std::fs::write(&p, b"one").unwrap();
        let before = fingerprint(&p);
        assert_eq!(before, fingerprint(&p));
        std::fs::write(&p, b"two").unwrap(); // same size, other content
        assert_ne!(before, fingerprint(&p));
        assert_eq!(fingerprint(&d.path().join("missing")), None);
    }

    #[test]
    fn renamed_copy_of_an_installed_denied_app_is_recognised() {
        let d = tempfile::tempdir().unwrap();
        let root = d.path().join("Applications");
        let installed = root.join("1Password.app/Contents/MacOS");
        std::fs::create_dir_all(&installed).unwrap();
        let original = installed.join("hidden-name");
        std::fs::write(&original, b"the installed denied program").unwrap();
        let copy = d.path().join("innocent");
        std::fs::copy(&original, &copy).unwrap();
        assert!(denied_copy_in_roots(&copy, &[root]));
    }

    #[test]
    fn magic_numbers_tell_binaries_from_scripts() {
        let d = tempfile::tempdir().unwrap();
        let w = |n: &str, b: &[u8]| {
            let p = d.path().join(n);
            std::fs::write(&p, b).unwrap();
            p
        };
        assert!(is_native_executable(&w("elf", b"\x7fELF....")));
        assert!(is_native_executable(&w(
            "macho",
            &[0xcf, 0xfa, 0xed, 0xfe, 0, 0]
        )));
        assert!(is_native_executable(&w(
            "fat",
            &[0xca, 0xfe, 0xba, 0xbe, 0, 0]
        )));
        assert!(is_native_executable(&w("pe", b"MZ\x90\x00")));
        assert!(!is_native_executable(&w("sh", b"#!/bin/sh\nid\n")));
        assert!(!is_native_executable(&w("tiny", b"x")));
    }

    #[cfg(unix)]
    #[test]
    fn a_renamed_copy_of_a_system_interpreter_is_recognised() {
        let d = tempfile::tempdir().unwrap();
        let copy = d.path().join("MyApp");
        std::fs::copy("/bin/sh", &copy).unwrap();
        assert!(interpreter_copy(&copy, None).is_some());
        let other = d.path().join("other");
        std::fs::write(&other, b"not a shell").unwrap();
        assert!(interpreter_copy(&other, None).is_none());
    }
}
