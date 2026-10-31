//! `synth run`: one headless task. Exit codes follow docs/specs/events.md §6.

use std::collections::BTreeMap;
use std::io::{IsTerminal, Read};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use clap::{Args, ValueEnum};
use serde_json::Value;
use sha2::{Digest, Sha256};
use synthesis_config::{Config, Env, LoadContext, ProjectTrust, parse_model, resolve_credential};
use synthesis_core::{AgentSession, BudgetUsage, SessionStatus, WorkspaceContext};
use synthesis_events::{ArtifactRef, Event, SessionEnd, TaskStatus};
use synthesis_harness::{
    Approval, ApprovalRequest, Approver, BoxFut, EventSink, Harness, HarnessConfig, Pricing, Scope,
    TaskOutcome, TaskRequest, ToolBox, budget_from_settings, from_mode, sanitize_for_display,
};
use synthesis_hooks::Hooks;
use synthesis_mcp::McpManager;
use synthesis_permissions::{Mode, PermissionRules, Permissions};
use synthesis_providers::{ModelProvider, NetSettings, ProviderSpec, RetryPolicy};
use synthesis_sandbox::{NetworkPolicy, SandboxBackend, SandboxPolicy, SandboxedCommand};
use synthesis_session::{NewSession, Retention, Store as SessionStore, StoreOptions};
use synthesis_settings::load_settings;
use synthesis_terminal::{Guarantee, SandboxMode, TerminalRuntime};
use synthesis_tools::{Registry, standard_policy_git};
use synthesis_verification::{CompletionGate, Criterion, VerifyRunner};
use tokio_util::sync::CancellationToken;

use crate::ctx::{PutArtifact, SynthesisContext};
use crate::events::{Persist, Store};
use crate::taskdb::TaskDb;
use crate::transcript::Transcript;
use crate::wt::Worktree;
use crate::{homes, load_config};

#[derive(Clone, Copy, ValueEnum)]
pub enum ModeArg {
    Default,
    #[value(name = "acceptEdits")]
    AcceptEdits,
    Plan,
    #[value(name = "dontAsk")]
    DontAsk,
    /// Headless: no classifier is wired, so asks the classifier would review are denied.
    Auto,
    /// Skips approval prompts. Still stops: deny rules, ask rules, critical rm, destructive MCP asks.
    /// Does NOT stop plain writes to protected paths (.git, .synthesis, .ssh ...), as in Claude Code. CLI only.
    #[value(name = "bypassPermissions")]
    BypassPermissions,
}

#[derive(Clone, Copy, ValueEnum)]
pub enum SandboxArg {
    /// Refuse to run commands that cannot be confined (default).
    Enforce,
    /// Confine when the OS backend can, otherwise run unconfined.
    BestEffort,
    /// No confinement at all.
    Off,
}

impl SandboxArg {
    pub fn mode(self) -> SandboxMode {
        match self {
            Self::Enforce => SandboxMode::Enforce,
            Self::BestEffort => SandboxMode::BestEffort,
            Self::Off => SandboxMode::Off,
        }
    }
}

#[derive(Clone, Copy, ValueEnum)]
pub enum NetworkArg {
    /// No network (default).
    Deny,
    /// Loopback only (dev server + tests in one sandbox). macOS: reaches the host's loopback
    /// services; Linux bwrap: private loopback; Landlock: unsupported.
    Loopback,
    /// Unrestricted network (loud warning).
    Allow,
}

impl NetworkArg {
    pub fn policy(self) -> NetworkPolicy {
        match self {
            Self::Deny => NetworkPolicy::Deny,
            Self::Loopback => NetworkPolicy::Loopback,
            Self::Allow => NetworkPolicy::Allow,
        }
    }
}

/// `Enforce` needs a backend that confines the filesystem and accepts the policy; `Err` is
/// the message for the exit-2 refusal.
pub(crate) fn sandbox_preflight(
    backend: &dyn SandboxBackend,
    policy: Option<&synthesis_terminal::SandboxPolicy>,
) -> Result<(), String> {
    let hint = "run `synth doctor` to see the sandbox backends, or pass --sandbox best-effort|off to run unconfined";
    let skipped = synthesis_sandbox::skipped_backends();
    let skipped = if skipped.is_empty() {
        String::new()
    } else {
        format!(" [{}]", skipped.join("; "))
    };
    if backend.capabilities().filesystem < Guarantee::Weak {
        return Err(format!(
            "no OS sandbox is available on this machine ({}){skipped}; {hint}",
            backend.name()
        ));
    }
    let policy = policy.ok_or_else(|| {
        format!("cannot build the sandbox policy (artifact dir not writable); {hint}")
    })?;
    if backend.network_guarantee(&policy.network) < Guarantee::Weak {
        return Err(format!(
            "the {} sandbox cannot honour --network {}; {hint}",
            backend.name(),
            match policy.network {
                NetworkPolicy::Loopback => "loopback",
                _ => "deny",
            }
        ));
    }
    backend
        .wrap(policy, &SandboxedCommand::default())
        .map(|_| ())
        .map_err(|e| {
            format!(
                "the {} sandbox rejects the standard policy: {e}{skipped}; {hint}",
                backend.name()
            )
        })
}

/// The sandbox mode when `--sandbox` is not given: enforce, except where the only backend is the
/// Windows Job Object (no filesystem or network confinement), where enforce would refuse every
/// command: best-effort then (owner decision D10). An explicit flag always wins.
pub fn default_sandbox_mode(job_object_only: bool) -> SandboxMode {
    if job_object_only {
        SandboxMode::BestEffort
    } else {
        SandboxMode::Enforce
    }
}

/// The loud one-liner for a defaulted best-effort mode (`None` when the mode is not defaulted).
pub fn sandbox_default_notice(c: &Common) -> Option<String> {
    (c.sandbox.is_none() && c.sandbox_mode() == SandboxMode::BestEffort).then(|| {
        "sandbox: this OS has no command confinement (Windows Job Object only), so commands run UNCONFINED with your privileges; pass --sandbox enforce to refuse them instead".to_string()
    })
}

impl Common {
    pub fn sandbox_mode(&self) -> SandboxMode {
        self.sandbox
            .map_or_else(|| default_sandbox_mode(cfg!(windows)), SandboxArg::mode)
    }
}

/// Flags shared by `synth run` and the interactive app.
#[derive(Args, Clone)]
pub struct Common {
    /// Command confinement: enforce (default), best-effort, or off (loud warning). On Windows,
    /// where no OS backend can confine, the default is best-effort with a notice at start.
    #[arg(long, value_enum)]
    pub sandbox: Option<SandboxArg>,
    /// Network for tool commands: deny (default), loopback, or allow (loud warning).
    #[arg(long, value_enum, default_value = "deny")]
    pub network: NetworkArg,
    /// Extra read-only path for commands, e.g. a toolchain dir. Must exist; $HOME, filesystem
    /// roots and credential-store directories (or their ancestors) are refused (repeatable).
    #[arg(long, value_name = "PATH")]
    pub sandbox_read: Vec<PathBuf>,
    /// Provider id (with --model NAME), or `fake:<script.json>` in test-provider builds.
    #[arg(long)]
    pub provider: Option<String>,
    /// `provider/model`, an alias, or a bare model name.
    #[arg(long)]
    pub model: Option<String>,
    #[arg(long, value_enum)]
    pub permission_mode: Option<ModeArg>,
    /// Trust `<cwd>/.synthesis/config.json`, `.synthesis/settings.json`, agents and (with approval) MCP servers.
    #[arg(long)]
    pub trust_project: bool,
    /// Start this project-scope MCP server (needs --trust-project; repeatable).
    #[arg(long, value_name = "NAME")]
    pub approve_mcp: Vec<String>,
    #[arg(long)]
    pub no_session_store: bool,
    /// Allow a tool call without asking, e.g. `--allow 'Bash(cargo test:*)'` (settings rule
    /// syntax; repeatable). Typed by you, so trusted; it cannot lift critical-path or
    /// protected-path protections.
    #[arg(long, value_name = "RULE")]
    pub allow: Vec<String>,
    /// Deny a tool call, e.g. `--deny 'Bash(git push:*)'` (settings rule syntax; repeatable).
    /// Deny wins over every allow.
    #[arg(long, value_name = "RULE")]
    pub deny: Vec<String>,
    /// Work in an isolated git worktree on branch `synthesis/<session>`; left in place for review.
    #[arg(long)]
    pub worktree: bool,
    /// With --worktree: remove the worktree at the end when nothing changed.
    #[arg(long, requires = "worktree")]
    pub cleanup_worktree: bool,
}

#[derive(Args)]
pub struct RunArgs {
    /// Workspace directory (default: the current directory).
    #[arg(long)]
    cwd: Option<PathBuf>,
    #[arg(long, conflicts_with = "prompt_file")]
    prompt: Option<String>,
    #[arg(long)]
    prompt_file: Option<PathBuf>,
    #[arg(long)]
    max_turns: Option<u32>,
    #[arg(long)]
    max_tool_calls: Option<u32>,
    /// Needs a price for the model: `synthesis.pricing` in the user settings.json (config.md §6).
    #[arg(long)]
    max_cost_usd: Option<f64>,
    #[arg(long)]
    max_wall_secs: Option<u64>,
    /// JSON array of `{id?, text, check?}` (or plain strings).
    #[arg(long, alias = "criteria")]
    criteria_file: Option<PathBuf>,
    /// Also write the event stream to this file.
    #[arg(long)]
    events_out: Option<PathBuf>,
    #[command(flatten)]
    common: Common,
}

#[derive(Default, Clone)]
pub struct Limits {
    pub max_turns: Option<u32>,
    pub max_tool_calls: Option<u32>,
    pub max_cost_usd: Option<f64>,
    pub max_wall_secs: Option<u64>,
}

pub enum Front {
    /// Events go to stdout (and `tee`); asks are denied with an audit trail.
    Headless {
        prompt: String,
        tee: Option<std::fs::File>,
        /// Set (and the run cancelled) when stdout is closed by the reader.
        closed: Arc<std::sync::atomic::AtomicBool>,
    },
    /// Events go to the UI; the UI approves.
    Tui,
}

/// Everything a front end needs, built once by [`prepare`].
pub struct Prepared {
    pub harness: Arc<Harness>,
    pub session: AgentSession,
    pub sid: String,
    pub cancel: CancellationToken,
    pub sink: EventSink,
    pub rt: Arc<TerminalRuntime>,
    pub store: Option<Store>,
    pub transcript: Option<Arc<Transcript>>,
    pub tokio: tokio::runtime::Runtime,
    pub mcp: Option<Arc<McpManager>>,
    pub notices: Vec<String>,
    pub worktree: Option<Worktree>,
    /// Required checks from `synthesis.verification.checks`.
    pub criteria: Vec<Criterion>,
    pub artifact_root: PathBuf,
    pub label: String,
    pub models: Vec<String>,
    /// `effort_tiers` the model declares (enables `/effort`).
    pub effort_tiers: Vec<String>,
    pub sandbox: synthesis_tui::SandboxStatus,
    pub link: Option<synthesis_tui::UiLink>,
    pub homes: crate::Homes,
    /// `#` notes and `/memory` (TUI).
    pub notes: crate::memory::Notes,
}

/// Rows of `{"synthesis":{"pricing":{"<provider/model>"|"<model>": {input_per_mtok, output_per_mtok}}}}`
/// from the USER settings file only (a repository must not be able to price a model at zero).
fn pricing(user_settings: &Path, label: &str, model: &str) -> Option<Pricing> {
    let raw = std::fs::read(user_settings)
        .ok()
        .filter(|b| b.len() <= 1 << 20)?;
    let v: Value = serde_json::from_slice(&raw).ok()?;
    parse_pricing(&v, label, model)
}

fn parse_pricing(v: &Value, label: &str, model: &str) -> Option<Pricing> {
    let table = v.get("synthesis")?.get("pricing")?;
    let e = table.get(label).or_else(|| table.get(model))?;
    let num = |k: &str| {
        e.get(k)
            .and_then(Value::as_f64)
            .filter(|n| n.is_finite() && *n >= 0.0)
    };
    Some(Pricing {
        input_per_mtok: num("input_per_mtok")?,
        output_per_mtok: num("output_per_mtok")?,
    })
}

/// `--allow` / `--deny` values: non-empty, one line, `Tool` or `Tool(content)`.
fn rules_arg(v: &[String], flag: &str) -> Result<Vec<String>, String> {
    v.iter()
        .map(|r| {
            let r = r.trim();
            let name = r.split('(').next().unwrap_or("");
            let well_formed = !name.is_empty()
                && name
                    .chars()
                    .all(|c| c.is_alphanumeric() || matches!(c, '_' | '-' | '.'))
                && (r.len() == name.len() || r.ends_with(')'))
                && r.len() <= 2000
                && !r.chars().any(char::is_control);
            if well_formed {
                Ok(r.to_string())
            } else {
                Err(format!(
                    "{flag} {}: expected a rule like Bash(cargo test:*), Edit or mcp__server__tool",
                    sanitize_for_display(r)
                ))
            }
        })
        .collect()
}

/// A name/value that `Command::env` accepts.
fn env_pair_ok(k: &str, v: &str) -> bool {
    !k.is_empty() && !k.contains(['=', '\0']) && !v.contains('\0')
}

/// Headless: nobody can answer, so every ask is refused, and the harness still records the
/// request and the resolution (`reviewer: policy:non-interactive`) in the event stream.
struct AuditDeny;

impl Approver for AuditDeny {
    fn approve<'a>(&'a self, _: ApprovalRequest, _: &'a CancellationToken) -> BoxFut<'a, Approval> {
        Box::pin(async {
            Approval {
                reviewer: "policy:non-interactive",
                ..Approval::auto_denied()
            }
        })
    }
}

/// Warnings for the sandbox/network flags; shown at start and sent as `notice` events.
pub fn sandbox_notices(mode: SandboxMode, network: NetworkArg) -> Vec<String> {
    let mut n = vec![];
    match mode {
        SandboxMode::Off => n.push(
            "unsandboxed_by_request: --sandbox off, commands run with the agent's full privileges".to_string(),
        ),
        SandboxMode::BestEffort => n.push(
            "sandbox best-effort: commands run unconfined (unsandboxed_by_request) when the OS backend cannot confine".to_string(),
        ),
        SandboxMode::Enforce => {}
    }
    if matches!(network, NetworkArg::Allow) {
        n.push("--network allow: commands have unrestricted network access".into());
    }
    n
}

pub fn sandbox_status(mode: SandboxMode, unsandboxed: bool) -> synthesis_tui::SandboxStatus {
    let b = synthesis_sandbox::detect();
    let c = b.capabilities();
    let g = |x: Guarantee| format!("{x:?}").to_lowercase();
    synthesis_tui::SandboxStatus {
        mode: match mode {
            SandboxMode::Enforce => "enforce",
            SandboxMode::BestEffort => "best-effort",
            SandboxMode::Off => "off",
        }
        .into(),
        backend: b.name().into(),
        filesystem: g(c.filesystem),
        network: g(c.network),
        unsandboxed_by_request: unsandboxed,
    }
}

/// Does a `Read(..)` deny rule (or any deny) cover `p`? Bypass mode keeps the answer to the rules
/// alone: no ask for outside reads or protected paths.
fn read_denied(perms: &Permissions, p: &Path) -> bool {
    perms
        .check(
            "Read",
            &serde_json::json!({ "file_path": p }),
            Mode::BypassPermissions,
        )
        .decision
        == synthesis_permissions::Decision::Deny
}

/// Default `--max-wall-secs` of a headless `synth run` (one hour). Turns (50) and tool calls (200)
/// are bounded by default already; a cost cap needs a configured price, so it has no default.
const HEADLESS_WALL_SECS: u64 = 3600;

/// The repository root (nearest trusted ancestor with `.git` or `.synthesis`, see
/// `synthesis_settings::find_project_root`), else the launch dir. The home directory and anything above
/// it never count: `~/.synthesis` or a stray `/Users/.git` would otherwise make `$HOME` the workspace
/// (and writable).
pub fn workspace_root(cwd: &Path, home: &Path) -> PathBuf {
    let real = synthesis_core::canonicalize(cwd).unwrap_or_else(|_| cwd.to_path_buf());
    let root = synthesis_settings::find_project_root(&real);
    let home = synthesis_core::canonicalize(home).unwrap_or_else(|_| home.to_path_buf());
    if home.starts_with(&root) { real } else { root }
}

#[allow(clippy::too_many_lines)]
pub fn prepare(
    c: &Common,
    cwd: &Path,
    env: &Env,
    limits: &Limits,
    front: Front,
    resume: Option<&synthesis_tui::Resume>,
) -> Result<Prepared, String> {
    let (cli_allow, cli_deny) = (
        rules_arg(&c.allow, "--allow")?,
        rules_arg(&c.deny, "--deny")?,
    );
    let is_tui = matches!(front, Front::Tui);
    let h = homes(env)?;
    let synthesis_deny = synthesis_sandbox::synthesis_state_paths(&h.synthesis);
    // Before anything is created (worktree, store): a bad grant is a usage error.
    let reads = {
        let mut probe = SandboxPolicy::standard(cwd, Path::new("/synthesis-probe-tmp"));
        probe.deny_paths.extend(synthesis_deny.clone());
        c.sandbox_read
            .iter()
            .map(|r| {
                probe
                    .check_read_root(r)
                    .map_err(|e| format!("--sandbox-read {e}"))
            })
            .collect::<Result<Vec<_>, _>>()?
    };
    let trust = if c.trust_project {
        ProjectTrust::Trusted
    } else {
        ProjectTrust::Untrusted
    };
    let lc = LoadContext {
        cwd,
        synthesis_home: &h.synthesis,
        user_home: &h.user,
        env,
    };
    let cfg = load_config(env, &h, Some(cwd), trust)?;
    let user_cfg = if c.trust_project {
        load_config(env, &h, Some(cwd), ProjectTrust::Untrusted)?
    } else {
        cfg.clone()
    };
    let settings = load_settings(&lc, trust);
    // A settings file we cannot read would silently drop its deny rules: stop before anything runs
    // (`synth run` exits 2; the TUI never opens).
    if !settings.errors.is_empty() {
        return Err(format!(
            "settings error: {}\nsynth: fix or remove the file; nothing was started because its deny/ask rules would not apply",
            sanitize_for_display(&settings.errors.join("; "))
        ));
    }
    let user_settings = if c.trust_project {
        load_settings(&lc, ProjectTrust::Untrusted)
    } else {
        settings.clone()
    };
    let vs = crate::verify_cfg::parse(&settings.synthesis.verification);
    for w in cfg
        .warnings
        .iter()
        .chain(&settings.diagnostics)
        .chain(&vs.diagnostics)
    {
        eprintln!("synth: warning: {}", sanitize_for_display(w));
    }
    if !settings.env_refused.is_empty() {
        eprintln!(
            "synth: warning: settings env: refused secret-shaped name(s): {}",
            sanitize_for_display(&settings.env_refused.join(", "))
        );
    }
    let res = resolve_provider(c, &cfg, &h.synthesis, env)?;

    let price = pricing(&h.synthesis.join("settings.json"), &res.label, &res.model);
    if limits.max_cost_usd.is_some() && price.is_none() {
        return Err(format!(
            "--max-cost-usd needs a price for {}: add synthesis.pricing[\"{}\"] = {{\"input_per_mtok\": N, \"output_per_mtok\": N}} to {} (see docs/specs/config.md §6)",
            res.label,
            res.label,
            h.synthesis.join("settings.json").display()
        ));
    }

    let mode = match c.permission_mode {
        Some(ModeArg::Default) => Mode::Default,
        Some(ModeArg::AcceptEdits) => Mode::AcceptEdits,
        Some(ModeArg::Plan) => Mode::Plan,
        Some(ModeArg::DontAsk) => Mode::DontAsk,
        Some(ModeArg::Auto) => Mode::Auto,
        Some(ModeArg::BypassPermissions) => {
            // A TUI session switches into bypass with a confirmation dialog instead (Shift+Tab
            // never reaches it); the flag is for `synth run`.
            if matches!(front, Front::Tui) {
                return Err(
                    "--permission-mode bypassPermissions is only for `synth run`; in the TUI press Shift+Tab until the bypass dialog opens"
                        .into(),
                );
            }
            Mode::BypassPermissions
        }
        // Settings (user or project) never select bypassPermissions: only the CLI flag does.
        None => match Mode::parse(&settings.permissions.default_mode) {
            Some(m @ (Mode::Default | Mode::AcceptEdits | Mode::Plan | Mode::DontAsk)) => m,
            _ => Mode::Default,
        },
    };
    // Managed settings beat the CLI flag.
    let mode = match settings
        .managed_default_mode
        .as_deref()
        .and_then(Mode::parse)
    {
        Some(m @ (Mode::Default | Mode::AcceptEdits | Mode::Plan | Mode::DontAsk)) => m,
        _ => mode,
    };
    let mut budget = resume.map_or_else(
        || budget_from_settings(&settings),
        |r| r.record.budget.clone(),
    );
    if let Some(v) = limits.max_turns {
        budget.max_turns = v;
    }
    if let Some(v) = limits.max_tool_calls {
        budget.max_tool_calls = v;
    }
    if let Some(v) = vs.max_attempts {
        budget.max_verify_attempts = v;
    }
    budget.max_cost_usd = limits.max_cost_usd;
    // an unattended run must not run (and spend) forever; the TUI has a person watching
    budget.max_wall_ms = limits
        .max_wall_secs
        .or((!is_tui).then_some(HEADLESS_WALL_SECS))
        .map(|s| s.saturating_mul(1000));

    let mut extra_dirs: Vec<PathBuf> = settings
        .permissions
        .additional_directories
        .iter()
        .filter_map(|d| match std::fs::canonicalize(d) {
            Ok(p) if Path::new(d).is_absolute() && p.is_dir() => Some(p),
            _ => {
                eprintln!("synth: warning: ignored additional directory (not an existing absolute directory)");
                None
            }
        })
        .collect();

    let sid = resume.map_or_else(|| uuid::Uuid::new_v4().to_string(), |r| r.record.id.clone());
    let artifact_root = h.synthesis.join("sessions").join(&sid);
    let (ws, worktree) = if let Some(r) = resume {
        (
            r.record
                .open_workspace(&h.synthesis)
                .map_err(|e| e.to_string())?,
            None,
        )
    } else if c.worktree {
        if !extra_dirs.is_empty() {
            eprintln!("synth: warning: additionalDirectories are ignored with --worktree");
            extra_dirs.clear();
        }
        let (w, ctx) = crate::wt::create(env, &h.synthesis, cwd, &sid, artifact_root.clone())?;
        (ctx, Some(w))
    } else {
        (
            WorkspaceContext::open(
                format!("ws-{}", &sid[..8]),
                &workspace_root(cwd, &h.user),
                cwd,
                &extra_dirs,
                artifact_root.clone(),
                sid.clone(),
            )
            .map_err(|e| e.to_string())?,
            None,
        )
    };
    let session = match resume {
        Some(r) => AgentSession {
            id: sid.clone(),
            task_id: None,
            status: r.record.status,
            workspace: ws.clone(),
            model_config_id: res.label.clone(),
            permission_policy_id: r.record.permission_policy_id.clone(),
            // a session never resumes in bypassPermissions
            permission_mode: match r.record.permission_mode {
                synthesis_core::PermissionMode::BypassPermissions => from_mode(mode),
                m => m,
            },
            event_cursor: r.record.event_cursor,
            budget: budget.clone(),
            usage: r.record.usage.clone(),
            created_at: r.record.created_at.clone(),
            updated_at: r.record.updated_at.clone(),
            parent: None,
        },
        None => AgentSession {
            id: sid.clone(),
            task_id: None,
            status: SessionStatus::Created,
            workspace: ws.clone(),
            model_config_id: res.label.clone(),
            permission_policy_id: "default".into(),
            permission_mode: from_mode(mode),
            event_cursor: 0,
            budget: budget.clone(),
            usage: BudgetUsage::default(),
            created_at: String::new(),
            updated_at: String::new(),
            parent: None,
        },
    };
    let ws = Arc::new(ws);

    let sandbox_mode = c.sandbox_mode();
    let net = c.network.policy();
    let git_dir = worktree.as_ref().map(|w| w.info.git_dir.clone());
    let state_paths = synthesis_deny.clone();
    let build = Arc::new(move |ws: &synthesis_core::WorkspaceContext| {
        let mut p = standard_policy_git(ws, net.clone(), git_dir.as_deref())?;
        p.deny_paths.extend(synthesis_deny.clone());
        p.readable_roots.extend(reads.iter().cloned());
        Ok::<_, String>(p)
    });
    // Per call an Err becomes None, which the terminal runtime REFUSES under Enforce (it only
    // runs unconfined in best-effort/off); preflight below reports the Err once.
    let make_policy = {
        let build = build.clone();
        Arc::new(move |ws: &synthesis_core::WorkspaceContext| build(ws).ok())
    };
    let policy = build(&ws);
    match (&policy, sandbox_mode) {
        (Err(e), SandboxMode::Enforce) => {
            return Err(format!("cannot build the sandbox policy: {e}"));
        }
        (Err(e), SandboxMode::BestEffort) => eprintln!(
            "synth: warning: cannot build the sandbox policy ({e}); commands run unconfined"
        ),
        _ => {}
    }
    if sandbox_mode == SandboxMode::Enforce {
        sandbox_preflight(&*synthesis_sandbox::detect(), policy.as_ref().ok())?;
    }
    let mut notices = sandbox_notices(sandbox_mode, c.network);
    notices.extend(sandbox_default_notice(c));
    if ws.root != ws.cwd {
        notices.push(format!(
            "workspace root is {} (launched in {}); the sandbox may write there",
            sanitize_for_display(&ws.root.display().to_string()),
            sanitize_for_display(&ws.cwd.display().to_string())
        ));
    }
    match mode {
        Mode::BypassPermissions => notices.push(
            "bypassPermissions: tool calls run without approval; only deny rules, ask rules, critical removals and destructive MCP asks still stop them; protected paths are not stopped"
                .to_string(),
        ),
        Mode::Auto => notices.push(
            "auto mode: a model call judges each call the rules leave open (allow / deny / ask); an unclear answer is an ask, which this run denies"
                .to_string(),
        ),
        _ => {}
    }
    for n in &notices {
        eprintln!("synth: WARNING: {n}");
    }

    let store: Option<Store> = if c.no_session_store {
        None
    } else {
        let opts = StoreOptions {
            write_node_transcripts: settings
                .synthesis
                .compat
                .write_node_transcripts
                .unwrap_or(true),
            secrets: res.secrets.clone(),
            ..StoreOptions::default()
        };
        let w = SessionStore::open(&h.synthesis, opts)
            .and_then(|s| match resume {
                Some(_) => s.resume_session(&sid, true),
                None => s.create_session(NewSession {
                    id: Some(sid.clone()),
                    workspace: &ws,
                    model_config_id: res.label.clone(),
                    permission_mode: session.permission_mode,
                    permission_policy_id: "default".into(),
                    budget: budget.clone(),
                    parent: None,
                    title: None,
                }),
            })
            .map_err(|e| {
                format!("session store: {e} (use --no-session-store to run without it)")
            })?;
        Some(Arc::new(Mutex::new(w)))
    };
    let transcript = store.clone().map(|s| {
        let t = Transcript::new(s);
        t.skip(resume.map_or(0, |r| r.history.len()));
        Arc::new(t)
    });
    let derived = match (&store, &transcript) {
        (Some(_), Some(t)) => match TaskDb::open(&h.synthesis, &sid, res.secrets.clone()) {
            Ok(db) => Some((db, t.clone())),
            Err(e) => {
                eprintln!("synth: warning: task rows disabled: {e}");
                None
            }
        },
        _ => None,
    };
    let cancel = CancellationToken::new();
    let (out, tee, prompt, link, approver, closed): (
        Box<dyn std::io::Write + Send>,
        _,
        String,
        _,
        Arc<dyn Approver>,
        _,
    ) = match front {
        Front::Headless {
            prompt,
            tee,
            closed,
        } => (
            Box::new(std::io::stdout()),
            tee,
            prompt,
            None,
            Arc::new(AuditDeny),
            Some((closed, cancel.clone())),
        ),
        Front::Tui => {
            let l = synthesis_tui::UiLink::new();
            let ap = l.approver();
            (
                Box::new(l.event_writer()),
                None,
                String::new(),
                Some(l),
                ap,
                None,
            )
        }
    };
    let sink = EventSink::new(
        Persist::new(
            out,
            tee,
            store.clone(),
            derived,
            resume.map_or(0, |r| r.record.event_cursor),
            closed,
        ),
        sid.clone(),
        res.secrets.clone(),
    );

    // settings.env: opt-in additions on top of the allowlisted child env (config.md §6). A
    // variable the process env already supplies to children wins, as in Node's agentEnv.
    let mut child_env: BTreeMap<String, String> = env
        .child_env()
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    for (k, v) in &settings.env {
        if env_pair_ok(k, v) && !child_env.contains_key(k) {
            child_env.insert(k.clone(), v.clone());
        }
    }
    let rt = Arc::new(TerminalRuntime::new(sandbox_mode));
    rt.open_session_sandboxed(&sid, ws.clone(), child_env.clone(), policy.ok());

    let rules = PermissionRules {
        allow: settings
            .permissions
            .allow
            .iter()
            .chain(&cli_allow)
            .cloned()
            .collect(),
        deny: settings
            .permissions
            .deny
            .iter()
            .chain(&cli_deny)
            .cloned()
            .collect(),
        ask: settings.permissions.ask.clone(),
        additional_directories: extra_dirs,
    };
    // `interactive` lets asks reach the approver, so each one leaves approval events.
    let mut perms = Permissions::new(
        rules,
        // the session's workspace (the worktree under --worktree), not the launch dir
        synthesis_permissions::Env::new(&ws.cwd, &h.user, mode)
            .with_project_root(&ws.root)
            .interactive(true)
            .protect(state_paths),
    );
    perms.env.auto_mode = synthesis_permissions::AutoModeSettings {
        classify_all_shell: user_settings.auto_mode.classify_all_shell.unwrap_or(false),
        use_auto_mode_during_plan: user_settings
            .auto_mode
            .use_auto_mode_during_plan
            .unwrap_or(true),
    };
    perms.block_outside_reads = settings.permissions.block_reads_outside_working_directories;

    let mut hcfg = HarnessConfig::new(res.model.clone());
    hcfg.max_output_tokens = cfg.max_tokens as u32;
    hcfg.env = child_env;
    hcfg.pricing = price;
    let (auto_compact, diag) = crate::ctx::read_auto_compact(&h.synthesis.join("settings.json"));
    for d in &diag {
        eprintln!("synth: warning: {d}");
    }
    hcfg.auto_compact = auto_compact;
    let (shell, diag) = crate::ctx::read_shell_settings(&h.synthesis.join("settings.json"));
    for d in &diag {
        eprintln!("synth: warning: {d}");
    }
    hcfg.shell = shell;
    let am = &user_settings.auto_mode;
    hcfg.auto.model = am.model.clone();
    hcfg.auto.rules = synthesis_permissions::automode::effective_rules(
        &synthesis_permissions::automode::AutoModeRules {
            environment: am.environment.clone(),
            allow: am.allow.clone(),
            soft_deny: am.soft_deny.clone(),
            hard_deny: am.hard_deny.clone(),
        },
    );

    let put: PutArtifact = match &store {
        Some(s) => {
            let s = s.clone();
            Box::new(move |b| {
                s.lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .put_artifact("summary", "text/plain", b, Retention::Session)
                    .map_err(|e| e.to_string())
            })
        }
        None => artifact_to_dir(artifact_root.clone()),
    };
    let user_files = [h.synthesis.join("SYNTHESIS.md")];
    // Read-deny rules apply to instruction files and their imports; a denied file is reported.
    let denied = Mutex::new(Vec::<PathBuf>::new());
    let deny = |p: &Path| {
        let d = read_denied(&perms, p);
        if d {
            denied
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(p.to_path_buf());
        }
        d
    };
    let mut instructions = synthesis_context::load_instructions_with(
        &ws,
        &user_files,
        Some(&h.user),
        &synthesis_context::DEFAULT_NAMES,
        &synthesis_context::InstructionLimits::default(),
        Some(&deny),
    );
    let mut denied = denied.into_inner().unwrap_or_else(|e| e.into_inner());
    denied.sort();
    denied.dedup();
    for p in denied {
        let w = format!(
            "instruction file not loaded (blocked by a Read deny rule): {}",
            p.display()
        );
        eprintln!("synth: warning: {}", sanitize_for_display(&w));
        notices.push(w);
    }
    // lowest priority: before the user and repository files
    instructions.splice(
        0..0,
        crate::ctx::load_auto_memory(&h.synthesis, &[&ws.root, &ws.cwd]),
    );
    let notes = crate::memory::Notes {
        user: user_files[0].clone(),
        project: crate::memory::project_file(&ws.root),
        root: ws.root.clone(),
        secrets: res.secrets.clone(),
        loaded: crate::memory::describe(&instructions),
    };
    let skills = crate::ctx::visible_skills(
        synthesis_tools::discover_skills(env, &ws.root),
        trust == ProjectTrust::Trusted,
    );
    let mut context = SynthesisContext::new(
        instructions,
        &prompt,
        res.provider.clone(),
        res.model.clone(),
        cancel.clone(),
        put,
    )
    .with_skills(&skills);
    if let Some(t) = &transcript {
        context = context.with_transcript(t.clone());
    }
    let agents = crate::agents::load(&h.synthesis, &settings.project_root, trust);
    if !agents.skipped.is_empty() {
        let w = format!(
            "{} project agent definition(s) not loaded (project not trusted)",
            agents.skipped.len()
        );
        eprintln!("synth: warning: {w}");
        notices.push(w);
    }

    // Everything fallible is done: start MCP servers last so an early error never leaves one.
    let tokio_rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| e.to_string())?;
    let mut mplan = crate::mcp::plan(
        &user_cfg,
        &settings,
        &user_settings,
        cwd,
        trust,
        &c.approve_mcp,
    );
    mplan.env_passthrough = crate::mcp::read_passthrough(&h.synthesis.join("settings.json"));
    mplan.guard = crate::mcp::read_guard(
        &h.synthesis.join("settings.json"),
        (trust == ProjectTrust::Trusted)
            .then(|| {
                settings
                    .project_root
                    .join(".synthesis")
                    .join("settings.json")
            })
            .as_deref(),
    );
    mplan.artifact_dir = Some(artifact_root.join("mcp"));
    // Printed now (before any TUI alt screen) and kept in `notices`, which become `notice`
    // events (headless) or TUI startup notes, so approvals stay visible after the screen clears.
    for n in &mplan.notices {
        let n = sanitize_for_display(n);
        eprintln!("synth: {n}");
        notices.push(n);
    }
    for (n, why) in &mplan.skipped {
        let w = format!(
            "MCP server \"{}\" not started: {}",
            sanitize_for_display(n),
            sanitize_for_display(why)
        );
        eprintln!("synth: warning: {w}");
        notices.push(w);
    }
    let (mcp, mcp_attach) = if mplan.servers.is_empty() {
        (None, None)
    } else {
        let (m, t, warns) = tokio_rt.block_on(crate::mcp::start(&mplan, env, &ws.root));
        for w in warns {
            eprintln!("synth: warning: {}", sanitize_for_display(&w));
        }
        (Some(m), Some(t))
    };

    // interactive: a Bash call that times out moves to the background; headless kills it (D6)
    let mut toolbox = ToolBox::new(
        Registry::with_terminal_policy(rt.clone(), make_policy)
            .with_new_tools()
            .with_background_on_timeout(is_tui),
    );
    if let Some(a) = mcp_attach {
        toolbox = a.apply(toolbox);
    }
    let gate_cfg = vs.gate(budget.max_verify_attempts);
    let rt_gate = rt.clone();
    let mut harness = Harness::new(res.provider.clone(), toolbox, perms, sink.clone(), hcfg)
        .with_approver(approver)
        .with_agents(agents.table)
        .with_context(Arc::new(context))
        .with_gate(Box::new(move |s| {
            CompletionGate::new(
                VerifyRunner::new(rt_gate.clone(), &s.id, Arc::new(s.workspace.clone())),
                gate_cfg.clone(),
            )
        }));
    if let Some(l) = &link {
        harness = harness.with_input(l.input());
    }
    // "Allow always" persists to .synthesis/settings.local.json, but only in a trusted project: an
    // untrusted project's allow rules are ignored on load, so writing them would be misleading.
    if trust == ProjectTrust::Trusted {
        let root = settings.project_root.clone();
        harness = harness.with_rule_store(Arc::new(move |rules| {
            if let Err(e) = synthesis_settings::persist_allow_rules(&root, rules) {
                eprintln!(
                    "synth: warning: could not save the rule: {}",
                    sanitize_for_display(&e.to_string())
                );
            }
        }));
    }
    if !settings.disable_all_hooks && !settings.hooks.is_empty() {
        harness = harness.with_hooks(Arc::new(Hooks::new(
            rt.clone(),
            sid.clone(),
            ws.clone(),
            settings.hooks.clone(),
        )));
    }

    eprintln!(
        "synth: session {sid} model {} cwd {} mode {}",
        res.label,
        ws.cwd.display(),
        mode.as_str()
    );
    if link.is_some() {
        // Event log / replay copy; the UI also gets `notices` as startup notes (tui.rs) and shows
        // each text once, so it no longer depends on this channel buffering.
        for n in &notices {
            sink.emit(
                &Scope::default(),
                Event::Notice {
                    level: "warning".into(),
                    text: n.clone(),
                },
            );
        }
    }
    let models = synthesis_config::list_models(&cfg, None)
        .into_iter()
        .map(|m| m.value)
        .collect();
    let effort_tiers = res
        .label
        .strip_suffix(&format!("/{}", res.model))
        .and_then(|id| cfg.providers.get(id)?.models_meta.as_ref()?.get(&res.model))
        .and_then(|m| m.capabilities.get("effort_tiers")?.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();
    Ok(Prepared {
        harness: Arc::new(harness),
        session,
        sid,
        cancel,
        sink,
        rt,
        store,
        transcript,
        tokio: tokio_rt,
        mcp,
        notices,
        worktree,
        criteria: vs.checks,
        artifact_root,
        label: res.label,
        models,
        effort_tiers,
        sandbox: sandbox_status(sandbox_mode, sandbox_mode != SandboxMode::Enforce),
        link,
        homes: h,
        notes,
    })
}

pub fn run(a: RunArgs, env: Env) -> Result<u8, String> {
    let prompt = read_prompt(&a)?;
    let mut criteria = a
        .criteria_file
        .as_deref()
        .map(read_criteria)
        .transpose()?
        .unwrap_or_default();
    if a.max_cost_usd.is_some_and(|c| !c.is_finite() || c <= 0.0) {
        return Err("--max-cost-usd must be a positive number".into());
    }
    let dir = match a.cwd.as_ref() {
        Some(d) => d.clone(),
        None => crate::launch_dir()?,
    };
    let cwd =
        synthesis_core::canonicalize(&dir).map_err(|e| format!("--cwd {}: {e}", dir.display()))?;
    if !cwd.is_dir() {
        return Err(format!("--cwd {} is not a directory", cwd.display()));
    }
    let tee = a
        .events_out
        .as_ref()
        .map(|p| create_private(p).map_err(|e| format!("--events-out {}: {e}", p.display())))
        .transpose()?;
    let limits = Limits {
        max_turns: a.max_turns,
        max_tool_calls: a.max_tool_calls,
        max_cost_usd: a.max_cost_usd,
        max_wall_secs: a.max_wall_secs,
    };
    let closed = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let p = prepare(
        &a.common,
        &cwd,
        &env,
        &limits,
        Front::Headless {
            prompt: prompt.clone(),
            tee,
            closed: closed.clone(),
        },
        None,
    )?;
    criteria.extend(p.criteria.iter().cloned());
    let Prepared {
        harness,
        session,
        sid,
        cancel,
        sink,
        rt,
        store,
        transcript,
        tokio: tokio_rt,
        mcp,
        notices,
        worktree,
        artifact_root,
        ..
    } = p;
    let cleanup = a.common.cleanup_worktree;
    let code = tokio_rt.block_on(async {
        on_signals(cancel.clone()).map_err(|e| format!("signal handlers: {e}"))?;
        let (store2, sink2, h2) = (store.clone(), sink.clone(), harness.clone());
        let task = tokio::spawn(async move {
            let mut session = session;
            let r = async {
                h2.start_session(&mut session, "startup")?;
                for n in notices {
                    sink2.emit(
                        &Scope::default(),
                        Event::Notice {
                            level: "warning".into(),
                            text: n,
                        },
                    );
                }
                let out = h2
                    .run_task(
                        &mut session,
                        TaskRequest {
                            request: prompt,
                            criteria,
                            history: vec![],
                            display_text: None,
                        },
                        cancel,
                    )
                    .await?;
                if let Some(w) = &worktree {
                    sink2.emit(
                        &Scope {
                            task_id: Some(out.task_id.clone()),
                            ..Scope::default()
                        },
                        Event::Notice {
                            level: "info".into(),
                            text: w.finish(cleanup),
                        },
                    );
                }
                h2.end_session(&out);
                Ok::<_, synthesis_harness::HarnessError>(out)
            }
            .await;
            if let (Some(s), Ok(out)) = (&store2, &r) {
                finish_store(s, transcript.as_deref(), out, &session.usage);
            }
            r
        });
        let code = match task.await {
            Ok(Ok(out)) => {
                eprintln!("synth: task {:?}: {}", out.status, out.reason);
                if let Some(h) = blocked_hint(&out) {
                    eprintln!("{h}");
                }
                exit_code(out.status)
            }
            Ok(Err(e)) => {
                eprintln!("synth: internal error: {e}");
                70
            }
            Err(e) => {
                eprintln!("synth: internal error: {e}");
                sink.emit(
                    &Scope::default(),
                    Event::SessionFailed(SessionEnd {
                        reason: "internal_error".into(),
                        error: Some(e.to_string()),
                    }),
                );
                70
            }
        };
        if let Some(m) = &mcp {
            m.close().await; // stdio servers get EOF, then their process group is killed
        }
        Ok::<u8, String>(code)
    })?;
    rt.close_session(&sid); // kills leftover background processes
    drop(harness);
    let code = if closed.load(std::sync::atomic::Ordering::SeqCst) {
        EXIT_STDOUT_CLOSED
    } else {
        code
    };
    if a.common.no_session_store {
        let _ = std::fs::remove_dir_all(&artifact_root);
    }
    Ok(code)
}

/// Flushes the conversation tail and the usage to the store.
pub fn finish_store(
    store: &Store,
    tr: Option<&Transcript>,
    out: &TaskOutcome,
    usage: &BudgetUsage,
) {
    if let Some(t) = tr {
        t.sync(&out.messages);
    }
    if let Err(e) = store
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .update_usage(usage)
    {
        eprintln!("synth: warning: session store: {e}");
    }
}

fn read_prompt(a: &RunArgs) -> Result<String, String> {
    let p = match (&a.prompt, &a.prompt_file) {
        (Some(p), _) => p.clone(),
        (_, Some(f)) => {
            std::fs::read_to_string(f).map_err(|e| format!("--prompt-file {}: {e}", f.display()))?
        }
        _ => {
            if std::io::stdin().is_terminal() {
                return Err("no prompt: pass --prompt, --prompt-file or pipe it on stdin".into());
            }
            let mut s = String::new();
            std::io::stdin()
                .read_to_string(&mut s)
                .map_err(|e| format!("stdin: {e}"))?;
            s
        }
    };
    if p.trim().is_empty() {
        return Err("empty prompt".into());
    }
    Ok(p)
}

fn read_criteria(f: &Path) -> Result<Vec<Criterion>, String> {
    let bad = |e: String| format!("--criteria-file {}: {e}", f.display());
    let raw = std::fs::read_to_string(f).map_err(|e| bad(e.to_string()))?;
    let items: Vec<Value> = serde_json::from_str(&raw).map_err(|e| bad(e.to_string()))?;
    items
        .iter()
        .enumerate()
        .map(|(i, v)| {
            let text = v
                .as_str()
                .or_else(|| v.get("text").and_then(Value::as_str))
                .ok_or_else(|| bad(format!("item {i}: missing text")))?;
            let id = v
                .get("id")
                .and_then(Value::as_str)
                .map_or(format!("c{}", i + 1), String::from);
            Ok(Criterion::user(
                &id,
                text,
                v.get("check").and_then(Value::as_str),
            ))
        })
        .collect()
}

struct Resolved {
    provider: Arc<dyn ModelProvider>,
    model: String,
    label: String,
    secrets: Vec<String>,
}

/// The error `synth run` exits 2 with when no model is configured; the interactive app starts its
/// first-run setup on this prefix instead (`tui.rs`).
pub const NO_MODEL: &str =
    "no model configured: pass --model provider/model or set SYNTHESIS_MODEL";

fn resolve_provider(
    a: &Common,
    cfg: &Config,
    synthesis_home: &Path,
    env: &Env,
) -> Result<Resolved, String> {
    if let Some(script) = a.provider.as_deref().and_then(|p| p.strip_prefix("fake:")) {
        #[cfg(feature = "test-provider")]
        {
            let model = a.model.clone().unwrap_or_else(|| "fake-model".into());
            return Ok(Resolved {
                provider: crate::fake::load(Path::new(script))?,
                label: format!("fake/{model}"),
                model,
                secrets: vec![],
            });
        }
        #[cfg(not(feature = "test-provider"))]
        return Err(format!(
            "--provider fake:{script} needs a build with the test-provider feature"
        ));
    }
    let spec = match (&a.provider, &a.model) {
        (Some(p), Some(m)) if !m.contains('/') => Some(format!("{p}/{m}")),
        (Some(p), None) => cfg
            .model
            .clone()
            .filter(|m| m.starts_with(&format!("{p}/")))
            .or_else(|| {
                cfg.providers
                    .get(p)?
                    .models
                    .as_ref()?
                    .first()
                    .map(|m| format!("{p}/{m}"))
            }),
        (_, m) => m.clone(),
    };
    let parsed = parse_model(spec.as_deref(), cfg).map_err(|_| NO_MODEL.to_string())?;
    let id = parsed.provider_id.ok_or("no provider configured")?;
    let p = cfg
        .providers
        .get(&id)
        .ok_or_else(|| format!("unknown provider \"{id}\""))?;
    let cred = resolve_credential(&id, Some(p), synthesis_home, env);
    let secrets = cred.iter().map(|c| c.secret.clone()).collect();
    if cred.is_none() && !p.is_no_auth() {
        // Name the variable the provider's config points at (the warning only carries the name).
        let var = cfg
            .warnings
            .iter()
            .filter(|w| w.contains(&format!("provider \"{id}\"")))
            .find_map(|w| w.split("missing env var \"").nth(1)?.split('"').next());
        return Err(match var {
            Some(v) => format!("no API key for provider \"{id}\": set {v} and restart"),
            None => format!(
                "no API key for provider \"{id}\": set SYNTHESIS_API_KEY (when SYNTHESIS_PROVIDER={id}) or add apiKey to its config"
            ),
        });
    }
    let provider = ProviderSpec::from_config(&id, p, cred.as_ref())
        .and_then(|s| s.connect(env, &NetSettings::default(), RetryPolicy::default()))
        .map_err(|e| e.message().to_string())?;
    Ok(Resolved {
        provider,
        label: format!("{id}/{}", parsed.model),
        model: parsed.model,
        secrets,
    })
}

/// stdout was closed by the reader; the task was cancelled and its events are in the store.
pub const EXIT_STDOUT_CLOSED: u8 = 141;

/// `synth: blocked: approval needed for ... - rerun with --allow ... or in the TUI`.
fn blocked_hint(out: &TaskOutcome) -> Option<String> {
    if out.needed_permissions.is_empty() {
        return None;
    }
    // Blocked is the usual outcome; a model that worked around the denial still gets told.
    let lead = if out.status == TaskStatus::Blocked {
        "synth: blocked: approval needed for"
    } else {
        "synth: note: approval was needed but unavailable for"
    };
    let rules = &out.needed_permissions;
    // The rule text derives from the model's command: never offer it paste-ready unless it is inert
    // in a shell (no quote, expansion, separator, redirection or control character).
    let paste_safe = |r: &str| {
        !r.chars()
            .any(|c| c.is_control() || "'\"$`\\;|&<>".contains(c))
    };
    let flags: Vec<String> = rules
        .iter()
        .filter(|r| paste_safe(r))
        .map(|r| format!("--allow '{r}'"))
        .collect();
    let names = rules
        .iter()
        .map(|r| sanitize_for_display(r))
        .collect::<Vec<_>>()
        .join(", ");
    let how = if flags.len() == rules.len() {
        format!("rerun with {} or in the TUI", flags.join(" "))
    } else {
        "the rule text comes from the model's command, so review it and add it with --allow yourself, or approve in the TUI".to_string()
    };
    Some(format!("{lead} {names} \u{2014} {how}"))
}

fn exit_code(s: TaskStatus) -> u8 {
    match s {
        TaskStatus::Completed => 0,
        TaskStatus::Failed => 1,
        TaskStatus::Unverified => 3,
        TaskStatus::Blocked => 4,
        TaskStatus::Cancelled => 130,
        _ => 70,
    }
}

#[cfg(unix)]
fn on_signals(cancel: CancellationToken) -> std::io::Result<()> {
    use tokio::signal::unix::{SignalKind, signal};
    let mut int = signal(SignalKind::interrupt())?;
    let mut term = signal(SignalKind::terminate())?;
    tokio::spawn(async move {
        tokio::select! { _ = int.recv() => {}, _ = term.recv() => {} }
        cancel.cancel();
        tokio::select! { _ = int.recv() => {}, _ = term.recv() => {} }
        std::process::exit(130); // second signal: stop waiting for a clean shutdown
    });
    Ok(())
}

#[cfg(not(unix))]
fn on_signals(cancel: CancellationToken) -> std::io::Result<()> {
    tokio::spawn(async move {
        let _ = tokio::signal::ctrl_c().await;
        cancel.cancel();
        let _ = tokio::signal::ctrl_c().await;
        std::process::exit(130);
    });
    Ok(())
}

/// Creates (or truncates) a file readable by the owner only; 0600 on unix even if it existed.
fn create_private(path: &Path) -> std::io::Result<std::fs::File> {
    let mut o = std::fs::OpenOptions::new();
    o.write(true).create(true).truncate(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut o, 0o600);
    let f = o.open(path)?;
    #[cfg(unix)]
    f.set_permissions(std::os::unix::fs::PermissionsExt::from_mode(0o600))?;
    Ok(f)
}

fn artifact_to_dir(dir: PathBuf) -> PutArtifact {
    Box::new(move |bytes| {
        let id = uuid::Uuid::new_v4().to_string();
        std::fs::create_dir_all(dir.join("artifacts")).map_err(|e| e.to_string())?;
        let path = format!("artifacts/{id}.txt");
        create_private(&dir.join(&path))
            .and_then(|mut f| std::io::Write::write_all(&mut f, bytes))
            .map_err(|e| e.to_string())?;
        let sha256 = Sha256::digest(bytes)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        Ok(ArtifactRef {
            artifact_id: id,
            path,
            size: bytes.len() as u64,
            sha256,
            mime: "text/plain".into(),
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sandbox_defaults_to_enforce_except_where_nothing_can_confine() {
        use clap::Parser;
        #[derive(Parser)]
        struct W {
            #[command(flatten)]
            c: Common,
        }
        assert_eq!(default_sandbox_mode(false), SandboxMode::Enforce);
        assert_eq!(default_sandbox_mode(true), SandboxMode::BestEffort);
        let none = W::parse_from(["x"]).c;
        let want = if cfg!(windows) {
            SandboxMode::BestEffort
        } else {
            SandboxMode::Enforce
        };
        assert_eq!(none.sandbox_mode(), want);
        // the loud one-liner exists exactly when the default turned best-effort
        assert_eq!(sandbox_default_notice(&none).is_some(), cfg!(windows));
        // an explicit flag always wins, and then there is no "defaulted" notice
        for (flag, mode) in [
            ("enforce", SandboxMode::Enforce),
            ("best-effort", SandboxMode::BestEffort),
            ("off", SandboxMode::Off),
        ] {
            let c = W::parse_from(["x", "--sandbox", flag]).c;
            assert_eq!(c.sandbox_mode(), mode);
            assert!(sandbox_default_notice(&c).is_none());
        }
    }

    fn outcome(rules: &[&str]) -> TaskOutcome {
        TaskOutcome {
            task_id: String::new(),
            status: TaskStatus::Blocked,
            reason: String::new(),
            verification_report_id: None,
            final_text: String::new(),
            usage: synthesis_events::Usage {
                input_tokens: 0,
                output_tokens: 0,
                cache_read_input_tokens: None,
                cache_creation_input_tokens: None,
                cost_usd: None,
            },
            changed_files: vec![],
            needed_permissions: rules.iter().map(|r| r.to_string()).collect(),
            messages: vec![],
        }
    }

    #[test]
    fn blocked_hint_offers_only_shell_inert_rules() {
        let h = blocked_hint(&outcome(&["Bash(cargo test:*)"])).unwrap();
        assert!(h.contains("--allow 'Bash(cargo test:*)'"), "{h}");
        for bad in [
            "Bash(a'; rm -rf ~:*)",
            "Bash($(id):*)",
            "Bash(a`b`:*)",
            "Bash(a\\b)",
            "Bash(a|b)",
            "Bash(a\nb)",
        ] {
            let h = blocked_hint(&outcome(&[bad])).unwrap();
            assert!(!h.contains("--allow '"), "{h}");
            assert!(h.contains("comes from the model's command"), "{h}");
        }
    }

    #[test]
    fn preflight_refuses_loopback_the_backend_cannot_honour() {
        struct NoLoopback;
        impl SandboxBackend for NoLoopback {
            fn name(&self) -> &'static str {
                "fake"
            }
            fn capabilities(&self) -> synthesis_terminal::SandboxCapabilities {
                synthesis_terminal::SandboxCapabilities {
                    filesystem: Guarantee::Strong,
                    network: Guarantee::Strong,
                    process_tree: Guarantee::Strong,
                    notes: vec![],
                }
            }
            fn wrap(
                &self,
                _: &synthesis_terminal::SandboxPolicy,
                c: &SandboxedCommand,
            ) -> Result<SandboxedCommand, synthesis_sandbox::SandboxError> {
                Ok(c.clone())
            }
        }
        let mut p = synthesis_terminal::SandboxPolicy::default();
        assert!(sandbox_preflight(&NoLoopback, Some(&p)).is_ok());
        p.network = NetworkPolicy::Loopback;
        let e = sandbox_preflight(&NoLoopback, Some(&p)).unwrap_err();
        assert!(
            e.contains("--network loopback") && e.contains("synth doctor"),
            "{e}"
        );
    }

    #[test]
    fn preflight_refuses_a_backend_that_cannot_confine() {
        let e = sandbox_preflight(&synthesis_sandbox::NullSandbox, None).unwrap_err();
        assert!(
            e.contains("synth doctor") && e.contains("no OS sandbox"),
            "{e}"
        );
    }

    #[test]
    fn workspace_root_is_the_repo_root_not_the_launch_dir_and_never_home() {
        let d = tempfile::tempdir().unwrap();
        let base = synthesis_core::canonicalize(d.path()).unwrap();
        let (repo, sub) = (base.join("repo"), base.join("repo/src/deep"));
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        assert_eq!(workspace_root(&sub, &base), repo);
        assert_eq!(workspace_root(&repo, &base), repo);
        // `.synthesis` also marks a root
        let proj = base.join("proj");
        std::fs::create_dir_all(proj.join(".synthesis")).unwrap();
        std::fs::create_dir_all(proj.join("a")).unwrap();
        assert_eq!(workspace_root(&proj.join("a"), &base), proj);
        // no marker: the launch dir
        let lone = base.join("lone/x");
        std::fs::create_dir_all(&lone).unwrap();
        assert_eq!(workspace_root(&lone, &base), lone);
        // `$HOME/.synthesis` must not turn the home directory into the workspace
        std::fs::create_dir_all(base.join(".synthesis")).unwrap();
        assert_eq!(workspace_root(&lone, &base), lone);
        // ... nor may a marker ABOVE home (`/Users/.git`)
        std::fs::remove_dir(base.join(".synthesis")).unwrap();
        std::fs::create_dir_all(base.join(".git")).unwrap();
        let home = base.join("users/me");
        std::fs::create_dir_all(home.join("proj")).unwrap();
        assert_eq!(workspace_root(&home.join("proj"), &home), home.join("proj"));
    }

    #[cfg(unix)]
    #[test]
    fn a_world_writable_marker_directory_is_not_a_workspace_root() {
        use std::os::unix::fs::PermissionsExt;
        let d = tempfile::tempdir().unwrap();
        let base = synthesis_core::canonicalize(d.path()).unwrap();
        let (repo, sub) = (base.join("repo"), base.join("repo/src"));
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        let home = base.join("home");
        assert_eq!(workspace_root(&sub, &home), repo);
        std::fs::set_permissions(&repo, std::fs::Permissions::from_mode(0o777)).unwrap();
        assert_eq!(workspace_root(&sub, &home), sub, "writable by anyone");
        std::fs::set_permissions(&repo, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::set_permissions(repo.join(".git"), std::fs::Permissions::from_mode(0o777))
            .unwrap();
        assert_eq!(
            workspace_root(&sub, &home),
            sub,
            "marker writable by anyone"
        );
    }

    #[test]
    fn started_in_a_subfolder_the_repo_instructions_and_skills_are_found() {
        let d = tempfile::tempdir().unwrap();
        let base = synthesis_core::canonicalize(d.path()).unwrap();
        let (repo, sub) = (base.join("repo"), base.join("repo/pkg"));
        std::fs::create_dir_all(sub.clone()).unwrap();
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        std::fs::write(repo.join("AGENTS.md"), "root rules").unwrap();
        std::fs::write(sub.join("SYNTHESIS.md"), "pkg rules").unwrap();
        let root = workspace_root(&sub, &base.join("home"));
        let ws = WorkspaceContext::open("w", &root, &sub, &[], base.join("art"), "pg").unwrap();
        assert_eq!(ws.cwd, sub);
        let got = synthesis_context::load_instructions(
            &ws,
            &[],
            None,
            &synthesis_context::DEFAULT_NAMES,
            &synthesis_context::InstructionLimits::default(),
        );
        let text: Vec<_> = got.iter().map(|f| f.content.as_str()).collect();
        assert_eq!(text, ["root rules", "pkg rules"], "root first");
    }

    #[test]
    fn a_read_deny_rule_keeps_an_instruction_file_out_of_the_prompt() {
        let d = tempfile::tempdir().unwrap();
        let base = synthesis_core::canonicalize(d.path()).unwrap();
        std::fs::write(base.join("AGENTS.md"), "secret plan").unwrap();
        std::fs::write(base.join("docs.md"), "imported").unwrap();
        std::fs::create_dir_all(base.join("sub")).unwrap();
        std::fs::write(base.join("sub/SYNTHESIS.md"), "see @../docs.md").unwrap();
        let rules = PermissionRules {
            // `/x` is anchored at the cwd (= base)
            deny: vec!["Read(/AGENTS.md)".into(), "Read(/docs.md)".into()],
            ..PermissionRules::default()
        };
        let perms = Permissions::new(
            rules,
            synthesis_permissions::Env::new(&base, &base, Mode::Default),
        );
        assert!(read_denied(&perms, &base.join("AGENTS.md")));
        assert!(!read_denied(&perms, &base.join("sub/SYNTHESIS.md")));
        let ws = WorkspaceContext::open("w", &base, &base.join("sub"), &[], base.join("art"), "pg")
            .unwrap();
        let deny = |p: &Path| read_denied(&perms, p);
        let got = synthesis_context::load_instructions_with(
            &ws,
            &[],
            None,
            &synthesis_context::DEFAULT_NAMES,
            &synthesis_context::InstructionLimits::default(),
            Some(&deny),
        );
        let text: Vec<_> = got.iter().map(|f| f.content.as_str()).collect();
        assert_eq!(
            text,
            ["see @../docs.md"],
            "the denied AGENTS.md and the denied import are not injected"
        );
    }
}
