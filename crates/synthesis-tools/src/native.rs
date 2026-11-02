//! Tools that need session-scoped state or the terminal runtime. State lives in the registry
//! (`Shared`, keyed by `ToolContext.session_id`), not in `ToolContext`, and is created on the
//! session's first call.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex, MutexGuard};

use serde_json::Value;
use synthesis_browser::BrowserRuntime;
use synthesis_config::Env;
use synthesis_core::{ToolContext, WorkspaceContext};
use synthesis_terminal::{Error as TermError, SandboxPolicy, TerminalRuntime};

use crate::browser::{self, BrowserConfig};
use crate::tasks::Todo;
use crate::web::{self, WebConfig};
use crate::{BoxFuture, Tool, ToolOutput, ToolSpec, artifact, tasks, terminal, verify};

#[derive(Default)]
pub(crate) struct SessionState {
    pub todos: Mutex<Vec<Todo>>,
    /// BashOutput read position per process id.
    pub cursors: Mutex<HashMap<String, String>>,
    /// Launched on the first browser_* call; dropping it (session end) kills the browser.
    pub browser: tokio::sync::OnceCell<Arc<BrowserRuntime>>,
    /// Login profile the browser was started with.
    pub browser_profile: Mutex<Option<String>>,
    /// (secret, origin) pairs the person already approved in this session.
    pub secret_ok: Mutex<std::collections::HashSet<(String, String)>>,
    /// How often the person refused a question (`profile:NAME`, `secret:NAME@ORIGIN`).
    pub refusals: Mutex<HashMap<String, u8>>,
}

pub(crate) fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Sandbox policy for a session's commands, from its workspace (`None` = no policy).
pub type PolicyFactory = Arc<dyn Fn(&WorkspaceContext) -> Option<SandboxPolicy> + Send + Sync>;

pub(crate) struct Shared {
    pub rt: Arc<TerminalRuntime>,
    policy: PolicyFactory,
    pub browser_cfg: Mutex<BrowserConfig>,
    pub web_cfg: Mutex<WebConfig>,
    /// A foreground Bash call that times out keeps running in the background (interactive app)
    /// instead of being killed (headless).
    pub background_on_timeout: std::sync::atomic::AtomicBool,
    sessions: Mutex<HashMap<String, Arc<SessionState>>>,
}

impl Shared {
    pub fn new(rt: Arc<TerminalRuntime>, policy: PolicyFactory) -> Self {
        Self {
            rt,
            policy,
            browser_cfg: Mutex::default(),
            web_cfg: Mutex::default(),
            background_on_timeout: Default::default(),
            sessions: Mutex::default(),
        }
    }

    /// State of the context's session; the first call also opens it in the terminal runtime
    /// with `Env::child_env` of the context env (allowlist: secrets never reach a child).
    pub fn session(&self, ctx: &ToolContext) -> Arc<SessionState> {
        lock(&self.sessions)
            .entry(ctx.session_id.clone())
            .or_insert_with(|| {
                let env: BTreeMap<String, String> =
                    Env::from_pairs(ctx.env.iter().map(|(k, v)| (k.clone(), v.clone())))
                        .child_env()
                        .iter()
                        .map(|(k, v)| (k.to_string(), v.to_string()))
                        .collect();
                // The embedder may have opened it on the shared runtime already (hooks, gate).
                if matches!(
                    self.rt.list(&ctx.session_id),
                    Err(TermError::UnknownSession(_))
                ) {
                    self.rt.open_session_sandboxed(
                        &ctx.session_id,
                        ctx.workspace.clone(),
                        env,
                        (self.policy)(&ctx.workspace),
                    );
                }
                Arc::default()
            })
            .clone()
    }

    /// Session end: kills every process tree of the session and drops its state.
    pub fn close_session(&self, id: &str) {
        lock(&self.sessions).remove(id);
        self.rt.close_session(id);
    }
}

pub(crate) enum Run {
    Sync(fn(&Shared, &Value, &ToolContext) -> ToolOutput),
    Async(for<'a> fn(&'a Shared, &'a Value, &'a ToolContext) -> BoxFuture<'a, ToolOutput>),
}

/// Implementation for a tool name, if it is one of the native ones.
pub(crate) fn lookup(name: &str) -> Option<Run> {
    Some(match name {
        "Bash" => Run::Async(terminal::bash),
        "BashOutput" => Run::Async(terminal::bash_output),
        "KillShell" => Run::Async(terminal::kill_shell),
        "terminal_spawn" => Run::Async(terminal::spawn),
        "terminal_logs" => Run::Async(terminal::logs),
        "terminal_signal" => Run::Async(terminal::signal),
        "terminal_status" => Run::Async(terminal::status),
        "browser_navigate" => Run::Async(browser::navigate),
        "browser_snapshot" => Run::Async(browser::snapshot),
        "browser_click" => Run::Async(browser::click),
        "browser_fill" => Run::Async(browser::fill),
        "browser_login" => Run::Async(browser::login),
        "browser_fill_secret" => Run::Async(browser::fill_secret),
        "browser_press_key" => Run::Async(browser::press_key),
        "browser_wait_for" => Run::Async(browser::wait_for),
        "browser_screenshot" => Run::Async(browser::screenshot),
        "browser_console" => Run::Async(browser::console),
        "browser_network" => Run::Async(browser::network),
        "browser_tabs" => Run::Async(browser::tabs),
        "browser_close" => Run::Async(browser::close),
        "WebFetch" => Run::Async(web::fetch),
        "WebSearch" => Run::Async(web::search),
        "verify_run" => Run::Async(verify::run),
        "artifact_read" => Run::Async(artifact::read),
        "TodoWrite" => Run::Sync(tasks::todo_write),
        "TaskCreate" => Run::Sync(tasks::task_create),
        "TaskGet" => Run::Sync(tasks::task_get),
        "TaskList" => Run::Sync(tasks::task_list),
        "TaskUpdate" => Run::Sync(tasks::task_update),
        "AskUserQuestion" => Run::Sync(tasks::ask_user_question),
        "ExitPlanMode" => Run::Sync(tasks::exit_plan_mode),
        _ => return None,
    })
}

pub(crate) struct Native {
    pub spec: ToolSpec,
    pub shared: Arc<Shared>,
    pub run: Run,
}

impl Tool for Native {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }
    fn execute<'a>(&'a self, input: Value, ctx: &'a ToolContext) -> BoxFuture<'a, ToolOutput> {
        Box::pin(async move {
            match self.run {
                Run::Sync(f) => f(&self.shared, &input, ctx),
                Run::Async(f) => f(&self.shared, &input, ctx).await,
            }
        })
    }
}
