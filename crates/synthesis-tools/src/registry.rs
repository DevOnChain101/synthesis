use std::path::Path;
use std::sync::Arc;

use serde_json::{Value, json};
use synthesis_core::ToolContext;

use crate::native::{self, Native, PolicyFactory, Shared};
use crate::{BoxFuture, SideEffectClass, Tool, ToolOutput, ToolRuntime, ToolSpec};
use crate::{BrowserConfig, DesktopConfig, WebConfig};
use crate::{edit, glob, grep, notebook, read, skill, write};
use synthesis_core::WorkspaceContext;
use synthesis_terminal::{NetworkPolicy, SandboxMode, SandboxPolicy, TerminalRuntime};

/// Wire name, description and input schema of the 20 ported tools in registry order
/// (17 built-ins in Node order, then the 3 harness tools).
const SCHEMAS: &str = include_str!("schemas.json");

/// The 4 new terminal tools (tools.md §5), registered after the ported ones by `with_new_tools`.
const TERMINAL_SCHEMAS: &str = include_str!("terminal_schemas.json");

/// browser_*, verify_run and artifact_read; their schemas say `additionalProperties:false` themselves.
const EXTRA_SCHEMAS: &str = include_str!("extra_schemas.json");

/// desktop_*: registered only by `with_desktop` (user setting `synt.desktop.enabled`).
const DESKTOP_SCHEMAS: &str = include_str!("desktop_schemas.json");

const STRICT_INPUT: [&str; 12] = [
    "Read",
    "Write",
    "Edit",
    "Glob",
    "Grep",
    "Bash",
    "BashOutput",
    "KillShell",
    "terminal_spawn",
    "terminal_logs",
    "terminal_signal",
    "terminal_status",
];

fn side_effect(name: &str) -> SideEffectClass {
    use SideEffectClass::*;
    match name {
        "Write" | "Edit" | "NotebookEdit" => Write,
        "Bash" | "KillShell" | "terminal_spawn" | "terminal_signal" => Execute,
        "WebFetch"
        | "WebSearch"
        | "browser_navigate"
        | "browser_click"
        | "browser_fill"
        | "browser_press_key"
        | "browser_login"
        | "browser_fill_secret"
        | "desktop_launch"
        | "desktop_act" => External,
        "verify_run" => Execute,
        _ => Read,
    }
}

fn runtime(name: &str) -> ToolRuntime {
    match name {
        "Task" | "AskUserQuestion" | "ExitPlanMode" | "SendMessage" | "ListAgents"
        | "ListSessions" => ToolRuntime::Harness,
        _ => ToolRuntime::Builtin,
    }
}

/// Registered but not implemented (Task, run by the harness): `execute` returns a
/// `not_implemented` error.
struct SchemaOnly(ToolSpec);

impl Tool for SchemaOnly {
    fn spec(&self) -> &ToolSpec {
        &self.0
    }
    fn execute<'a>(&'a self, _input: Value, _ctx: &'a ToolContext) -> BoxFuture<'a, ToolOutput> {
        Box::pin(async move {
            ToolOutput::error(format!(
                "{}: NotImplemented: this tool is not available in the Rust core yet.",
                self.0.name
            ))
            .with_meta(json!({"error": "not_implemented"}))
        })
    }
}

/// Filesystem tools are synchronous; they run on the blocking pool and poll `ctx.cancel`
/// themselves (a dropped future cannot stop a running blocking task).
macro_rules! fs_tool {
    ($ty:ident, $run:path) => {
        struct $ty(ToolSpec);
        impl Tool for $ty {
            fn spec(&self) -> &ToolSpec {
                &self.0
            }
            fn execute<'a>(
                &'a self,
                input: Value,
                ctx: &'a ToolContext,
            ) -> BoxFuture<'a, ToolOutput> {
                let ctx = ctx.clone();
                Box::pin(async move {
                    tokio::task::spawn_blocking(move || $run(&input, &ctx))
                        .await
                        .unwrap_or_else(|e| ToolOutput::error(format!("tool task failed: {e}")))
                })
            }
        }
    };
}
fs_tool!(ReadTool, read::run);
fs_tool!(WriteTool, write::run);
fs_tool!(EditTool, edit::run);
fs_tool!(GlobTool, glob::run);
fs_tool!(GrepTool, grep::run);
fs_tool!(NotebookTool, notebook::run);
fs_tool!(SkillTool, skill::run);

fn entry(v: Value, shared: &Arc<Shared>) -> Entry {
    let name = v["name"].as_str().expect("name").to_string();
    let spec = ToolSpec {
        description: v["description"].as_str().expect("description").to_string(),
        input_schema: v["input_schema"].clone(),
        side_effect: side_effect(&name),
        runtime: runtime(&name),
        name,
    };
    // Divergence (golden-divergences.json `validation_stricter`): these tools validate as if
    // `additionalProperties:false`; the published schema text stays verbatim.
    let mut checked = spec.input_schema.clone();
    if STRICT_INPUT.contains(&spec.name.as_str())
        && let Some(o) = checked.as_object_mut()
    {
        o.insert("additionalProperties".into(), Value::Bool(false));
    }
    let validator = jsonschema::validator_for(&checked).expect("valid input_schema");
    let tool: Arc<dyn Tool> = if let Some(run) = native::lookup(&spec.name) {
        Arc::new(Native {
            spec,
            shared: shared.clone(),
            run,
        })
    } else {
        match spec.name.as_str() {
            "Read" => Arc::new(ReadTool(spec)),
            "Write" => Arc::new(WriteTool(spec)),
            "Edit" => Arc::new(EditTool(spec)),
            "Glob" => Arc::new(GlobTool(spec)),
            "Grep" => Arc::new(GrepTool(spec)),
            "NotebookEdit" => Arc::new(NotebookTool(spec)),
            "Skill" => Arc::new(SkillTool(spec)),
            _ => Arc::new(SchemaOnly(spec)),
        }
    };
    Entry { tool, validator }
}

struct Entry {
    tool: Arc<dyn Tool>,
    validator: jsonschema::Validator,
}

/// Stable-order tool registry; `execute` validates input against the tool's schema first.
pub struct Registry {
    entries: Vec<Entry>,
    shared: Arc<Shared>,
}

impl Registry {
    /// The 20 ported tools in spec order, with a fail-closed terminal runtime
    /// (`SandboxMode::Enforce`, [`standard_policy`]).
    pub fn builtin() -> Self {
        Self::with_terminal(Arc::new(TerminalRuntime::new(SandboxMode::Enforce)))
    }

    /// Commands run confined by [`standard_policy`] under `rt`'s mode. Pass the same `rt` to
    /// hooks and the verification runner so every command shares one runtime and one sandbox.
    pub fn with_terminal(rt: Arc<TerminalRuntime>) -> Self {
        Self::with_terminal_policy(rt, Arc::new(standard_policy))
    }

    /// Own runtime in `mode`, sessions confined by what `policy` returns for their workspace.
    pub fn with_sandbox(mode: SandboxMode, policy: PolicyFactory) -> Self {
        Self::with_terminal_policy(Arc::new(TerminalRuntime::new(mode)), policy)
    }

    /// NO confinement (`SandboxMode::Off`). For tests only; product code uses `builtin`.
    pub fn unsandboxed_for_tests() -> Self {
        Self::with_sandbox(SandboxMode::Off, Arc::new(|_| None))
    }

    pub fn with_terminal_policy(rt: Arc<TerminalRuntime>, policy: PolicyFactory) -> Self {
        let shared = Arc::new(Shared::new(rt, policy));
        let raw: Vec<Value> = serde_json::from_str(SCHEMAS).expect("schemas.json is valid");
        let entries = raw.into_iter().map(|v| entry(v, &shared)).collect();
        Self { entries, shared }
    }

    /// Appends terminal_spawn/logs/signal/status, the browser_* tools, verify_run and
    /// artifact_read. Separate from `builtin` because the golden
    /// schema test pins the list of the 20 Node tools.
    pub fn with_new_tools(mut self) -> Self {
        let shared = self.shared.clone();
        for src in [TERMINAL_SCHEMAS, EXTRA_SCHEMAS] {
            let raw: Vec<Value> = serde_json::from_str(src).expect("valid");
            for v in raw {
                // Same name = a newer schema of a ported tool (Task gained name/run_in_background).
                let e = entry(v, &shared);
                match self
                    .entries
                    .iter()
                    .position(|x| x.tool.spec().name == e.tool.spec().name)
                {
                    Some(i) => self.entries[i] = e,
                    None => self.entries.push(e),
                }
            }
        }
        self
    }

    /// Interactive use: a foreground `Bash` call that reaches its timeout moves to the background
    /// (the model is told its id) instead of being killed. Headless keeps the kill: deterministic.
    pub fn with_background_on_timeout(self, on: bool) -> Self {
        self.shared
            .background_on_timeout
            .store(on, std::sync::atomic::Ordering::Relaxed);
        self
    }

    /// Browser settings (URL policy, image support) used when a session first launches its browser.
    pub fn with_browser(self, cfg: BrowserConfig) -> Self {
        *crate::native::lock(&self.shared.browser_cfg) = cfg;
        self
    }

    /// Registers the desktop_* tools with these settings. Without this call they do not exist.
    pub fn with_desktop(mut self, cfg: DesktopConfig) -> Self {
        *crate::native::lock(&self.shared.desktop_cfg) = cfg;
        let shared = self.shared.clone();
        let raw: Vec<Value> = serde_json::from_str(DESKTOP_SCHEMAS).expect("valid");
        self.entries
            .extend(raw.into_iter().map(|v| entry(v, &shared)));
        self
    }

    /// URL policy for WebFetch / WebSearch (default: private, loopback and metadata addresses denied).
    pub fn with_web(self, cfg: WebConfig) -> Self {
        *crate::native::lock(&self.shared.web_cfg) = cfg;
        self
    }

    /// Tool input safe to put in events and logs (secrets of `browser_fill sensitive` removed).
    pub fn redact_input(&self, name: &str, input: &Value) -> Value {
        crate::redact_input(name, input)
    }

    /// Session end: kills the session's background processes and browser and drops its todo/task state.
    pub fn close_session(&self, session_id: &str) {
        self.shared.close_session(session_id);
    }

    pub fn get(&self, name: &str) -> Option<&Arc<dyn Tool>> {
        self.entries
            .iter()
            .map(|e| &e.tool)
            .find(|t| t.spec().name == name)
    }

    pub fn specs(&self) -> impl Iterator<Item = &ToolSpec> {
        self.entries.iter().map(|e| e.tool.spec())
    }

    /// Pipeline steps 0-1 and 5 (tools.md §3): unknown tool, schema validation, execute.
    pub async fn execute(&self, name: &str, mut input: Value, ctx: &ToolContext) -> ToolOutput {
        let Some(entry) = self.entries.iter().find(|e| e.tool.spec().name == name) else {
            return ToolOutput::error(format!("Unknown tool \"{name}\""))
                .with_meta(json!({"error": "unknown_tool"}));
        };
        // Claude Code repairs these close-but-wrong key names of the Task tools before running them
        if matches!(name, "TaskGet" | "TaskUpdate")
            && let Some(o) = input.as_object_mut()
        {
            for (from, to) in [
                ("id", "taskId"),
                ("task_id", "taskId"),
                ("active_form", "activeForm"),
            ] {
                if !o.contains_key(to)
                    && let Some(v) = o.remove(from)
                {
                    o.insert(to.into(), v);
                }
            }
        }
        let violations: Vec<Value> = entry
            .validator
            .iter_errors(&input)
            .take(5)
            .map(|e| json!({"path": e.instance_path().to_string(), "message": e.to_string()}))
            .collect();
        if !violations.is_empty() {
            let list = violations
                .iter()
                .map(|v| {
                    format!(
                        "{} ({})",
                        v["message"].as_str().unwrap_or(""),
                        v["path"].as_str().unwrap_or("")
                    )
                })
                .collect::<Vec<_>>()
                .join("; ");
            return ToolOutput::error(format!("{name}: invalid input: {list}"))
                .with_meta(json!({"error": "invalid_input", "violations": violations}));
        }
        entry.tool.execute(input, ctx).await
    }
}

/// `SandboxPolicy::standard` for the workspace root (plus its additional dirs, writable) with a
/// private 0700 temp dir under the artifact root. `None` if that dir cannot be created.
pub fn standard_policy(ws: &WorkspaceContext) -> Option<SandboxPolicy> {
    standard_policy_with(ws, NetworkPolicy::Deny)
}

/// [`standard_policy`] with an explicit network mode (`synt run --network`).
pub fn standard_policy_with(
    ws: &WorkspaceContext,
    network: NetworkPolicy,
) -> Option<SandboxPolicy> {
    standard_policy_git(ws, network, None).ok()
}

/// [`standard_policy_with`] for a worktree session: `git_dir` is `GitInfo::git_dir`
/// (`<main>/.git/worktrees/<id>`). It and the main `.git` stay readable (git status/diff) but
/// unwritable, and the worktree's `.git` pointer file is read-only, so the agent cannot
/// redirect the `GIT_DIR` pin. Errors if either would be writable (e.g. `--sandbox-write` of
/// the repo) or `git_dir` has an unexpected shape. `git add` in the sandbox cannot write the
/// index (it lives in `git_dir`); Synthesis stages and commits from outside.
pub fn standard_policy_git(
    ws: &WorkspaceContext,
    network: NetworkPolicy,
    git_dir: Option<&Path>,
) -> Result<SandboxPolicy, String> {
    let tmp = ws.artifact_root.join("tmp");
    let unavailable = |e: std::io::Error| format!("sandbox tmp dir {}: {e}", tmp.display());
    std::fs::create_dir_all(&tmp).map_err(unavailable)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o700))
            .map_err(unavailable)?;
    }
    let mut p = SandboxPolicy::standard(&ws.root, &tmp);
    p.network = network;
    p.writable_roots.extend(ws.additional_dirs.iter().cloned());
    if let Some(git_dir) = git_dir {
        // <main>/.git/worktrees/<id> -> <main>/.git (objects, refs, config)
        let common = git_dir
            .parent()
            .filter(|w| w.file_name().is_some_and(|n| n == "worktrees"))
            .and_then(Path::parent)
            .ok_or_else(|| format!("{} is not <repo>/.git/worktrees/<id>", git_dir.display()))?;
        p.ensure_unwritable(common).map_err(|e| e.to_string())?;
        p.ensure_unwritable(git_dir).map_err(|e| e.to_string())?;
        p.readable_roots.push(common.to_path_buf());
        p.protect([ws.root.join(".git")]);
    }
    Ok(p)
}
