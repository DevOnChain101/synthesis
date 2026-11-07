//! `synt run`: one headless task. Exit codes follow docs/specs/events.md §6.

use std::collections::BTreeMap;
use std::io::{IsTerminal, Read};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use clap::{Args, ValueEnum};
use serde_json::Value;
use serde_json::json;
use sha2::{Digest, Sha256};
use synthesis_config::{Config, Env, LoadContext, ProjectTrust, parse_model, resolve_credential};
use synthesis_core::{AgentSession, BudgetUsage, SessionStatus, WorkspaceContext};
use synthesis_events::{ArtifactRef, Event, SessionEnd, TaskStatus};
use synthesis_harness::{
    Approval, ApprovalRequest, Approver, BoxFut, EventSink, Harness, HarnessConfig, Pricing, Scope,
    TaskOutcome, TaskRequest, ToolBox, budget_from_settings, from_mode, sanitize_for_display,
};
use synthesis_hooks::{HookEvent, Hooks};
use synthesis_mcp::McpManager;
use synthesis_permissions::{Mode, PermissionRules, Permissions};
use synthesis_providers::{
    Content, Message, ModelProvider, NetSettings, ProviderSpec, RetryPolicy, Role,
};
use synthesis_sandbox::{NetworkPolicy, SandboxBackend, SandboxPolicy, SandboxedCommand};
use synthesis_session::{NewSession, Retention, Store as SessionStore, StoreOptions};
use synthesis_settings::load_settings_with_file;
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
    #[value(alias = "manual")]
    Default,
    #[value(name = "acceptEdits")]
    AcceptEdits,
    Plan,
    #[value(name = "dontAsk")]
    DontAsk,
    /// Edits inside the workspace are allowed (as in acceptEdits); a judge model call reviews the other open calls (Bash, MCP, edits outside the workspace); ask, unclear answer, error or timeout is denied.
    Auto,
    /// Skips approval prompts. Still stops: deny rules, ask rules, critical rm, destructive MCP asks.
    /// Does NOT stop plain writes to protected paths (.git, .synt, .ssh ...), as in Claude Code. CLI only.
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
pub fn sandbox_preflight(
    backend: &dyn SandboxBackend,
    policy: Option<&synthesis_terminal::SandboxPolicy>,
) -> Result<(), String> {
    let hint = "run `synt doctor` to see the sandbox backends, or pass --sandbox best-effort|off to run unconfined";
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

/// Flags of `synt run`.
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
    /// Trust `<cwd>/.synt/config.json`, `.synt/settings.json`, agents and (with approval) MCP servers.
    #[arg(long)]
    pub trust_project: bool,
    /// Start this project-scope MCP server (needs --trust-project; repeatable).
    #[arg(long, value_name = "NAME")]
    pub approve_mcp: Vec<String>,
    #[arg(long)]
    pub no_session_store: bool,
    /// Run without long-term memory: nothing is retrieved, injected or saved.
    #[arg(long)]
    pub no_memory: bool,
    /// Allow a tool call without asking, e.g. `--allow 'Bash(cargo test:*)'` (settings rule
    /// syntax; repeatable). Typed by you, so trusted; it cannot lift critical-path or
    /// protected-path protections.
    #[arg(long, alias = "allowedTools", value_name = "RULE")]
    pub allow: Vec<String>,
    /// Deny a tool call, e.g. `--deny 'Bash(git push:*)'` (settings rule syntax; repeatable).
    /// Deny wins over every allow.
    #[arg(long, alias = "disallowedTools", value_name = "RULE")]
    pub deny: Vec<String>,
    /// Work in an isolated git worktree on branch `synthesis/<session>`; left in place for review.
    #[arg(long)]
    pub worktree: bool,
    /// With --worktree: remove the worktree at the end when nothing changed.
    #[arg(long, requires = "worktree")]
    pub cleanup_worktree: bool,
    /// Let other live sessions of this user message this one, and this one message them (turns
    /// `synt.peers.enabled` on for this run; the allow list still applies).
    #[arg(long)]
    pub peers: bool,
    /// Session name other sessions can address (`sess:<name>`): `[a-z0-9_-]`, at most 32; unique
    /// among live sessions of the workspace. Only used with peers on.
    #[arg(long, value_name = "NAME")]
    pub name: Option<String>,
    /// What this session is for, shown to peers in `ListSessions` (<= 120 chars). Default: the
    /// first line of AGENTS.md. Only used with peers on.
    #[arg(long, value_name = "TEXT")]
    pub about: Option<String>,
    /// Continue a stored session of this workspace by id (or unique id prefix) with the new prompt.
    #[arg(long, value_name = "ID", conflicts_with = "continue_session")]
    pub resume: Option<String>,
    /// With --resume: also accept a session of another workspace (default: only this one).
    #[arg(long, requires = "resume")]
    pub resume_any_workspace: bool,
    /// Continue the most recent session of this workspace.
    #[arg(long = "continue", short = 'c')]
    pub continue_session: bool,
    /// With --resume/--continue: start a new session seeded with the stored conversation.
    #[arg(long)]
    pub fork_session: bool,
    /// Replace the default system rules (the untrusted-data rule stays).
    #[arg(long, value_name = "TEXT", conflicts_with = "system_prompt_file")]
    pub system_prompt: Option<String>,
    #[arg(long, value_name = "FILE")]
    pub system_prompt_file: Option<PathBuf>,
    /// Add text after the system rules.
    #[arg(
        long,
        value_name = "TEXT",
        conflicts_with = "append_system_prompt_file"
    )]
    pub append_system_prompt: Option<String>,
    #[arg(long, value_name = "FILE")]
    pub append_system_prompt_file: Option<PathBuf>,
    /// Extra working directory the agent may access (repeatable).
    #[arg(long, value_name = "DIR")]
    pub add_dir: Vec<PathBuf>,
    /// Extra settings.json layered above local settings (managed settings still win).
    #[arg(long, value_name = "FILE")]
    pub settings: Option<PathBuf>,
    /// Fail (exit 2) unless this MCP server starts (repeatable).
    #[arg(long, value_name = "NAME")]
    pub require_mcp: Vec<String>,
    /// Minimal run: no hooks, no instruction files, no long-term memory.
    #[arg(long)]
    pub bare: bool,
}

/// `--output-format`: what `synt run` prints on stdout.
#[derive(Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum OutputFormat {
    /// The final assistant message only.
    Text,
    /// One JSON object (result, session id, usage, status, exit code).
    Json,
    /// The JSONL event stream (default; docs/specs/events.md).
    StreamJson,
}

#[derive(Args)]
pub struct RunArgs {
    /// Workspace directory (default: the current directory).
    #[arg(long)]
    cwd: Option<PathBuf>,
    #[arg(long, short = 'p', conflicts_with = "prompt_file")]
    prompt: Option<String>,
    #[arg(long)]
    prompt_file: Option<PathBuf>,
    /// An MCP prompt to run as the task: `"/mcp__<server>__<prompt> arg1 arg2"` (`synt mcp prompts`
    /// lists them). The same form works as the first token of any prompt.
    #[arg(long, value_name = "COMMAND", conflicts_with_all = ["prompt", "prompt_file"])]
    prompt_command: Option<String>,
    #[arg(long)]
    max_turns: Option<u32>,
    #[arg(long)]
    max_tool_calls: Option<u32>,
    /// Needs a price for the model: `synt.pricing` in the user settings.json (config.md §6).
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
    /// What stdout carries: text (final message), json (one result object) or stream-json (events).
    #[arg(long, value_enum, default_value = "stream-json")]
    output_format: OutputFormat,
    /// JSON Schema (inline JSON or a file) the final answer must satisfy; exit 1 when it does not.
    #[arg(long, value_name = "SCHEMA")]
    json_schema: Option<String>,
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
        /// False for `--output-format text|json`: events go to the store and `--events-out` only.
        stdout_events: bool,
        /// Set (and the run cancelled) when stdout is closed by the reader.
        closed: Arc<std::sync::atomic::AtomicBool>,
    },
    /// Events go to the interactive app; the person approves.
    Tui,
    /// A native front end (the desktop window): the host supplies the event writer, approver and
    /// input provider instead of the TUI's `UiLink`. Behaves like `Tui` otherwise.
    Native(Native),
}

/// The host side of [`Front::Native`].
pub struct Native {
    pub out: Box<dyn std::io::Write + Send>,
    pub approver: Arc<dyn Approver>,
    pub input: Arc<dyn synthesis_harness::InputProvider>,
}

/// Everything a front end needs, built once by [`prepare`].
pub struct Prepared {
    pub harness: Arc<Harness>,
    /// The context the harness uses (interactive sessions refresh its memory block per task).
    pub context: Arc<SynthesisContext>,
    /// Skills the Skill tool serves (project skills only in a trusted project).
    pub skills: Vec<synthesis_tools::SkillInfo>,
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
    /// Required checks from `synt.verification.checks`.
    pub criteria: Vec<Criterion>,
    pub artifact_root: PathBuf,
    pub label: String,
    /// Entries of the interactive `/model` picker.
    pub models: Vec<String>,
    /// `effort_tiers` the model declares (enables `/effort`).
    pub effort_tiers: Vec<String>,
    /// The model's window as known now (config, cache, built-in table); `None` = unknown.
    pub context_window: Option<u64>,
    pub sandbox: synthesis_tui::SandboxStatus,
    /// The interactive app's side of the event/approval/input channels (`Front::Tui` only).
    pub link: Option<synthesis_tui::UiLink>,
    pub homes: crate::Homes,
    /// Open long-term memory store (None with --no-memory, a failure, or `synt.memory.enabled: false`).
    pub memory: Option<Arc<crate::memcmd::MemoryRun>>,
    /// `#` notes and `/memory` (interactive app).
    pub notes: crate::memory::Notes,
    /// Stored conversation of `--resume/--continue/--fork-session` (the next task's history).
    pub history: Vec<Message>,
    /// Set when the stored session itself continues: the last event seq it had.
    pub resumed_from_seq: Option<u64>,
    /// Set with `resumed_from_seq` when the session continues on another model than the stored one.
    pub model_changed: Option<String>,
}

/// The stored session `--resume ID` / `--continue` names and its live (non-compacted) messages.
fn resolve_resume(
    c: &Common,
    h: &crate::Homes,
    cwd: &Path,
) -> Result<Option<(String, Vec<Message>)>, String> {
    if c.resume.is_none() && !c.continue_session {
        if c.fork_session {
            return Err("--fork-session needs --resume ID or --continue".into());
        }
        return Ok(None);
    }
    let r = synthesis_session::Reader::open(&h.synthesis).map_err(|e| e.to_string())?;
    let id = match &c.resume {
        Some(p) if !p.trim().is_empty() => {
            // another workspace's conversation would enter this one's context: opt-in only
            let here = cwd.to_string_lossy();
            let all = r
                .list_sessions((!c.resume_any_workspace).then_some(&*here))
                .map_err(|e| e.to_string())?;
            let hits: Vec<_> = all
                .iter()
                .filter(|s| s.id.starts_with(p.as_str()))
                .collect();
            match hits.as_slice() {
                [one] => one.id.clone(),
                [] => {
                    return Err(format!(
                        "--resume: no saved session of this directory matches \"{p}\" (--resume-any-workspace for another workspace)"
                    ));
                }
                many => many
                    .iter()
                    .find(|s| s.id == *p)
                    .map(|s| s.id.clone())
                    .ok_or(format!(
                        "--resume: \"{p}\" matches {} sessions; use a longer id",
                        many.len()
                    ))?,
            }
        }
        Some(_) => return Err("--resume: empty session id".into()),
        None => r
            .list_sessions(Some(&cwd.to_string_lossy()))
            .map_err(|e| e.to_string())?
            .into_iter()
            .find(|s| s.parent.is_none())
            .map(|s| s.id)
            .ok_or("--continue: no saved session for this directory")?,
    };
    let mut history = Vec::new();
    for m in r.read_messages(&id).map_err(|e| e.to_string())? {
        let role = match m.role.as_str() {
            "user" => Role::User,
            "assistant" => Role::Assistant,
            _ => continue,
        };
        if m.compacted {
            continue;
        }
        let content: Content = serde_json::from_value(m.content)
            .map_err(|e| format!("message {} of {id}: {e}", m.idx))?;
        history.push(Message { role, content });
    }
    Ok(Some((id, history)))
}

/// Rows of `{"synt":{"pricing":{"<provider/model>"|"<model>": {input_per_mtok, output_per_mtok}}}}`
/// from the USER settings file only (a repository must not be able to price a model at zero).
pub(crate) fn pricing(user_settings: &Path, label: &str, model: &str) -> Option<Pricing> {
    let raw = std::fs::read(user_settings)
        .ok()
        .filter(|b| b.len() <= 1 << 20)?;
    let v: Value = serde_json::from_slice(&raw).ok()?;
    parse_pricing(&v, label, model)
}

fn parse_pricing(v: &Value, label: &str, model: &str) -> Option<Pricing> {
    let table = v.get("synt")?.get("pricing")?;
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
            let well_formed = synthesis_permissions::Permissions::is_well_formed_rule(r);
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
    fn approve<'a>(
        &'a self,
        req: ApprovalRequest,
        cancel: &'a CancellationToken,
    ) -> BoxFut<'a, Approval> {
        Box::pin(async move {
            // A person at the terminal can approve a login hand-off (it needs them anyway).
            // Nothing else is ever approved here, and no terminal means no.
            if req.tool == "browser_login" {
                let q = format!(
                    "Open a visible browser at {} so you can log in?",
                    sanitize_for_display(req.input["origin"].as_str().unwrap_or("?"))
                );
                if crate::login::ask_yes(&q, cancel).await {
                    return Approval::allow();
                }
            }
            Approval {
                reviewer: "policy:non-interactive",
                ..Approval::auto_denied()
            }
        })
    }
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

/// `@file` / `@dir/` / `@"name with spaces"` mentions in a prompt become fenced data blocks after
/// it (workspace containment, caps and secret-file rules are `expand_mentions`', read-deny rules
/// are the session's). Returns the request and one notice per mention that named nothing.
pub fn expand_file_mentions(
    harness: &Harness,
    ws: &WorkspaceContext,
    request: String,
) -> (String, Vec<String>) {
    let perms = harness.permissions();
    let deny = |p: &Path| {
        let g = perms.lock().unwrap_or_else(|e| e.into_inner());
        read_denied(&g, p)
    };
    let opts = synthesis_context::MentionOptions {
        deny: Some(&deny),
        ..Default::default()
    };
    let m = synthesis_context::expand_mentions(&request, ws, &opts);
    let notes = m
        .missing
        .iter()
        .map(|x| format!("@{}: no such file", sanitize_for_display(x)))
        .collect();
    (request + &m.text, notes)
}

/// Default `--max-wall-secs` of a headless `synt run` (one hour). Turns (50) and tool calls (200)
/// are bounded by default already; a cost cap needs a configured price, so it has no default.
const HEADLESS_WALL_SECS: u64 = 3600;

/// The repository root (nearest trusted ancestor with `.git` or `.synt`, see
/// `synthesis_settings::find_project_root`), else the launch dir. The home directory and anything above
/// it never count: `~/.synt` or a stray `/Users/.git` would otherwise make `$HOME` the workspace
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
) -> Result<Prepared, String> {
    let (cli_allow, cli_deny) = (
        rules_arg(&c.allow, "--allow")?,
        rules_arg(&c.deny, "--deny")?,
    );
    let is_tui = matches!(front, Front::Tui | Front::Native(_));
    let h = homes(env)?;
    let mut synthesis_deny = synthesis_sandbox::synthesis_state_paths(&h.synthesis);
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
    let settings = load_settings_with_file(&lc, trust, c.settings.as_deref());
    // A settings file we cannot read would silently drop its deny rules: stop before anything runs
    // (`synt run` exits 2).
    if !settings.errors.is_empty() {
        return Err(format!(
            "settings error: {}\nsynt: fix or remove the file; nothing was started because its deny/ask rules would not apply",
            sanitize_for_display(&settings.errors.join("; "))
        ));
    }
    let user_settings = if c.trust_project {
        load_settings_with_file(&lc, ProjectTrust::Untrusted, c.settings.as_deref())
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
        eprintln!("synt: warning: {}", sanitize_for_display(w));
    }
    if !settings.env_refused.is_empty() {
        eprintln!(
            "synt: warning: settings env: refused secret-shaped name(s): {}",
            sanitize_for_display(&settings.env_refused.join(", "))
        );
    }
    let resume = resolve_resume(c, &h, cwd)?;
    if resume.is_some() && (c.worktree || c.no_session_store) {
        return Err(
            "--resume/--continue cannot be combined with --worktree or --no-session-store".into(),
        );
    }
    // a resumed session continues on the model it last used (`/model` is stored) unless
    // --model says otherwise or --provider names another provider; a stored model that cannot be
    // built falls back to the default
    let stored = resume
        .as_ref()
        .filter(|_| c.model.is_none())
        .and_then(|(id, _)| {
            synthesis_session::Reader::open(&h.synthesis)
                .ok()?
                .get_session(id)
                .ok()
        })
        .map(|r| r.model_config_id)
        .filter(|m| {
            c.provider
                .as_deref()
                .is_none_or(|p| m.starts_with(&format!("{}/", p.split(':').next().unwrap_or(p))))
        });
    let default = resolve_provider(
        c.provider.as_deref(),
        c.model.as_deref(),
        &cfg,
        &h.synthesis,
        env,
    );
    // a stored model that cannot be used is kept as it is (warned about, not overwritten)
    let mut stored_unbuildable = false;
    let mut model_warning = None;
    let res = match (default, stored) {
        (Ok(r), Some(m)) if r.label != m => {
            let stored_res = match stored_model_gone(&cfg, &m) {
                Some(why) => Err(why),
                None => resolve_provider(c.provider.as_deref(), Some(&m), &cfg, &h.synthesis, env),
            };
            match stored_res {
                Ok(s) => s,
                Err(e) => {
                    let w = format!(
                        "stored model {} is not available ({}); using {}",
                        sanitize_for_display(&m),
                        sanitize_for_display(&e),
                        sanitize_for_display(&r.label)
                    )
                    // one line: a label with a newline must not forge a second `synt: warning:`
                    .replace(['\n', '\t'], " ");
                    eprintln!("synt: warning: {w}");
                    model_warning = Some(w);
                    stored_unbuildable = true;
                    r
                }
            }
        }
        (Ok(r), _) => r,
        // nothing configured by default: the stored model may still be buildable
        (Err(e), Some(m)) => {
            resolve_provider(c.provider.as_deref(), Some(&m), &cfg, &h.synthesis, env)
                .map_err(|_| e)?
        }
        (Err(e), None) => return Err(e),
    };

    // `synt.browser` comes from the user's own settings file only (the loader enforces it)
    let browser = crate::login::parse_settings(&settings.synthesis.browser);
    for w in &browser.warnings {
        eprintln!("synt: warning: {}", sanitize_for_display(w));
    }
    // exact values of the stored browser secrets are masked in events, transcript and database
    let mut res = res;
    res.secrets.extend(browser.values(env));
    // files that hold stored secrets are state too: hidden from commands, Read/Grep/Glob and writes
    synthesis_deny.extend(browser.secret_files());

    let price = pricing(&h.synthesis.join("settings.json"), &res.label, &res.model);
    if limits.max_cost_usd.is_some() && price.is_none() {
        return Err(format!(
            "--max-cost-usd needs a price for {}: add synt.pricing[\"{}\"] = {{\"input_per_mtok\": N, \"output_per_mtok\": N}} to {} (see docs/specs/config.md §6)",
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
            // never reaches it); the flag is for `synt run`.
            if is_tui {
                return Err(
                    "--permission-mode bypassPermissions is only for `synt run`; in the TUI press Shift+Tab until the bypass dialog opens"
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
    let mut budget = budget_from_settings(&settings);
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
    // an unattended run must not run (and spend) forever
    // (the interactive app has a person watching)
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
                eprintln!("synt: warning: ignored additional directory (not an existing absolute directory)");
                None
            }
        })
        .collect();

    for d in &c.add_dir {
        match std::fs::canonicalize(d) {
            Ok(p) if p.is_dir() => extra_dirs.push(p),
            _ => {
                return Err(format!(
                    "--add-dir {}: not an existing directory",
                    d.display()
                ));
            }
        }
    }
    let in_place = resume.is_some() && !c.fork_session;
    let sid = match &resume {
        Some((id, _)) if in_place => id.clone(),
        _ => uuid::Uuid::new_v4().to_string(),
    };
    let artifact_root = h.synthesis.join("sessions").join(&sid);
    let (ws, worktree) = if c.worktree {
        if !extra_dirs.is_empty() {
            eprintln!("synt: warning: additionalDirectories are ignored with --worktree");
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
    let mut session = AgentSession {
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
            "synt: warning: cannot build the sandbox policy ({e}); commands run unconfined"
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
        Mode::Auto => notices.push(format!(
            "auto mode: a model call judges each call the rules leave open (allow / deny / ask); an unclear answer is an ask, which {}",
            if is_tui { "you answer in a dialog" } else { "this run denies" }
        )),
        _ => {}
    }
    for n in &notices {
        eprintln!("synt: WARNING: {n}");
    }
    notices.extend(model_warning); // already printed above

    let mut resumed_from_seq = None;
    let mut model_changed = None;
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
            .and_then(|s| {
                if in_place {
                    return s.resume_session(&sid, true);
                }
                s.create_session(NewSession {
                    id: Some(sid.clone()),
                    workspace: &ws,
                    model_config_id: res.label.clone(),
                    permission_mode: session.permission_mode,
                    permission_policy_id: "default".into(),
                    budget: budget.clone(),
                    parent: None,
                    title: None,
                })
            })
            .map_err(|e| {
                format!("session store: {e} (use --no-session-store to run without it)")
            })?;
        let mut w = w;
        if in_place {
            // terminal -> idle, interrupted -> recovering: the states the harness continues from
            let to = match w.get_session(&sid).map_err(|e| e.to_string())?.status {
                SessionStatus::Interrupted => Some(SessionStatus::Recovering),
                SessionStatus::Idle => None,
                // opened but never prompted (a /model switch before the first task)
                SessionStatus::Preparing => Some(SessionStatus::Idle),
                s if s.is_task_terminal() => Some(SessionStatus::Idle),
                s => return Err(format!("session {sid} is {s:?} and cannot be resumed")),
            };
            if let Some(to) = to {
                w.update_status(to, Some("resumed"))
                    .map_err(|e| e.to_string())?;
                session.status = to;
            } else {
                session.status = SessionStatus::Idle;
            }
            let rec = w.get_session(&sid).map_err(|e| e.to_string())?;
            resumed_from_seq = Some(rec.event_cursor);
            if rec.model_config_id != res.label && !stored_unbuildable {
                w.set_model(&res.label).map_err(|e| e.to_string())?;
                model_changed = Some(res.label.clone());
            }
        }
        Some(Arc::new(Mutex::new(w)))
    };
    let transcript = store.clone().map(|s| Arc::new(Transcript::new(s)));
    if let (true, Some(t), Some((_, hist))) = (in_place, &transcript, &resume) {
        t.skip(hist.len());
    }
    let derived = match (&store, &transcript) {
        (Some(_), Some(t)) => match TaskDb::open(&h.synthesis, &sid, res.secrets.clone()) {
            Ok(db) => Some((db, t.clone())),
            Err(e) => {
                eprintln!("synt: warning: task rows disabled: {e}");
                None
            }
        },
        _ => None,
    };
    let cancel = CancellationToken::new();
    let mut native_input = None;
    let (out, tee, prompt, link, approver, closed): (
        Box<dyn std::io::Write + Send>,
        _,
        String,
        _,
        Arc<dyn Approver>,
        _,
    ) = match front {
        Front::Native(n) => {
            native_input = Some(n.input);
            (n.out, None, String::new(), None, n.approver, None)
        }
        Front::Headless {
            prompt,
            tee,
            stdout_events,
            closed,
        } => (
            if stdout_events {
                Box::new(std::io::stdout())
            } else {
                Box::new(std::io::sink())
            },
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
    let taint = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let sink = EventSink::new(
        Persist::new(
            out,
            tee,
            store.clone(),
            derived,
            resumed_from_seq.unwrap_or(0),
            closed,
        )
        .with_taint(taint.clone()),
        sid.clone(),
        res.secrets.clone(),
    );
    if let Some(t) = &transcript {
        let sink = sink.clone();
        t.set_notifier(Arc::new(move |m| {
            sink.emit(
                &Scope::default(),
                Event::Notice {
                    level: "warning".into(),
                    text: m.to_string(),
                },
            );
        }));
    }

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
            .deny_reads(state_paths.clone())
            .protect(state_paths),
    );
    // Saved logins or {file:} secrets exist: the main agent gets the stricter Bash scan and the
    // secret-file read ban the peer responder has (`$`, globs and `~user` words become a hard ask).
    perms.env.deny_secret_reads = browser.holds_credentials(&h.synthesis);
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
        eprintln!("synt: warning: {d}");
    }
    hcfg.auto_compact = auto_compact;
    let (shell, diag) = crate::ctx::read_shell_settings(&h.synthesis.join("settings.json"));
    for d in &diag {
        eprintln!("synt: warning: {d}");
    }
    hcfg.shell = shell;
    let (tool_search, diag) = crate::ctx::read_tool_search(&h.synthesis.join("settings.json"));
    for d in &diag {
        eprintln!("synt: warning: {d}");
    }
    hcfg.tool_search = tool_search;
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
    let user_files = [h.synthesis.join("AGENTS.md")];
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
        eprintln!("synt: warning: {}", sanitize_for_display(&w));
        notices.push(w);
    }
    // lowest priority: before the user and repository files
    instructions.splice(
        0..0,
        crate::ctx::load_auto_memory(&h.synthesis, &[&ws.root, &ws.cwd]),
    );
    instructions.extend(crate::ctx::load_rules(&ws.root));
    if c.bare {
        instructions.clear();
    }
    let repo = crate::memcmd::repo_of(&ws.root);
    let small = crate::memcmd::small_model(&cfg, &res.label, &res.model);
    let (memory, memory_block) = crate::memcmd::start(
        &crate::memcmd::StartArgs {
            enabled: !c.no_memory && !c.bare,
            home: &h,
            repo: &repo,
            prompt: &prompt,
            sid: &sid,
            project_trusted: trust == ProjectTrust::Trusted,
            secrets: &res.secrets,
            provider: res.provider.clone(),
            model: small.clone(),
            sink: Some(sink.clone()),
            pricing: pricing(&h.synthesis.join("settings.json"), &small, &small),
            taint,
        },
        cancel.clone(),
    );
    let notes = crate::memory::Notes {
        user: user_files[0].clone(),
        project: crate::memory::project_file(&ws.root),
        root: ws.root.clone(),
        secrets: res.secrets.clone(),
        loaded: crate::memory::describe(&instructions),
        long_term: memory.is_some(),
    };
    if let Some(m) = &memory {
        // MemoryUpdate/MemoryForget on a note the agent did not save ask the person
        perms.env.memory_notes = Some(synthesis_permissions::NoteOracle(
            crate::memtools::agent_note_oracle(m.clone()),
        ));
    }
    // after memory starts: learned skills are installed by `start`
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
    .with_skills(&skills)
    .with_memory(memory_block)
    .with_system_prompt(
        text_or_file(
            &c.system_prompt,
            &c.system_prompt_file,
            "--system-prompt-file",
        )?,
        text_or_file(
            &c.append_system_prompt,
            &c.append_system_prompt_file,
            "--append-system-prompt-file",
        )?,
    );
    if let Some(t) = &transcript {
        context = context.with_transcript(t.clone());
    }
    let agents = crate::agents::load(&h.synthesis, &settings.project_root, trust);
    if let Some(m) = &memory {
        let (pinned, per) = crate::memcmd::sub_agent_blocks(
            m,
            agents
                .table
                .iter()
                .map(|d| (d.name.as_str(), d.prompt.as_str())),
        );
        context = context
            .with_sub_memory(pinned, per)
            .with_compact_hook(crate::memcmd::compaction_hook(m.clone()));
    }
    if !agents.skipped.is_empty() {
        let w = format!(
            "{} project agent definition(s) not loaded (project not trusted)",
            agents.skipped.len()
        );
        eprintln!("synt: warning: {w}");
        notices.push(w);
    }

    // Everything fallible is done: start MCP servers last so an early error never leaves one.
    let tokio_rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| e.to_string())?;
    // `synt.modelInfo.fetch` is the user's own setting: a repository cannot switch it
    if user_settings.synthesis.model_info_fetch != Some(false) {
        start_model_info(
            tokio_rt.handle(),
            &cfg,
            &user_cfg,
            &h.synthesis,
            env,
            (
                res.label.split('/').next().unwrap_or_default(),
                &res.provider,
            ),
        );
    }
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
            .then(|| settings.project_root.join(".synt").join("settings.json"))
            .as_deref(),
    );
    mplan.artifact_dir = Some(artifact_root.join("mcp"));
    // Printed now and kept in `notices`, which become `notice` events.
    for n in &mplan.notices {
        let n = sanitize_for_display(n);
        eprintln!("synt: {n}");
        notices.push(n);
    }
    for (n, why) in &mplan.skipped {
        let w = format!(
            "MCP server \"{}\" not started: {}",
            sanitize_for_display(n),
            sanitize_for_display(why)
        );
        eprintln!("synt: warning: {w}");
        notices.push(w);
    }
    let (mcp, mcp_attach) = if mplan.servers.is_empty() {
        (None, None)
    } else {
        let (m, t, warns) = tokio_rt.block_on(crate::mcp::start(&mplan, env, &ws.root));
        for w in &warns {
            let w = sanitize_for_display(w);
            eprintln!("synt: warning: {w}");
            notices.push(w); // also a `notice` event, so a script sees the failure
        }
        // `--require-mcp NAME`: a server that did not come up ends the run before the model starts
        for n in &c.require_mcp {
            let quoted = format!("\"{}\"", sanitize_for_display(n));
            if !mplan.servers.iter().any(|s| &s.name == n)
                || warns.iter().any(|w| w.contains(&quoted))
            {
                tokio_rt.block_on(m.close());
                return Err(format!("--require-mcp: MCP server {quoted} is not running"));
            }
        }
        (Some(m), Some(t))
    };
    if let Some(n) = c
        .require_mcp
        .iter()
        .find(|n| mcp.is_none() || !mplan.servers.iter().any(|s| &s.name == *n))
    {
        return Err(format!(
            "--require-mcp: MCP server \"{}\" is not configured or was not started",
            sanitize_for_display(n)
        ));
    }

    // interactive: a Bash call that times out moves to the background; headless kills it (D6)
    let stdio = crate::login::stdio_servers(&mplan.servers);
    for w in crate::login::exposure_warnings(
        &h.synthesis,
        &browser,
        &stdio,
        sandbox_mode == SandboxMode::Enforce,
    ) {
        let w = sanitize_for_display(&w);
        if is_tui {
            notices.push(w); // a startup note and a `notice` event; stderr would garble the screen
        } else {
            eprintln!("synt: warning: {w}");
        }
    }
    // The interactive app asks in its own dialogs; headless asks on the terminal.
    let browser_host: Arc<dyn synthesis_tools::BrowserHost> = if is_tui {
        Arc::new(crate::login::DialogHost {
            approver: approver.clone(),
            sink: sink.clone(),
        })
    } else {
        Arc::new(crate::login::CliHost { sink: sink.clone() })
    };
    let mut toolbox = ToolBox::new(
        Registry::with_terminal_policy(rt.clone(), make_policy)
            .with_new_tools()
            .with_browser(browser.config(
                synthesis_tools::BrowserConfig::default(),
                &h.synthesis,
                env,
                browser_host,
                sandbox_mode == SandboxMode::Enforce,
                stdio,
            ))
            .with_background_on_timeout(is_tui),
    );
    if let Some(a) = mcp_attach {
        toolbox = a.apply(toolbox);
    }
    // the four Memory* tools exist only while the run has an open store
    if let Some(m) = &memory {
        for t in crate::memtools::tools(m.clone()) {
            toolbox = toolbox.with_tool(t);
        }
    }
    let gate_cfg = vs.gate(budget.max_verify_attempts);
    let rt_gate = rt.clone();
    // Peer endpoint (off by default). Bound here, inside the runtime; the server starts below.
    let peers = {
        let _enter = tokio_rt.enter();
        crate::peers::setup(
            c.peers,
            c.name.as_deref(),
            crate::peers::about_of(c.about.as_deref(), &session.workspace.root, &|t| {
                sink.redact(t)
            }),
            &user_settings.synthesis.peers,
            &h.synthesis,
            env,
            &session,
            &mut notices,
        )?
    };
    let (peer_link, peer_listener) = match peers {
        Some(s) => (Some(s.link), Some(s.listener)),
        None => (None, None),
    };
    let context = Arc::new(context);
    let mut harness = Harness::new(res.provider.clone(), toolbox, perms, sink.clone(), hcfg)
        .with_approver(approver)
        .with_agents(agents.table)
        .with_model_resolver(agent_model_resolver(&cfg, &res.label))
        .with_context(context.clone())
        .with_gate(Box::new(move |s| {
            CompletionGate::new(
                VerifyRunner::new(rt_gate.clone(), &s.id, Arc::new(s.workspace.clone())),
                gate_cfg.clone(),
            )
        }));
    if let Some(l) = &link {
        harness = harness.with_input(l.input());
    }
    if let Some(i) = native_input {
        harness = harness.with_input(i);
    }
    if let Some(l) = peer_link {
        harness = harness.with_peers(l);
    }
    // "Allow always" persists to .synt/settings.local.json, but only in a trusted project: an
    // untrusted project's allow rules are ignored on load, so writing them would be misleading.
    if trust == ProjectTrust::Trusted {
        let root = settings.project_root.clone();
        harness = harness.with_rule_store(Arc::new(move |rules| {
            if let Err(e) = synthesis_settings::persist_allow_rules(&root, rules) {
                eprintln!(
                    "synt: warning: could not save the rule: {}",
                    sanitize_for_display(&e.to_string())
                );
            }
        }));
    }
    if !c.bare && !settings.disable_all_hooks && !settings.hooks.is_empty() {
        harness = harness.with_hooks(Arc::new(Hooks::new(
            rt.clone(),
            sid.clone(),
            ws.clone(),
            settings.hooks.clone(),
        )));
    }

    eprintln!(
        "synt: session {sid} model {} cwd {} mode {}",
        res.label,
        ws.cwd.display(),
        mode.as_str()
    );
    if is_tui {
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
    let caps = res.provider.capabilities(&res.model);
    let effort_tiers = caps.effort_tiers;
    let context_window = caps.context_window;
    let harness = Arc::new(harness);
    if let (Some(l), Some(server)) = (peer_listener, harness.peer_server()) {
        if let Some(w) = harness.peer_mailbox_worker(server.clone()) {
            tokio_rt.spawn(w);
        }
        tokio_rt.spawn(server.run(l));
    }
    Ok(Prepared {
        harness,
        context,
        skills,
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
        context_window,
        sandbox: sandbox_status(sandbox_mode, sandbox_mode != SandboxMode::Enforce),
        link,
        homes: h,
        memory,
        notes,
        history: resume.map(|(_, hist)| hist).unwrap_or_default(),
        resumed_from_seq,
        model_changed,
    })
}

pub fn run(a: RunArgs, env: Env) -> Result<u8, String> {
    let prompt = read_prompt(&a)?;
    let schema = a.json_schema.as_deref().map(parse_schema).transpose()?;
    let mut common = a.common.clone();
    if let Some((raw, _)) = &schema {
        let note = format!(
            "Your final answer must be only a JSON value that conforms to this JSON Schema, with no other text:\n{raw}"
        );
        common.append_system_prompt = Some(match common.append_system_prompt.take() {
            Some(t) => format!("{t}\n\n{note}"),
            None => note,
        });
    }
    let format = a.output_format;
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
        &common,
        &cwd,
        &env,
        &limits,
        Front::Headless {
            prompt: prompt.clone(),
            tee,
            stdout_events: format == OutputFormat::StreamJson,
            closed: closed.clone(),
        },
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
        memory,
        history,
        resumed_from_seq,
        model_changed,
        ..
    } = p;
    // `@server:uri` mentions and `/mcp__server__prompt` commands; memory below sees what was typed
    let (request, more) =
        match tokio_rt.block_on(crate::mcpinput::expand(mcp.as_ref(), &prompt, &cancel)) {
            Ok(r) => r,
            Err(e) => {
                if let Some(m) = &mcp {
                    tokio_rt.block_on(m.close());
                }
                return Err(e);
            }
        };
    let mut notices = notices;
    let (request, files_missing) = expand_file_mentions(&harness, &session.workspace, request);
    let more: Vec<String> = more.into_iter().chain(files_missing).collect();
    for n in more {
        let n = sanitize_for_display(&n);
        eprintln!("synt: warning: {n}");
        notices.push(n);
    }
    let cleanup = a.common.cleanup_worktree;
    let code = tokio_rt.block_on(async {
        on_signals(cancel.clone()).map_err(|e| format!("signal handlers: {e}"))?;
        let (store2, sink2, h2) = (store.clone(), sink.clone(), harness.clone());
        let (memory2, request2) = (memory.clone(), prompt.clone());
        let task = tokio::spawn(async move {
            let mut session = session;
            let r = async {
                match resumed_from_seq {
                    Some(seq) => {
                        h2.resume_session(&session, seq);
                        if let Some(m) = &model_changed {
                            h2.model_changed(m);
                        }
                    }
                    None => h2.start_session(&mut session, "startup")?,
                }
                h2.session_hook(
                    HookEvent::SessionStart,
                    json!({"source": if resumed_from_seq.is_some() { "resume" } else { "startup" }}),
                    Some(if resumed_from_seq.is_some() { "resume" } else { "startup" }),
                )
                .await;
                if let Some(m) = &memory2 {
                    crate::memcmd::announce(m);
                }
                for n in notices {
                    h2.session_hook(
                        HookEvent::Notification,
                        json!({"message": n, "notification_type": "notice"}),
                        Some("notice"),
                    )
                    .await;
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
                            request,
                            images: vec![],
                            criteria,
                            history,
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
                h2.session_hook(
                    HookEvent::SessionEnd,
                    json!({"reason": out.reason}),
                    Some(&out.reason),
                )
                .await;
                // before the terminal session event, so the stream still ends with it
                if let Some(m) = &memory2 {
                    let verified = h2.verified_commands(&out.task_id);
                    crate::memcmd::task_ended(m, &request2, &out, &verified);
                }
                h2.end_session(&out);
                Ok::<_, synthesis_harness::HarnessError>(out)
            }
            .await;
            if let (Some(s), Ok(out)) = (&store2, &r) {
                finish_store(s, transcript.as_deref(), out, &session.usage);
            }
            // reflection (a model call, opt-in) runs after the result and the session end are final
            if let (Some(m), Ok(_)) = (&memory2, &r)
                && let Some(rf) = &m.reflect
            {
                crate::memcmd::reflect_jobs(&m.store, rf, 2).await;
            }
            r
        });
        let code = match task.await {
            Ok(Ok(out)) => {
                eprintln!("{}", task_line(out.status, &out.reason));
                if let Some(h) = blocked_hint(&out) {
                    eprintln!("{h}");
                }
                let mut code = exit_code(out.status);
                let structured = match schema.as_ref().map(|(_, v)| check_schema(v, &out.final_text)) {
                    Some(Err(e)) => {
                        eprintln!("synt: --json-schema: {e}");
                        code = code.max(1);
                        None
                    }
                    Some(Ok(v)) => Some(v),
                    None => None,
                };
                print_result(format, &out, &sid, code, structured);
                code
            }
            Ok(Err(e)) => {
                eprintln!("synt: internal error: {e}");
                70
            }
            Err(e) => {
                eprintln!("synt: internal error: {e}");
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

/// `--json-schema`: inline JSON or a file; returns the text shown to the model and the schema.
fn parse_schema(arg: &str) -> Result<(String, Value), String> {
    let raw = if arg.trim_start().starts_with('{') || arg.trim() == "true" {
        arg.to_string()
    } else {
        std::fs::read_to_string(arg).map_err(|e| format!("--json-schema {arg}: {e}"))?
    };
    let v: Value = serde_json::from_str(&raw).map_err(|e| format!("--json-schema: {e}"))?;
    jsonschema::validator_for(&v).map_err(|e| format!("--json-schema: {e}"))?;
    Ok((raw, v))
}

/// The final answer (optionally in a code fence) parsed and validated against `schema`.
fn check_schema(schema: &Value, text: &str) -> Result<Value, String> {
    let t = text.trim();
    let t = t
        .strip_prefix("```json")
        .or_else(|| t.strip_prefix("```"))
        .and_then(|r| r.strip_suffix("```"))
        .unwrap_or(t);
    let v: Value =
        serde_json::from_str(t.trim()).map_err(|e| format!("the answer is not JSON: {e}"))?;
    let validator = jsonschema::validator_for(schema).map_err(|e| e.to_string())?;
    let first = validator.iter_errors(&v).next().map(|e| e.to_string());
    match first {
        None => Ok(v),
        Some(e) => Err(format!("the answer does not match the schema: {e}")),
    }
}

/// stdout for `--output-format text|json` (stream-json already printed the events).
fn print_result(
    f: OutputFormat,
    out: &TaskOutcome,
    sid: &str,
    code: u8,
    structured: Option<Value>,
) {
    match f {
        OutputFormat::StreamJson => {}
        OutputFormat::Text => match structured {
            Some(v) => println!("{v}"),
            None => println!("{}", out.final_text),
        },
        OutputFormat::Json => {
            let mut r = json!({
                "type": "result",
                "status": out.status,
                "is_error": code != 0,
                "exit_code": code,
                "reason": out.reason,
                "result": out.final_text,
                "session_id": sid,
                "task_id": out.task_id,
                "usage": out.usage,
                "changed_files": out.changed_files,
            });
            if let Some(v) = structured {
                r["structured_output"] = v;
            }
            println!("{r}");
        }
    }
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
        eprintln!("synt: warning: session store: {e}");
    }
}

/// Largest prompt read from stdin (Claude Code: 10 MB).
const MAX_STDIN_PROMPT: u64 = 10 << 20;

fn read_prompt(a: &RunArgs) -> Result<String, String> {
    let p = match (&a.prompt, &a.prompt_file) {
        _ if a.prompt_command.is_some() => {
            let c = a.prompt_command.clone().unwrap_or_default();
            if !crate::mcpinput::is_command(&c) {
                return Err(
                    "--prompt-command: expected \"/mcp__<server>__<prompt> [args...]\"".into(),
                );
            }
            c
        }
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
                .take(MAX_STDIN_PROMPT + 1)
                .read_to_string(&mut s)
                .map_err(|e| format!("stdin: {e}"))?;
            if s.len() as u64 > MAX_STDIN_PROMPT {
                return Err(format!(
                    "the prompt on stdin is over the {} MB limit; use --prompt-file or shorten it",
                    MAX_STDIN_PROMPT >> 20
                ));
            }
            s
        }
    };
    if p.trim().is_empty() {
        return Err("empty prompt".into());
    }
    Ok(p)
}

fn text_or_file(
    t: &Option<String>,
    f: &Option<PathBuf>,
    flag: &str,
) -> Result<Option<String>, String> {
    match (t, f) {
        (Some(t), _) => Ok(Some(t.clone())),
        (_, Some(f)) => std::fs::read_to_string(f)
            .map(Some)
            .map_err(|e| format!("{flag} {}: {e}", f.display())),
        _ => Ok(None),
    }
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

/// Frontmatter `model` of a sub-agent definition: the harness has one provider per run, so only a
/// model of that provider is accepted (alias, `provider/model` or a bare id); anything else is
/// refused with the reason, and the agent runs on the session's model.
fn agent_model_resolver(cfg: &Config, label: &str) -> synthesis_harness::ModelResolver {
    let cfg = cfg.clone();
    let run = label.split_once('/').map_or("", |(p, _)| p).to_string();
    Arc::new(move |m| {
        let resolved = synthesis_config::resolve_alias(m, &cfg);
        if let Some((p, model)) = resolved.split_once('/') {
            return if p == run {
                Ok(model.to_string())
            } else {
                Err(format!(
                    "it belongs to provider \"{p}\" and this run uses \"{run}\" (one provider per run)"
                ))
            };
        }
        let lists = |id: &str| {
            cfg.providers
                .get(id)
                .and_then(|p| p.models.as_ref())
                .is_some_and(|l| l.contains(&resolved))
        };
        match cfg.providers.keys().find(|id| lists(id)) {
            Some(other) if *other != run => Err(format!(
                "it belongs to provider \"{other}\" and this run uses \"{run}\" (one provider per run)"
            )),
            _ => Ok(resolved),
        }
    })
}

/// Starts fetching model info (context window, max output, ...) in the background for the active
/// provider and for every other one that has declared models or `discover: true` and a credential
/// (`discover: false` is never contacted; each provider gets only its own key). The active one
/// updates `active` in place; the others come from the user's own config (`user_cfg`, never a
/// project's) and only rewrite their cache file (read at the next start or `/model` switch). A fresh cache, a provider without a
/// credential and any failure (offline, timeout) are silent; nothing here blocks the UI.
pub fn start_model_info(
    rt: &tokio::runtime::Handle,
    cfg: &Config,
    user_cfg: &Config,
    synthesis_home: &Path,
    env: &Env,
    active: (&str, &Arc<dyn ModelProvider>),
) {
    let net = NetSettings {
        connect_timeout: std::time::Duration::from_secs(3),
        header_timeout: std::time::Duration::from_secs(5),
        ..NetSettings::default()
    };
    let others = user_cfg.providers.iter().filter(|(id, _)| *id != active.0);
    for (id, p) in cfg
        .providers
        .get_key_value(active.0)
        .into_iter()
        .chain(others)
    {
        let wanted = p.discover == Some(true) || p.models.as_ref().is_some_and(|m| !m.is_empty());
        if p.discover == Some(false) || (id != active.0 && !wanted) {
            continue;
        }
        let provider = if id == active.0 {
            active.1.clone()
        } else {
            let cred = resolve_credential(id, Some(p), synthesis_home, env);
            let spec = ProviderSpec::from_config(id, p, cred.as_ref());
            let cache = synthesis_providers::modelinfo::cache_path(synthesis_home, id);
            match spec
                .and_then(|s| s.connect_cached(env, &net, RetryPolicy::default(), Some(cache)))
            {
                Ok(x) => x,
                Err(_) => continue, // no credential or unusable settings
            }
        };
        rt.spawn(async move {
            let cancel = CancellationToken::new();
            let _ = tokio::time::timeout(
                std::time::Duration::from_secs(5),
                provider.refresh_model_info(false, &cancel),
            )
            .await;
        });
    }
}

/// Why a stored `provider/model` can no longer be used although its provider is still configured:
/// the provider's model list no longer has it (a provider without a list is not checked; an
/// unknown provider is an error of `resolve_provider`).
fn stored_model_gone(cfg: &Config, label: &str) -> Option<String> {
    let (p, m) = label.split_once('/')?;
    cfg.providers
        .get(p)?
        .models
        .as_ref()
        .filter(|l| !l.is_empty() && !l.iter().any(|x| x == m))
        .map(|_| format!("not in the models of provider \"{p}\""))
}

pub struct Resolved {
    pub provider: Arc<dyn ModelProvider>,
    pub model: String,
    pub label: String,
    pub secrets: Vec<String>,
}

/// The error `synt run` exits 2 with when no model is configured.
pub const NO_MODEL: &str = "no model configured: pass --model provider/model or set SYNT_MODEL";

pub fn resolve_provider(
    provider_arg: Option<&str>,
    model_arg: Option<&str>,
    cfg: &Config,
    synthesis_home: &Path,
    env: &Env,
) -> Result<Resolved, String> {
    if let Some(script) = provider_arg.and_then(|p| p.strip_prefix("fake:")) {
        #[cfg(feature = "test-provider")]
        {
            let model = model_arg.map_or_else(|| "fake-model".to_string(), String::from);
            return Ok(Resolved {
                provider: crate::fake::load(Path::new(script))?,
                // `/model fake/other` passes the label as the model
                label: if model.starts_with("fake/") {
                    model.clone()
                } else {
                    format!("fake/{model}")
                },
                model,
                secrets: vec![],
            });
        }
        #[cfg(not(feature = "test-provider"))]
        return Err(format!(
            "--provider fake:{script} needs a build with the test-provider feature"
        ));
    }
    let spec = match (provider_arg, model_arg) {
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
        (_, m) => m.map(String::from),
    };
    // `--provider X` without a model: say what is missing about X (key first), not NO_MODEL
    let (id, parsed) = match parse_model(spec.as_deref(), cfg) {
        Ok(m) => (
            m.provider_id.clone().ok_or("no provider configured")?,
            Some(m),
        ),
        Err(_) => (provider_arg.ok_or(NO_MODEL)?.to_string(), None),
    };
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
                "no API key for provider \"{id}\": set SYNT_API_KEY (when SYNT_PROVIDER={id}) or add apiKey to its config"
            ),
        });
    }
    let parsed = parsed.ok_or_else(|| {
        format!("no model for provider \"{id}\": pass --model {id}/<model> or set SYNT_MODEL")
    })?;
    let provider = ProviderSpec::from_config(&id, p, cred.as_ref())
        .and_then(|s| {
            s.connect_cached(
                env,
                &NetSettings::default(),
                RetryPolicy::default(),
                Some(synthesis_providers::modelinfo::cache_path(
                    synthesis_home,
                    &id,
                )),
            )
        })
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

/// `synt: blocked: approval needed for ... - rerun with --allow ...`.
/// The stderr summary of a finished task; the machine-readable reason stays in the JSON result.
fn task_line(status: TaskStatus, reason: &str) -> String {
    let word = format!("{status:?}").to_lowercase();
    let why = match reason {
        "gate_not_applicable" => "no verification applied",
        "verified" => "verified",
        // the reason only repeats the status
        r if r.eq_ignore_ascii_case(&word) => "",
        r => &r.replace('_', " "),
    };
    if why.is_empty() {
        format!("synt: task {word}")
    } else {
        format!("synt: task {word} ({why})")
    }
}

fn blocked_hint(out: &TaskOutcome) -> Option<String> {
    if out.needed_permissions.is_empty() {
        return None;
    }
    // Blocked is the usual outcome; a model that worked around the denial still gets told.
    let lead = if out.status == TaskStatus::Blocked {
        "synt: blocked: approval needed for"
    } else {
        "synt: note: approval was needed but unavailable for"
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
        format!("rerun with {}", flags.join(" "))
    } else {
        "the rule text comes from the model's command, so review it and add it with --allow yourself".to_string()
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

    /// A server that counts the connections it gets and answers an empty model list.
    fn counting_server() -> (String, Arc<std::sync::atomic::AtomicUsize>) {
        use std::io::{Read, Write};
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", l.local_addr().unwrap());
        let n = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let n2 = n.clone();
        std::thread::spawn(move || {
            for mut s in l.incoming().flatten() {
                n2.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let mut b = [0u8; 2048];
                let _ = s.read(&mut b);
                let body = r#"{"data":[]}"#;
                let _ = write!(
                    s,
                    "HTTP/1.1 200 OK\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
            }
        });
        (url, n)
    }

    #[test]
    fn startup_fetch_skips_discover_false_and_providers_nobody_asked_for() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("h");
        std::fs::create_dir_all(&home).unwrap();
        let names = ["act", "listed", "nodisc", "bare"];
        let servers: Vec<_> = names.iter().map(|_| counting_server()).collect();
        let prov = |i: usize, extra: &str| {
            format!(
                r#""{}":{{"type":"anthropic","baseURL":"{}","apiKey":"{{env:SYNT_TEST_K}}"{extra}}}"#,
                names[i], servers[i].0
            )
        };
        let json = format!(
            r#"{{"$schema":"config.json/v1","providers":{{{},{},{},{}}}}}"#,
            prov(0, r#","models":["m"]"#),
            prov(1, r#","models":["m"]"#),
            prov(2, r#","models":["m"],"discover":false"#),
            prov(3, "")
        );
        std::fs::write(home.join("synt.json"), json).unwrap();
        let env = Env::from_pairs([
            ("HOME", dir.path().to_str().unwrap()),
            ("SYNT_HOME", home.to_str().unwrap()),
            ("SYNT_TEST_K", "sk-test-0123456789abcdef"),
        ]);
        let h = homes(&env).unwrap();
        let cfg = load_config(&env, &h, None, ProjectTrust::Untrusted).unwrap();
        let res = resolve_provider(None, Some("act/m"), &cfg, &h.synthesis, &env).unwrap();
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        start_model_info(
            rt.handle(),
            &cfg,
            &cfg,
            &h.synthesis,
            &env,
            ("act", &res.provider),
        );
        let hits = |i: usize| servers[i].1.load(std::sync::atomic::Ordering::SeqCst);
        for _ in 0..100 {
            if hits(0) == 1 && hits(1) == 1 {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        std::thread::sleep(std::time::Duration::from_millis(300));
        assert_eq!(
            (hits(0), hits(1), hits(2), hits(3)),
            (1, 1, 0, 0),
            "active and listed are asked; discover:false and unlisted are not"
        );
    }

    #[test]
    fn agent_models_resolve_only_within_the_runs_provider() {
        let mut cfg = Config::default();
        cfg.aliases
            .insert("fast".into(), Some("anthropic/haiku-x".into()));
        cfg.aliases
            .insert("alien".into(), Some("openai/gpt-x".into()));
        let r = agent_model_resolver(&cfg, "anthropic/sonnet-x");
        assert_eq!(r("fast").unwrap(), "haiku-x");
        assert_eq!(r("anthropic/opus-x").unwrap(), "opus-x");
        assert_eq!(r("some-id").unwrap(), "some-id");
        for bad in ["alien", "openai/gpt-y"] {
            let e = r(bad).unwrap_err();
            assert!(e.contains("one provider per run"), "{e}");
        }
    }

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
    fn the_task_line_is_a_sentence() {
        assert_eq!(
            task_line(TaskStatus::Completed, "gate_not_applicable"),
            "synt: task completed (no verification applied)"
        );
        assert_eq!(
            task_line(TaskStatus::Unverified, "unverified"),
            "synt: task unverified"
        );
        assert_eq!(
            task_line(TaskStatus::Blocked, "approval_unavailable"),
            "synt: task blocked (approval unavailable)"
        );
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
            e.contains("--network loopback") && e.contains("synt doctor"),
            "{e}"
        );
    }

    #[test]
    fn preflight_refuses_a_backend_that_cannot_confine() {
        let e = sandbox_preflight(&synthesis_sandbox::NullSandbox, None).unwrap_err();
        assert!(
            e.contains("synt doctor") && e.contains("no OS sandbox"),
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
        // `.synt` also marks a root
        let proj = base.join("proj");
        std::fs::create_dir_all(proj.join(".synt")).unwrap();
        std::fs::create_dir_all(proj.join("a")).unwrap();
        assert_eq!(workspace_root(&proj.join("a"), &base), proj);
        // no marker: the launch dir
        let lone = base.join("lone/x");
        std::fs::create_dir_all(&lone).unwrap();
        assert_eq!(workspace_root(&lone, &base), lone);
        // `$HOME/.synt` must not turn the home directory into the workspace
        std::fs::create_dir_all(base.join(".synt")).unwrap();
        assert_eq!(workspace_root(&lone, &base), lone);
        // ... nor may a marker ABOVE home (`/Users/.git`)
        std::fs::remove_dir(base.join(".synt")).unwrap();
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
        std::fs::write(sub.join("AGENTS.md"), "pkg rules").unwrap();
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
        std::fs::write(base.join("sub/AGENTS.md"), "see @../docs.md").unwrap();
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
        assert!(!read_denied(&perms, &base.join("sub/AGENTS.md")));
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
