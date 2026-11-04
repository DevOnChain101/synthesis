//! TerminalRuntime (doc §5.8): ephemeral `exec`, long-running `spawn`, incremental `read_logs`,
//! `signal`, `status`, readiness checks, tree-kill and per-session cleanup.
//!
//! Design notes:
//! - Child stdout+stderr go straight to one log file under `<artifact_root>/terminal/` (merged by
//!   default; `exec` can split stderr into a second file). No output pipes, so a detached grandchild can never block us on EOF and the file is both the on-disk
//!   log and the buffer behind `read_logs` cursors (cursor = byte offset). There is no separate
//!   in-memory ring buffer.
//! - No monitor tasks: exit is observed lazily with `try_wait` (polled every `POLL`).
//! - The environment is exactly `session env + spec env` (`env_clear`); the host env is not
//!   inherited, so callers must put `PATH` in the session env. cwd is always set explicitly.
//! - Resource limits ([`Limits`]): per-process log cap (polled; the tree is killed when exceeded,
//!   `output_limit_exceeded`), max runtime for `spawn` services and a per-session cap on live
//!   spawned processes. `unsafe` is forbidden, so there is no RLIMIT_FSIZE via `pre_exec`.
//! - Sandbox (R7): with a [`SandboxPolicy`] (per call, else the session default from
//!   [`TerminalRuntime::open_session_sandboxed`]) the shell command is wrapped by the
//!   [`SandboxBackend`] (`synthesis_sandbox::detect()` unless injected with
//!   [`TerminalRuntime::with_sandbox_backend`]) before process-wrap spawns it, so group/job
//!   kill, limits and log handling apply to the wrapper (which execs in place or is the
//!   leader). The effective [`SandboxCapabilities`] are reported on [`ExecResult`] /
//!   [`ProcessStatus`]. Fail closed by default: the [`SandboxMode`] given to the constructor
//!   decides what happens when no confinement is possible. `Enforce` refuses (with
//!   [`Error::SandboxUnavailable`], nothing runs) a launch without a policy, with an unusable
//!   backend (`Guarantee::None`) or below `max(policy.required, Weak)`; `BestEffort` runs
//!   unconfined only when the backend gives no filesystem confinement at all; `Off` never
//!   sandboxes. Every unconfined run reports `sandbox: None` and
//!   `unsandboxed_by_request: true`. `Weak` guarantees are defence in depth, not a security
//!   boundary.
//! - Known limit without a sandbox: a process that leaves the process group/job (`setsid`,
//!   `setpgid`, double fork with a new session) escapes tree-kill and the limits above.
//! - Disk: besides the per-process cap, [`Limits::max_session_log_bytes`] bounds all log files
//!   of one session (tracked as they grow; the offending tree is killed, new launches are
//!   refused once it is used up). Logs are not deleted by this crate.
//! - Log files are created with `create_new`, mode 0600 (unix), random names, in a 0700 dir.
//! - Events: `synthesis_events` defines `process.*`; the runtime has no sink, so callers convert a
//!   [`ProcessStatus`] with [`ProcessStatus::to_event_info`] and emit.

mod shell;

use std::collections::{BTreeMap, HashMap};
use std::fs::{self, File, OpenOptions};
use std::hash::BuildHasher;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::process::{ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};

use process_wrap::tokio::{ChildWrapper, CommandWrap};
use synthesis_core::WorkspaceContext;
pub use synthesis_sandbox::{
    Guarantee, NetworkPolicy, SandboxBackend, SandboxCapabilities, SandboxPolicy,
};
use synthesis_sandbox::{SandboxError, SandboxedCommand};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::time::{sleep, timeout};
use tokio_util::sync::CancellationToken;

pub use shell::{ShellKind, ShellSpec, find_git_bash, native_shell, resolve_shell};

const POLL: Duration = Duration::from_millis(10);
const MONITOR: Duration = Duration::from_millis(50);

#[derive(Debug)]
pub enum Error {
    UnknownSession(String),
    UnknownProcess(String),
    InvalidSpec(String),
    /// A [`Limits`] cap was hit (e.g. too many live spawned processes).
    LimitExceeded(String),
    /// The sandbox policy cannot be satisfied (required guarantee not met, backend refused it).
    /// Nothing was run.
    SandboxUnavailable(String),
    Io(std::io::Error),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnknownSession(s) => write!(f, "unknown session: {s}"),
            Self::UnknownProcess(p) => write!(f, "unknown process: {p}"),
            Self::InvalidSpec(m) => write!(f, "invalid spec: {m}"),
            Self::LimitExceeded(m) => write!(f, "limit exceeded: {m}"),
            Self::SandboxUnavailable(m) => write!(f, "sandbox unavailable: {m}"),
            Self::Io(e) => write!(f, "{e}"),
        }
    }
}
impl std::error::Error for Error {}
impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}
pub type Result<T> = std::result::Result<T, Error>;

/// What to do when a command cannot be confined. Chosen explicitly at construction.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum SandboxMode {
    /// Fail closed: every launch needs a policy and a backend that meets it, else
    /// [`Error::SandboxUnavailable`].
    #[default]
    Enforce,
    /// Use the sandbox when the backend can confine (a failing requirement is still an error),
    /// but run unconfined (flagged `unsandboxed_by_request`) when there is no policy or the
    /// backend gives no filesystem confinement at all.
    BestEffort,
    /// Never sandbox, even when a policy is present. Flagged `unsandboxed_by_request`.
    Off,
}

/// Resource caps. Defaults are generous; tighten per deployment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// Per log file (stdout/stderr file of one process), polled every `POLL` (exec) or
    /// `MONITOR` (spawn), so it can overshoot by what the child writes in one interval.
    pub max_log_bytes: u64,
    /// Wall clock for `spawn` services (`exec` has its own `timeout`).
    pub spawn_max_runtime: Duration,
    /// Live (not yet exited) spawned processes per session.
    pub max_live_procs: usize,
    /// All log files of one session together (grows as processes write, same polling slack as
    /// `max_log_bytes`). Exceeding it kills the writing tree (`output_limit_exceeded`).
    pub max_session_log_bytes: u64,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_log_bytes: 64 << 20,
            spawn_max_runtime: Duration::from_secs(4 * 3600),
            max_live_procs: 32,
            max_session_log_bytes: 1 << 30,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Signal {
    Int,
    Term,
    Kill,
}

impl Signal {
    #[cfg(unix)]
    fn number(self) -> i32 {
        match self {
            Self::Int => 2,
            Self::Kill => 9,
            Self::Term => 15,
        }
    }
}

/// How `exec` captures the child's stderr.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum StderrMode {
    /// One log, stdout and stderr interleaved (`ExecResult::output`).
    #[default]
    Merged,
    /// stdout in `output`/`log_path`, stderr in `stderr`/`stderr_log_path`.
    Separate,
}

#[derive(Debug, Clone)]
pub struct ExecSpec {
    pub command: String,
    /// Overrides the workspace cwd; relative paths are joined onto it.
    pub cwd: Option<PathBuf>,
    pub env: BTreeMap<String, String>,
    pub timeout: Duration,
    pub max_output_bytes: usize,
    pub max_output_lines: usize,
    /// Written to the child's stdin, then closed. None = null stdin. A child that exits or
    /// ignores stdin never blocks `exec`; the write just ends (broken pipe is not an error).
    pub stdin: Option<Vec<u8>>,
    pub stderr: StderrMode,
    /// Overrides the session's default sandbox policy.
    pub sandbox: Option<SandboxPolicy>,
    /// Run unconfined whatever the mode and policy say (flagged `unsandboxed_by_request`). Only
    /// for a command the person typed, behind an explicit setting; never for model input.
    pub unconfined: bool,
    /// Live output while the command runs (see [`OutputSink`]).
    pub on_output: Option<OutputSink>,
    /// At the timeout keep the command running as a background process of the session (same as
    /// `spawn`: poll it with `read_logs` / `get_status`, stop it with `signal`) instead of killing
    /// it. If the session has no room for another one the command is killed as usual.
    pub background_on_timeout: bool,
}

/// Receives the new output of a running `exec`, UTF-8 safe, coalesced to at least 4 KiB or 50 ms,
/// and the rest when the command ends. The full output stays in the log.
#[derive(Clone)]
pub struct OutputSink(pub Arc<dyn Fn(&str) + Send + Sync>);

impl std::fmt::Debug for OutputSink {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("OutputSink")
    }
}

const OUT_CHUNK: usize = 4096;
const OUT_EVERY: Duration = Duration::from_millis(50);

/// Tails a log file for `OutputSink`.
struct Tail {
    file: Option<File>,
    pending: Vec<u8>,
    last: Instant,
    sink: OutputSink,
}

impl Tail {
    fn new(path: &Path, sink: OutputSink) -> Self {
        Self {
            file: File::open(path).ok(),
            pending: vec![],
            last: Instant::now(),
            sink,
        }
    }

    /// Reads what was appended; hands it on when big or old enough (`all`: whatever is there).
    fn pump(&mut self, all: bool) {
        if let Some(f) = &mut self.file {
            let _ = f.read_to_end(&mut self.pending);
        }
        if self.pending.is_empty()
            || !(all || self.pending.len() >= OUT_CHUNK || self.last.elapsed() >= OUT_EVERY)
        {
            return;
        }
        // keep an incomplete trailing character for the next round
        let keep = if all {
            0
        } else {
            match std::str::from_utf8(&self.pending) {
                Err(e) if e.error_len().is_none() => self.pending.len() - e.valid_up_to(),
                _ => 0,
            }
        };
        let rest = self.pending.split_off(self.pending.len() - keep);
        let text = String::from_utf8_lossy(&self.pending).into_owned();
        self.pending = rest;
        self.last = Instant::now();
        if !text.is_empty() {
            (self.sink.0)(&text);
        }
    }
}

impl ExecSpec {
    /// Defaults follow Node's Bash tool (30000 bytes) with a 2 minute timeout.
    pub fn new(command: impl Into<String>) -> Self {
        Self {
            command: command.into(),
            cwd: None,
            env: BTreeMap::new(),
            timeout: Duration::from_secs(120),
            max_output_bytes: 30_000,
            max_output_lines: 2_000,
            stdin: None,
            stderr: StderrMode::Merged,
            sandbox: None,
            unconfined: false,
            on_output: None,
            background_on_timeout: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecResult {
    /// None when killed by a signal (see `signal`) or on timeout/cancel.
    pub exit_code: Option<i32>,
    pub signal: Option<i32>,
    pub timed_out: bool,
    pub cancelled: bool,
    /// Combined stdout+stderr (stdout only with `StderrMode::Separate`), head 60% + tail 40%
    /// when truncated.
    pub output: String,
    pub truncated: bool,
    pub total_bytes: u64,
    /// The log cap ([`Limits::max_log_bytes`]) was hit and the tree was killed.
    pub output_limit_exceeded: bool,
    /// Full, untruncated output.
    pub log_path: PathBuf,
    /// `StderrMode::Separate` only (empty otherwise); same truncation rules as `output`.
    pub stderr: String,
    pub stderr_truncated: bool,
    pub stderr_total_bytes: u64,
    pub stderr_log_path: Option<PathBuf>,
    /// What the sandbox enforced; None = ran unsandboxed.
    pub sandbox: Option<SandboxCapabilities>,
    /// Ran unconfined because the runtime's [`SandboxMode`] allowed it (`sandbox` is None).
    pub unsandboxed_by_request: bool,
    /// `background_on_timeout` fired: the command is still running under this process id.
    /// `output` is what it had written so far; `timed_out` is false.
    pub background_id: Option<String>,
}

#[derive(Debug, Clone)]
pub enum Readiness {
    /// 127.0.0.1:port accepts a connection.
    Port(u16),
    /// `GET <url>` (plain `http://host:port/path`) answers 200.
    Http(String),
    /// Regex matches somewhere in the log.
    LogPattern(String),
}

#[derive(Debug, Clone)]
pub struct SpawnSpec {
    pub command: String,
    pub name: Option<String>,
    pub cwd: Option<PathBuf>,
    pub env: BTreeMap<String, String>,
    pub ready: Option<Readiness>,
    pub ready_timeout: Duration,
    /// Overrides the session's default sandbox policy.
    pub sandbox: Option<SandboxPolicy>,
}

impl SpawnSpec {
    pub fn new(command: impl Into<String>) -> Self {
        Self {
            command: command.into(),
            name: None,
            cwd: None,
            env: BTreeMap::new(),
            ready: None,
            ready_timeout: Duration::from_secs(30),
            sandbox: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessHandle {
    pub process_id: String,
    pub pid: u32,
    /// True when no check was requested or the check passed in time.
    pub ready: bool,
    pub log_cursor: String,
    pub log_path: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogChunk {
    pub data: String,
    /// Pass back to continue reading.
    pub cursor: String,
    /// The process has exited and the log is fully consumed.
    pub eof: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessStatus {
    pub process_id: String,
    pub name: Option<String>,
    pub pid: u32,
    pub running: bool,
    pub exit_code: Option<i32>,
    pub signal: Option<i32>,
    pub output_limit_exceeded: bool,
    /// Killed by [`Limits::spawn_max_runtime`].
    pub max_runtime_exceeded: bool,
    /// What the sandbox enforced; None = running unsandboxed.
    pub sandbox: Option<SandboxCapabilities>,
    /// Running unconfined because the runtime's [`SandboxMode`] allowed it.
    pub unsandboxed_by_request: bool,
}

impl ProcessStatus {
    /// Payload for `process.spawned|ready|exited`.
    pub fn to_event_info(&self) -> synthesis_events::ProcessInfo {
        synthesis_events::ProcessInfo {
            process_id: self.process_id.clone(),
            name: self.name.clone(),
            pid: Some(self.pid.into()),
            exit_code: self.exit_code.map(Into::into),
            signal: self.signal.map(|s| match s {
                2 => "SIGINT".into(),
                9 => "SIGKILL".into(),
                15 => "SIGTERM".into(),
                n => format!("SIG{n}"),
            }),
        }
    }
}

#[derive(Clone, Copy)]
struct Exit {
    code: Option<i32>,
    signal: Option<i32>,
}

impl From<ExitStatus> for Exit {
    fn from(s: ExitStatus) -> Self {
        #[cfg(unix)]
        let signal = std::os::unix::process::ExitStatusExt::signal(&s);
        #[cfg(not(unix))]
        let signal = None;
        Self {
            code: s.code(),
            signal,
        }
    }
}

/// Optional stdio wiring for `launch`.
#[derive(Default)]
struct Io {
    stdin: Option<Vec<u8>>,
    stderr_log: bool,
    max_runtime: Option<Duration>,
    sandbox: Option<SandboxPolicy>,
    unconfined: bool,
}

struct Proc {
    id: String,
    name: Option<String>,
    pid: u32,
    log: PathBuf,
    err_log: Option<PathBuf>,
    /// Taken by `Drop` to reap off the caller's thread.
    child: Mutex<Option<Box<dyn ChildWrapper>>>,
    exit: Mutex<Option<Exit>>,
    max_log: u64,
    deadline: Option<Instant>,
    output_limit: AtomicBool,
    runtime_limit: AtomicBool,
    sandbox: Option<SandboxCapabilities>,
    unsandboxed_by_request: bool,
    quota: Arc<Quota>,
    /// Bytes of this process's logs already added to `quota.used`.
    counted: AtomicU64,
}

/// Disk used by the logs of one session.
struct Quota {
    used: AtomicU64,
    max: u64,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl Proc {
    fn poll(&self) -> Option<Exit> {
        let mut exit = lock(&self.exit);
        if exit.is_none() {
            *exit = lock(&self.child)
                .as_mut()
                .and_then(|c| c.try_wait().ok().flatten())
                .map(Exit::from);
        }
        *exit
    }

    fn signal(&self, sig: Signal) -> Result<()> {
        let mut guard = lock(&self.child);
        let Some(child) = guard.as_mut() else {
            return Ok(());
        };
        #[cfg(unix)]
        child.signal(sig.number())?;
        // shortcut: Job Objects only terminate; INT/TERM become a kill, upgrade with CTRL_BREAK.
        #[cfg(not(unix))]
        {
            let _ = sig;
            child.start_kill()?;
        }
        Ok(())
    }

    /// SIGKILL / TerminateJobObject on the whole tree. Errors mean "already gone".
    fn kill_tree(&self) {
        if let Some(c) = lock(&self.child).as_mut() {
            let _ = c.start_kill();
        }
    }

    /// Checks the log cap and the runtime deadline; kills the tree on a hit. True if a limit
    /// was hit now or earlier.
    fn enforce(&self) -> bool {
        let len = |p: &Path| fs::metadata(p).map_or(0, |m| m.len());
        let over = |p: &Path| len(p) > self.max_log;
        let now = len(&self.log) + self.err_log.as_deref().map_or(0, len);
        let seen = self.counted.fetch_max(now, Ordering::Relaxed);
        let used = if now > seen {
            self.quota.used.fetch_add(now - seen, Ordering::Relaxed) + (now - seen)
        } else {
            self.quota.used.load(Ordering::Relaxed)
        };
        if over(&self.log) || self.err_log.as_deref().is_some_and(over) || used > self.quota.max {
            self.output_limit.store(true, Ordering::Relaxed);
        } else if self.deadline.is_some_and(|d| Instant::now() >= d) && self.poll().is_none() {
            self.runtime_limit.store(true, Ordering::Relaxed);
        }
        let hit =
            self.output_limit.load(Ordering::Relaxed) || self.runtime_limit.load(Ordering::Relaxed);
        if hit {
            self.kill_tree();
        }
        hit
    }

    async fn wait_exit(&self, within: Duration) -> Option<Exit> {
        let end = Instant::now() + within;
        loop {
            if let Some(e) = self.poll() {
                return Some(e);
            }
            if Instant::now() >= end {
                return None;
            }
            sleep(POLL).await;
        }
    }

    fn status(&self) -> ProcessStatus {
        let e = self.poll();
        ProcessStatus {
            process_id: self.id.clone(),
            name: self.name.clone(),
            pid: self.pid,
            running: e.is_none(),
            exit_code: e.and_then(|e| e.code),
            signal: e.and_then(|e| e.signal),
            output_limit_exceeded: self.output_limit.load(Ordering::Relaxed),
            max_runtime_exceeded: self.runtime_limit.load(Ordering::Relaxed),
            sandbox: self.sandbox.clone(),
            unsandboxed_by_request: self.unsandboxed_by_request,
        }
    }
}

impl Drop for Proc {
    /// Ephemeral procs, closed sessions and a dropped runtime all end here: kill the tree now,
    /// reap the leader on the blocking pool when inside a runtime (never sleep on an executor
    /// thread), inline otherwise.
    fn drop(&mut self) {
        self.kill_tree();
        let Some(mut child) = lock(&self.child).take() else {
            return;
        };
        let mut reap = move || {
            for _ in 0..100 {
                if child.try_wait().ok().flatten().is_some() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(5));
            }
        };
        match tokio::runtime::Handle::try_current() {
            Ok(h) => {
                h.spawn_blocking(reap);
            }
            Err(_) => reap(),
        }
    }
}

struct Session {
    ws: Arc<WorkspaceContext>,
    env: BTreeMap<String, String>,
    procs: HashMap<String, Arc<Proc>>,
    sandbox: Option<SandboxPolicy>,
    quota: Arc<Quota>,
}

/// Usable backends (best first) and the reasons other backends were skipped.
type Backends = (Vec<Arc<dyn SandboxBackend>>, Vec<String>);

pub struct TerminalRuntime {
    shell: ShellSpec,
    mode: SandboxMode,
    limits: Limits,
    sessions: Mutex<HashMap<String, Session>>,
    next_id: AtomicU64,
    /// Usable backends, best first, plus why others were skipped; detected on first sandboxed
    /// launch (detection may run a probe process).
    backends: OnceLock<Backends>,
    /// The unconfined-fallback notice is printed once.
    degraded_noted: AtomicBool,
}

impl Default for TerminalRuntime {
    /// Fail closed: [`SandboxMode::Enforce`].
    fn default() -> Self {
        Self::new(SandboxMode::Enforce)
    }
}

impl TerminalRuntime {
    pub fn new(mode: SandboxMode) -> Self {
        Self::with_shell(native_shell(), mode)
    }

    pub fn with_shell(shell: ShellSpec, mode: SandboxMode) -> Self {
        Self {
            shell,
            mode,
            limits: Limits::default(),
            sessions: Mutex::default(),
            next_id: AtomicU64::new(1),
            backends: OnceLock::new(),
            degraded_noted: AtomicBool::new(false),
        }
    }

    pub fn with_limits(mut self, limits: Limits) -> Self {
        self.limits = limits;
        self
    }

    /// `true`: allow unconfined runs when the backend cannot confine (`BestEffort`); `false`:
    /// `Enforce`. Same as choosing the mode in the constructor; never touches `Off`.
    pub fn allow_unsandboxed(mut self, allow: bool) -> Self {
        if self.mode != SandboxMode::Off {
            self.mode = if allow {
                SandboxMode::BestEffort
            } else {
                SandboxMode::Enforce
            };
        }
        self
    }

    pub fn mode(&self) -> SandboxMode {
        self.mode
    }

    /// Use this backend instead of `synthesis_sandbox::detect()` (tests, embedders).
    pub fn with_sandbox_backend(self, backend: Arc<dyn SandboxBackend>) -> Self {
        self.with_sandbox_chain(vec![backend])
    }

    /// Like [`Self::with_sandbox_backend`], with fallbacks: best-effort tries them in order.
    pub fn with_sandbox_chain(self, chain: Vec<Arc<dyn SandboxBackend>>) -> Self {
        let _ = self.backends.set((chain, vec![]));
        self
    }

    pub fn shell(&self) -> &ShellSpec {
        &self.shell
    }

    /// Registers a session; `env` is the already secret-stripped `ToolContext.env`.
    pub fn open_session(&self, id: &str, ws: Arc<WorkspaceContext>, env: BTreeMap<String, String>) {
        self.open_session_sandboxed(id, ws, env, None);
    }

    /// Like `open_session`, with a default sandbox policy for every exec/spawn of the session
    /// (a per-call `sandbox` overrides it).
    pub fn open_session_sandboxed(
        &self,
        id: &str,
        ws: Arc<WorkspaceContext>,
        env: BTreeMap<String, String>,
        sandbox: Option<SandboxPolicy>,
    ) {
        lock(&self.sessions).insert(
            id.to_string(),
            Session {
                ws,
                env,
                procs: HashMap::new(),
                sandbox,
                quota: Arc::new(Quota {
                    used: AtomicU64::new(0),
                    max: self.limits.max_session_log_bytes,
                }),
            },
        );
    }

    /// Session shutdown: kills every process tree of the session and forgets it.
    pub fn close_session(&self, id: &str) {
        let removed = lock(&self.sessions).remove(id);
        drop(removed); // Proc::drop does the killing, outside the registry lock
    }

    #[allow(clippy::type_complexity)]
    fn context(
        &self,
        id: &str,
    ) -> Result<(
        Arc<WorkspaceContext>,
        BTreeMap<String, String>,
        Option<SandboxPolicy>,
        Arc<Quota>,
    )> {
        let s = lock(&self.sessions);
        let s = s.get(id).ok_or_else(|| Error::UnknownSession(id.into()))?;
        Ok((
            s.ws.clone(),
            s.env.clone(),
            s.sandbox.clone(),
            s.quota.clone(),
        ))
    }

    fn backend_chain(&self) -> &Backends {
        self.backends.get_or_init(|| {
            let chain = synthesis_sandbox::detect_chain();
            (
                chain.into_iter().map(Arc::from).collect(),
                synthesis_sandbox::skipped_backends(),
            )
        })
    }

    /// Filesystem guarantee of the backend `Enforce` uses (`None` when nothing can confine).
    pub fn enforced_filesystem_guarantee(&self) -> Guarantee {
        self.backend_chain()
            .0
            .first()
            .map_or(Guarantee::None, |b| b.capabilities().filesystem)
    }

    /// Decides how a launch is confined. `Ok(None)` = run `cmd` as is (unconfined, allowed by the
    /// mode and flagged by the caller); otherwise the wrapped command and what it enforces.
    /// Runs before any file is created.
    fn sandboxed(
        &self,
        policy: Option<&SandboxPolicy>,
        cmd: &SandboxedCommand,
    ) -> Result<Option<(SandboxedCommand, SandboxCapabilities)>> {
        let refuse = |why: String| Err(Error::SandboxUnavailable(why));
        let policy = match (self.mode, policy) {
            (SandboxMode::Off, _) | (SandboxMode::BestEffort, None) => return Ok(None),
            (_, Some(p)) => p,
            (_, None) => {
                return refuse("no sandbox policy for this session (SandboxMode::Enforce)".into());
            }
        };
        let (chain, skipped) = self.backend_chain();
        let mut why = skipped.clone();
        // Enforce only ever uses the best backend (and refuses with its reason); best-effort
        // degrades to the next one when a backend cannot set up this policy.
        let usable = match self.mode {
            SandboxMode::Enforce => &chain[..chain.len().min(1)],
            _ => &chain[..],
        };
        for backend in usable {
            let caps = backend.capabilities();
            if caps.filesystem == Guarantee::None {
                continue;
            }
            // An unusable backend (None) never passes, whatever the policy asks.
            let required = policy.required.max(Guarantee::Weak);
            // Allow restricts nothing (Strong); Deny/Loopback must be honoured by the backend.
            let net = backend.network_guarantee(&policy.network);
            if caps.filesystem < required || net < required {
                return refuse(format!(
                    "{} gives filesystem {:?} / network {:?} for {:?}, policy requires {:?}",
                    backend.name(),
                    caps.filesystem,
                    net,
                    policy.network,
                    required
                ));
            }
            if let Some(t) = &policy.tmp_dir {
                create_private_dir(t)?;
            }
            match backend.wrap(policy, cmd) {
                Ok(w) => return Ok(Some((w, caps))),
                Err(SandboxError::InvalidPolicy(m)) => {
                    return Err(Error::InvalidSpec(format!("sandbox policy: {m}")));
                }
                Err(e @ (SandboxError::Unsupported(_) | SandboxError::Unavailable(_)))
                    if self.mode == SandboxMode::BestEffort =>
                {
                    why.push(format!("{}: {e}", backend.name()));
                }
                Err(e) => return Err(Error::SandboxUnavailable(e.to_string())),
            }
        }
        if self.mode == SandboxMode::BestEffort {
            // Flagged on every result (`sandbox: None`, `unsandboxed_by_request`), loud once.
            if !why.is_empty() && !self.degraded_noted.swap(true, Ordering::Relaxed) {
                eprintln!(
                    "synt: WARNING: no OS sandbox could be set up, commands run UNCONFINED (unsandboxed_by_request): {}",
                    why.join("; ")
                );
            }
            return Ok(None);
        }
        refuse(if why.is_empty() {
            "no OS sandbox is available on this machine".into()
        } else {
            why.join("; ")
        })
    }

    fn find(&self, sid: &str, pid: &str) -> Result<Arc<Proc>> {
        let s = lock(&self.sessions);
        let s = s
            .get(sid)
            .ok_or_else(|| Error::UnknownSession(sid.into()))?;
        s.procs
            .get(pid)
            .cloned()
            .ok_or_else(|| Error::UnknownProcess(pid.into()))
    }

    fn procs_of(&self, session_id: &str) -> Result<Vec<Arc<Proc>>> {
        let s = lock(&self.sessions);
        let s = s
            .get(session_id)
            .ok_or_else(|| Error::UnknownSession(session_id.into()))?;
        Ok(s.procs.values().cloned().collect())
    }

    fn launch(
        &self,
        sid: &str,
        command: &str,
        name: Option<String>,
        cwd: Option<&Path>,
        env: &BTreeMap<String, String>,
        mut io: Io,
    ) -> Result<Arc<Proc>> {
        if command.trim().is_empty() {
            return Err(Error::InvalidSpec("empty command".into()));
        }
        let (ws, mut vars, session_policy, quota) = self.context(sid)?;
        if quota.used.load(Ordering::Relaxed) >= quota.max {
            return Err(Error::LimitExceeded(format!(
                "session log quota of {} bytes used up",
                quota.max
            )));
        }
        vars.extend(env.iter().map(|(k, v)| (k.clone(), v.clone())));
        let cwd = match cwd {
            Some(c) => ws
                .resolve(&c.to_string_lossy())
                .map_err(|e| Error::InvalidSpec(format!("cwd: {e}")))?,
            None => ws.cwd.clone(),
        };
        // Shell startup files must not be injectable through the session env.
        vars.remove("BASH_ENV");
        vars.remove("ENV");
        let mut base_args: Vec<std::ffi::OsString> =
            self.shell.args.iter().map(Into::into).collect();
        base_args.push(command.into());
        let base = SandboxedCommand {
            program: self.shell.program.clone().into(),
            args: base_args,
            env: vars.iter().map(|(k, v)| (k.into(), v.into())).collect(),
            cwd: Some(cwd.clone()),
            clear_env: true,
        };
        let policy = io.sandbox.take().or(session_policy);
        let confined = if io.unconfined {
            None
        } else {
            self.sandboxed(policy.as_ref(), &base)?
        };
        let (cmd, sandbox) = match confined {
            Some((w, caps)) => (w, Some(caps)),
            None => (base, None),
        };
        let id = format!("proc_{}", self.next_id.fetch_add(1, Ordering::Relaxed));
        let dir = ws.artifact_root.join("terminal");
        create_private_dir(&dir)?;
        // Random per-process hasher keys: names are not guessable; `create_new` refuses any
        // pre-existing file or symlink anyway.
        let tag = std::hash::RandomState::new().hash_one(&id);
        let log_path = dir.join(format!("{id}-{tag:016x}.log"));
        let log = create_log(&log_path)?;
        let err_path = dir.join(format!("{id}-{tag:016x}.err.log"));
        let err_file = if io.stderr_log {
            Some(create_log(&err_path)?)
        } else {
            None
        };

        let mut wrap = CommandWrap::with_new(&cmd.program, |c| {
            #[cfg(windows)]
            let (last, init) = match (self.shell.kind, cmd.args.split_last()) {
                (ShellKind::Cmd, Some((l, i))) => (Some(l), i),
                _ => (None, &cmd.args[..]),
            };
            #[cfg(not(windows))]
            let init = &cmd.args[..];
            c.args(init);
            #[cfg(windows)]
            if let Some(l) = last {
                c.raw_arg(l); // cmd.exe needs the command line verbatim
            }
            if let Some(d) = &cmd.cwd {
                c.current_dir(d);
            }
            if cmd.clear_env {
                c.env_clear();
            }
            c.envs(cmd.env.iter().map(|(k, v)| (k, v)))
                .stdin(if io.stdin.is_some() {
                    Stdio::piped()
                } else {
                    Stdio::null()
                });
        });
        let (out, err) = match err_file {
            Some(e) => (log, e),
            None => (log.try_clone()?, log),
        };
        wrap.command_mut().stdout(out).stderr(err);
        // A new session (setsid) has no controlling terminal: a command cannot open /dev/tty to
        // read the person's answers to a prompt or push keystrokes into it. It is also a group
        // leader, so the process-tree kill is unchanged.
        #[cfg(unix)]
        wrap.wrap(process_wrap::tokio::ProcessSession);
        #[cfg(windows)]
        wrap.wrap(process_wrap::tokio::JobObject);
        let mut child = wrap.spawn()?;
        if let (Some(data), Some(mut pipe)) = (io.stdin.take(), child.stdin().take()) {
            // Detached writer: the pipe closes when it finishes or the child (tree) dies, which
            // fails the write; that error is the expected "child did not read it all".
            tokio::spawn(async move {
                let _ = pipe.write_all(&data).await;
            });
        }
        let pid = child.id().unwrap_or(0);
        Ok(Arc::new(Proc {
            id,
            name,
            pid,
            log: log_path,
            err_log: io.stderr_log.then_some(err_path),
            child: Mutex::new(Some(child)),
            exit: Mutex::new(None),
            max_log: self.limits.max_log_bytes,
            deadline: io.max_runtime.map(|d| Instant::now() + d),
            output_limit: AtomicBool::new(false),
            runtime_limit: AtomicBool::new(false),
            unsandboxed_by_request: sandbox.is_none(),
            sandbox,
            quota,
            counted: AtomicU64::new(0),
        }))
    }

    /// Runs a command to completion. Stdout and stderr are merged unless `spec.stderr` is
    /// `Separate`. On timeout or cancellation
    /// the whole process tree is killed; after normal completion leftover group members are
    /// killed too (ephemeral means ephemeral, use `spawn` for services).
    pub async fn exec(
        &self,
        session_id: &str,
        mut spec: ExecSpec,
        cancel: &CancellationToken,
    ) -> Result<ExecResult> {
        let p = self.launch(
            session_id,
            &spec.command,
            None,
            spec.cwd.as_deref(),
            &spec.env,
            Io {
                stdin: spec.stdin.take(),
                stderr_log: spec.stderr == StderrMode::Separate,
                max_runtime: spec
                    .background_on_timeout
                    .then_some(self.limits.spawn_max_runtime),
                sandbox: spec.sandbox.take(),
                unconfined: spec.unconfined,
            },
        )?;
        let deadline = Instant::now() + spec.timeout;
        let (mut timed_out, mut cancelled, mut background_id) = (false, false, None);
        let mut tail = spec.on_output.take().map(|s| Tail::new(&p.log, s));
        while p.poll().is_none() {
            if let Some(t) = &mut tail {
                t.pump(false);
            }
            if Instant::now() >= deadline {
                if spec.background_on_timeout && self.adopt(session_id, &p).is_ok() {
                    background_id = Some(p.id.clone());
                    break;
                }
                timed_out = true;
            }
            if timed_out || cancelled || p.enforce() {
                p.kill_tree();
                p.wait_exit(Duration::from_secs(5)).await;
                break;
            }
            tokio::select! {
                () = cancel.cancelled() => cancelled = true,
                () = sleep(POLL) => {}
            }
        }
        if background_id.is_none() {
            p.kill_tree(); // grandchildren that outlived the leader
        }
        p.enforce(); // a fast writer may have exited between polls
        if let Some(t) = &mut tail {
            t.pump(true);
        }
        let exit = p.poll();
        let (output, truncated, total_bytes) =
            read_truncated(&p.log, spec.max_output_bytes, spec.max_output_lines)?;
        let (stderr, stderr_truncated, stderr_total_bytes) = match &p.err_log {
            Some(e) => read_truncated(e, spec.max_output_bytes, spec.max_output_lines)?,
            None => (String::new(), false, 0),
        };
        Ok(ExecResult {
            exit_code: exit.and_then(|e| e.code),
            signal: exit.and_then(|e| e.signal),
            timed_out,
            cancelled,
            output,
            truncated,
            total_bytes,
            output_limit_exceeded: p.output_limit.load(Ordering::Relaxed),
            log_path: p.log.clone(),
            stderr,
            stderr_truncated,
            stderr_total_bytes,
            stderr_log_path: p.err_log.clone(),
            sandbox: p.sandbox.clone(),
            unsandboxed_by_request: p.unsandboxed_by_request,
            background_id,
        })
    }

    fn check_room(&self, session_id: &str) -> Result<()> {
        let cap = self.limits.max_live_procs;
        let live = self
            .procs_of(session_id)?
            .iter()
            .filter(|p| p.poll().is_none())
            .count();
        if live >= cap {
            return Err(Error::LimitExceeded(format!(
                "{live} live spawned processes in session (max {cap}); stop one first"
            )));
        }
        Ok(())
    }

    /// Makes a launched process a session process (listed, pollable, killable, monitored) if the
    /// session has room, as `spawn` does.
    fn adopt(&self, session_id: &str, p: &Arc<Proc>) -> Result<()> {
        self.check_room(session_id)?;
        lock(&self.sessions)
            .get_mut(session_id)
            .ok_or_else(|| Error::UnknownSession(session_id.into()))?
            .procs
            .insert(p.id.clone(), p.clone());
        // Nobody polls a service, so a monitor enforces the limits (and reaps) until it exits
        // or the session drops it.
        let weak = Arc::downgrade(p);
        tokio::spawn(async move {
            loop {
                sleep(MONITOR).await;
                let Some(p) = weak.upgrade() else { break };
                p.enforce();
                if p.poll().is_some() {
                    break;
                }
            }
        });
        Ok(())
    }

    /// Starts a service that outlives this call (not the session). With `spec.ready` it waits
    /// for the check, giving up on timeout or when the process exits first (`ready == false`).
    pub async fn spawn(&self, session_id: &str, spec: SpawnSpec) -> Result<ProcessHandle> {
        if let Some(Readiness::LogPattern(re)) = &spec.ready {
            regex::Regex::new(re).map_err(|e| Error::InvalidSpec(e.to_string()))?;
        }
        self.check_room(session_id)?;
        let p = self.launch(
            session_id,
            &spec.command,
            spec.name,
            spec.cwd.as_deref(),
            &spec.env,
            Io {
                max_runtime: Some(self.limits.spawn_max_runtime),
                sandbox: spec.sandbox,
                ..Io::default()
            },
        )?;
        self.adopt(session_id, &p)?;
        let ready = match &spec.ready {
            None => true,
            Some(r) => wait_ready(&p, r, spec.ready_timeout).await,
        };
        Ok(ProcessHandle {
            process_id: p.id.clone(),
            pid: p.pid,
            ready,
            log_cursor: "0".into(),
            log_path: p.log.clone(),
        })
    }

    /// Reads up to `max_bytes` after `cursor` (None = from the start).
    pub fn read_logs(
        &self,
        session_id: &str,
        process_id: &str,
        cursor: Option<&str>,
        max_bytes: usize,
    ) -> Result<LogChunk> {
        let p = self.find(session_id, process_id)?;
        let start: u64 = match cursor {
            None => 0,
            Some(c) => c
                .parse()
                .map_err(|_| Error::InvalidSpec(format!("bad cursor: {c}")))?,
        };
        let exited = p.poll().is_some(); // before reading, so the log is final when true
        let mut f = File::open(&p.log)?;
        let len = f.metadata()?.len();
        f.seek(SeekFrom::Start(start.min(len)))?;
        let mut buf = Vec::new();
        f.take(max_bytes as u64).read_to_end(&mut buf)?;
        // Do not split a multi-byte char across chunks.
        if let Err(e) = std::str::from_utf8(&buf)
            && e.error_len().is_none()
            && e.valid_up_to() > 0
        {
            buf.truncate(e.valid_up_to());
        }
        let next = start.min(len) + buf.len() as u64;
        Ok(LogChunk {
            data: String::from_utf8_lossy(&buf).into_owned(),
            cursor: next.to_string(),
            eof: exited && next >= len,
        })
    }

    pub fn signal(&self, session_id: &str, process_id: &str, sig: Signal) -> Result<()> {
        self.find(session_id, process_id)?.signal(sig)
    }

    pub fn get_status(&self, session_id: &str, process_id: &str) -> Result<ProcessStatus> {
        Ok(self.find(session_id, process_id)?.status())
    }

    /// All processes of one session, by id.
    pub fn list(&self, session_id: &str) -> Result<Vec<ProcessStatus>> {
        let procs = self.procs_of(session_id)?;
        let mut v: Vec<_> = procs.iter().map(|p| p.status()).collect();
        v.sort_by_key(|s| s.pid);
        Ok(v)
    }
}

fn create_private_dir(dir: &Path) -> std::io::Result<()> {
    let mut b = fs::DirBuilder::new();
    b.recursive(true);
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut b, 0o700);
    b.create(dir)
}

/// New file only (never follows a planted symlink or reuses a file), 0600 on unix.
fn create_log(path: &Path) -> std::io::Result<File> {
    let mut o = OpenOptions::new();
    o.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut o, 0o600);
    o.open(path)
}

async fn wait_ready(p: &Proc, check: &Readiness, within: Duration) -> bool {
    let end = Instant::now() + within;
    loop {
        if probe(p, check).await {
            return true;
        }
        if p.poll().is_some() || Instant::now() >= end {
            return false;
        }
        sleep(Duration::from_millis(25)).await;
    }
}

async fn probe(p: &Proc, check: &Readiness) -> bool {
    match check {
        Readiness::Port(port) => connect(("127.0.0.1", *port)).await.is_some(),
        Readiness::LogPattern(re) => {
            let text = fs::read(&p.log).map(|b| String::from_utf8_lossy(&b).into_owned());
            regex::Regex::new(re).is_ok_and(|re| text.is_ok_and(|t| re.is_match(&t)))
        }
        Readiness::Http(url) => http_ok(url).await,
    }
}

async fn connect(addr: (&str, u16)) -> Option<TcpStream> {
    timeout(Duration::from_millis(500), TcpStream::connect(addr))
        .await
        .ok()?
        .ok()
}

/// Request target of a readiness probe, all parts checked.
#[derive(Debug, PartialEq, Eq)]
struct ProbeUrl {
    /// `Host:` header value (as written, already validated).
    hostport: String,
    /// Address to connect to; loopback literals only, never resolved through DNS.
    connect: (&'static str, u16),
    /// Starts with `/`.
    target: String,
}

/// `http://{127.0.0.1|localhost|[::1]}:port[/path]` only, port >= 1024 (privileged and metadata-style
/// services never answer a probe) and a plain path `^/[A-Za-z0-9._~/-]{0,128}$`: no query, no
/// escapes, so the probe cannot hit a state-changing endpoint of whatever listens on the port.
/// shortcut: the port is not checked against the sockets of the spawned process tree (needs
/// lsof or /proc per OS); upgrade if loopback services owned by other processes matter. Anything else (other hosts such as
/// 169.254.169.254, userinfo, whitespace/control characters, non-ASCII, backslashes, fragments)
/// is rejected rather than escaped, so no byte of the URL can add a header or change the host.
fn parse_probe_url(url: &str) -> Option<ProbeUrl> {
    let rest = url.strip_prefix("http://")?;
    let safe = |c: char| c.is_ascii_alphanumeric() || "-._~:/?&=%+,;@!$'()*[]".contains(c);
    if !rest.chars().all(safe) {
        return None;
    }
    let (hostport, path) = rest.split_once('/').map_or((rest, ""), |(h, p)| (h, p));
    let (host, port) = match hostport.rsplit_once(':') {
        Some((h, p)) if !h.ends_with(':') && !p.contains(']') => (h, p.parse::<u16>().ok()?),
        _ => (hostport, 80),
    };
    let plain = |c: char| c.is_ascii_alphanumeric() || "._~/-".contains(c);
    if port < 1024 || path.len() > 127 || !path.chars().all(plain) {
        return None;
    }
    let connect = match host {
        "127.0.0.1" | "localhost" => "127.0.0.1",
        "[::1]" => "::1",
        _ => return None,
    };
    Some(ProbeUrl {
        hostport: hostport.into(),
        connect: (connect, port),
        target: format!("/{path}"),
    })
}

/// Whether `url` can be probed at all (same rules as the probe itself), for callers that must
/// refuse a readiness URL up front instead of timing out.
pub fn validate_probe_url(url: &str) -> std::result::Result<(), String> {
    parse_probe_url(url).map(drop).ok_or_else(|| {
        "must be http://127.0.0.1, http://localhost or http://[::1] with an explicit port >= 1024 and a path of letters, digits and . _ ~ / - only (no userinfo, query or fragment)".into()
    })
}

/// Minimal HTTP/1.1 GET over TCP to a loopback address.
async fn http_ok(url: &str) -> bool {
    let Some(u) = parse_probe_url(url) else {
        return false;
    };
    let Some(mut s) = connect(u.connect).await else {
        return false;
    };
    let req = format!(
        "GET {} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n\r\n",
        u.target, u.hostport
    );
    let mut line = [0u8; 12]; // "HTTP/1.1 200"
    let r = timeout(Duration::from_secs(2), async {
        s.write_all(req.as_bytes()).await?;
        s.read_exact(&mut line).await
    })
    .await;
    matches!(r, Ok(Ok(_))) && line.starts_with(b"HTTP/") && &line[9..12] == b"200"
}

/// (text, truncated, total_bytes). Over the byte cap keeps head 60% + tail 40% (as Node's Bash
/// tool); over the line cap likewise.
fn read_truncated(
    path: &Path,
    max_bytes: usize,
    max_lines: usize,
) -> std::io::Result<(String, bool, u64)> {
    let mut f = File::open(path)?;
    let total = f.metadata()?.len();
    let mut truncated = false;
    let mut text = if total as usize <= max_bytes {
        let mut b = Vec::new();
        f.read_to_end(&mut b)?;
        String::from_utf8_lossy(&b).into_owned()
    } else {
        truncated = true;
        let head_len = max_bytes * 6 / 10;
        let tail_len = max_bytes - head_len;
        let (mut head, mut tail) = (vec![0; head_len], vec![0; tail_len]);
        f.read_exact(&mut head)?;
        f.seek(SeekFrom::Start(total - tail_len as u64))?;
        f.read_exact(&mut tail)?;
        format!(
            "{}\n\n... [{} bytes truncated] ...\n\n{}",
            String::from_utf8_lossy(&head),
            total - max_bytes as u64,
            String::from_utf8_lossy(&tail)
        )
    };
    let lines: Vec<&str> = text.lines().collect();
    if lines.len() > max_lines {
        let head = max_lines * 6 / 10;
        let tail = max_lines - head;
        text = format!(
            "{}\n\n... [{} lines truncated] ...\n\n{}",
            lines[..head].join("\n"),
            lines.len() - max_lines,
            lines[lines.len() - tail..].join("\n")
        );
        truncated = true;
    }
    Ok((text, truncated, total))
}

#[cfg(test)]
mod url_tests {
    use super::*;

    #[test]
    fn accepts_loopback_only() {
        let u = parse_probe_url("http://127.0.0.1:8080/health").unwrap();
        assert_eq!(u.connect, ("127.0.0.1", 8080));
        assert_eq!(
            (u.hostport.as_str(), u.target.as_str()),
            ("127.0.0.1:8080", "/health")
        );
        assert_eq!(
            parse_probe_url("http://localhost:3000/").unwrap().connect,
            ("127.0.0.1", 3000)
        );
        assert_eq!(
            parse_probe_url("http://[::1]:3000").unwrap().connect,
            ("::1", 3000)
        );
        for bad in [
            "http://127.0.0.1:8080/health?x=1", // no query
            "http://127.0.0.1:8080/a%2e",       // no escapes
            "http://localhost/",                // port < 1024 (default 80)
            "http://127.0.0.1:1023/",
            &format!("http://127.0.0.1:8080/{}", "a".repeat(128)),
            "http://169.254.169.254/latest/meta-data/",
            "http://169.254.169.254:80/",
            "http://10.0.0.1/",
            "http://example.com/",
            "http://127.0.0.2/",
            "http://127.1/",
            "http://2130706433/",
            "http://127.0.0.1.evil.com/",
            "http://localhost.evil.com/",
            "http://0.0.0.0:80/",
            "http://[::ffff:169.254.169.254]/",
            "https://127.0.0.1/",
            "ftp://127.0.0.1/",
            "127.0.0.1",
            "",
        ] {
            assert_eq!(parse_probe_url(bad), None, "{bad}");
        }
    }

    #[test]
    fn rejects_userinfo_and_unsafe_characters() {
        for bad in [
            "http://user@127.0.0.1/",
            "http://user:pw@127.0.0.1/",
            "http://evil.com@127.0.0.1/", // userinfo trick
            "http://127.0.0.1:80@evil.com/",
            "http://127.0.0.1:99999/",
            "http://127.0.0.1:/",
            "http://127.0.0.1:80:80/",
            "http://127.0.0.1/a b",
            "http://127.0.0.1/a\\b",
            "http://127.0.0.1/a#frag",
            "http://127.0.0.1/\u{e9}",
            "http://127.0.0.1/\x7f",
            "http://127.0.0.1/\0",
            "http://127.0.0.1/\t",
            "http://127.0.0.1 /",
        ] {
            // `@` in the *path* is legal, but never before the first `/` (userinfo).
            let r = parse_probe_url(bad);
            assert_eq!(r, None, "{bad:?}");
        }
    }

    #[test]
    fn crlf_cannot_inject_headers_or_requests() {
        for bad in [
            "http://127.0.0.1/x\r\nX-Evil: 1",
            "http://127.0.0.1/x\nX-Evil: 1",
            "http://127.0.0.1/x%0d%0aX: 1\r\n",
            "http://127.0.0.1\r\nHost: evil/",
            "http://127.0.0.1:80\r\n/",
            "http://127.0.0.1/ HTTP/1.1\r\nHost: evil\r\n\r\nGET /",
        ] {
            assert_eq!(parse_probe_url(bad), None, "{bad:?}");
        }
        // escapes are rejected outright; the longest allowed path is 127 chars after the `/`
        assert_eq!(parse_probe_url("http://127.0.0.1:8080/a%0d%0ab"), None);
        let ok = format!("http://127.0.0.1:8080/{}", "a".repeat(127));
        assert!(parse_probe_url(&ok).is_some());
    }

    #[tokio::test]
    async fn rejected_urls_never_open_a_connection() {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        l.set_nonblocking(true).unwrap();
        let port = l.local_addr().unwrap().port();
        for bad in [
            format!("http://127.0.0.1:{port}/x\r\nX-Evil: 1"),
            format!("http://u@127.0.0.1:{port}/"),
            format!("http://localhost.evil.com:{port}/"),
            "http://169.254.169.254/latest/meta-data/".to_string(),
        ] {
            assert!(!http_ok(&bad).await, "{bad:?}");
        }
        assert_eq!(
            l.accept().unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock
        );
    }

    #[test]
    fn validate_probe_url_matches_the_parser() {
        for ok in ["http://127.0.0.1:8080/", "http://localhost:3000/health"] {
            assert_eq!(validate_probe_url(ok), Ok(()), "{ok}");
        }
        for bad in [
            "http://169.254.169.254:8080/",
            "http://192.0.2.1:8080/",
            "http://u:p@127.0.0.1:8080/",
            "http://127.0.0.1:8080/x?y=1",
            "http://127.0.0.1:80/",
            "http://127.0.0.1:8080/a\r\nHost: x",
        ] {
            assert!(validate_probe_url(bad).is_err(), "{bad:?}");
        }
    }
}
