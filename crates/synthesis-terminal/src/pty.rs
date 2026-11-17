//! Shared PTY (docs/specs/terminal-browser.md 1.2, 1.7): an interactive shell on a pseudo
//! terminal, owned by the engine, spawned exactly like a `Bash` command (same sandbox wrap and
//! fail-closed modes, `env_clear` plus the session env) and counted against [`crate::Limits`].
//!
//! - Output: one reader thread per PTY feeds an engine-side `alacritty_terminal::Term` (the
//!   screen the agent will read), appends the raw bytes to a 0600 log under
//!   `<artifact_root>/terminal/` (per-process cap and session quota, the tree is killed on a hit)
//!   and, while a subscriber exists ([`Pty::subscribe`]), forwards them. At most [`WINDOW`] bytes
//!   are unacknowledged by the subscriber; past that the reader stops reading the master, so the
//!   child blocks on write (normal PTY flow control). Nothing in here is an unbounded queue.
//! - Kill: unix SIGKILL to the shell's process group (portable-pty makes the child a session
//!   and group leader) and to the current foreground group (a job of the interactive shell);
//!   Windows `taskkill /T /F`; both on drop, so closing a session or the runtime kills every
//!   PTY. The shell's pid is never signalled once it was reaped (it may be recycled). Known limit (as `TerminalRuntime`): without a sandbox that
//!   confines the process tree (bubblewrap's PID namespace), a process that leaves both groups
//!   (`setsid`, double fork, a background job `cmd &` of the interactive shell) survives.
//! - The environment is the session env minus `SYNT_*` (except the `SYNT_AGENT_SESSION`
//!   marker) and secret-shaped names; `TERM`/`COLORTERM` describe our emulator.
//! - The engine `Term` answers terminal queries (cursor position report, ...): ConPTY needs it.
//! - shortcut: no 16 ms chunk coalescing (a chunk is one `read`, at most 64 KiB), no disk-quota
//!   watcher; add them with the consumers (T2/T4).

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use alacritty_terminal::event::{Event, EventListener};
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::{Column, Line, Point};
use alacritty_terminal::term::{Config, Osc52, Term};
use alacritty_terminal::vte::ansi::Processor;
use portable_pty::{Child, CommandBuilder, MasterPty, PtySize, native_pty_system};
use synthesis_sandbox::{SandboxCapabilities, SandboxPolicy, SandboxedCommand};

use crate::{Error, Result, ShellKind, TerminalRuntime, create_log, create_private_dir, lock};

/// Bytes the subscriber may leave unacknowledged before the reader stops reading the master.
pub const WINDOW: usize = 1 << 20;
const CHUNK: usize = 64 << 10;
const SCROLLBACK: usize = 5000;
/// Shared PTYs per session, exited ones included until `close_pty` (running ones also count in
/// `Limits::max_live_procs`).
pub const MAX_PTYS: usize = 4;
/// Bounds the Term memory: (5000 scrollback + 500 rows) x 300 cols x ~24 B per cell, about 40 MB
/// per PTY (scrollback is released when the output ends).
pub const MAX_COLS: u16 = 300;
pub const MAX_ROWS: u16 = 500;
/// One `write_input` call (spec 1.9 `pty.input`).
pub const MAX_INPUT: usize = 64 << 10;
/// Output-only marker the core sets for commands the agent can reach (`synthesis-config`).
const AGENT_MARKER: &str = "SYNT_AGENT_SESSION";

#[derive(Debug, Clone)]
pub struct PtySpec {
    pub cols: u16,
    pub rows: u16,
    /// Relative paths are joined onto the workspace cwd; must stay inside the workspace roots.
    pub cwd: Option<PathBuf>,
    pub env: BTreeMap<String, String>,
    /// Overrides the session's default sandbox policy. Trusted code only (settings, the person's
    /// action), never model-supplied tool arguments; same for `env`.
    pub sandbox: Option<SandboxPolicy>,
}

impl PtySpec {
    pub fn new(cols: u16, rows: u16) -> Self {
        Self {
            cols,
            rows,
            cwd: None,
            env: BTreeMap::new(),
            sandbox: None,
        }
    }
}

struct Dims {
    cols: usize,
    rows: usize,
}

impl Dimensions for Dims {
    fn total_lines(&self) -> usize {
        self.rows
    }
    fn screen_lines(&self) -> usize {
        self.rows
    }
    fn columns(&self) -> usize {
        self.cols
    }
}

fn io_err(e: impl std::fmt::Display) -> Error {
    Error::Io(std::io::Error::other(e.to_string()))
}

fn check_size(cols: u16, rows: u16) -> Result<()> {
    if (1..=MAX_COLS).contains(&cols) && (1..=MAX_ROWS).contains(&rows) {
        Ok(())
    } else {
        Err(Error::InvalidSpec(format!(
            "pty size {cols}x{rows} outside 1..={MAX_COLS} x 1..={MAX_ROWS}"
        )))
    }
}

/// Unacknowledged bytes of the subscriber; the reader waits here before reading the master.
#[derive(Default)]
struct Window {
    used: Mutex<usize>,
    cv: Condvar,
}

impl Window {
    /// Blocks while the window is full; wakes up at least every 50 ms to see `closed`.
    fn wait_room(&self, closed: &AtomicBool) {
        let mut used = lock(&self.used);
        while *used >= WINDOW && !closed.load(Ordering::Relaxed) {
            used = self
                .cv
                .wait_timeout(used, Duration::from_millis(50))
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
    }

    fn add(&self, n: usize) {
        *lock(&self.used) += n;
    }

    fn release(&self, n: usize) {
        let mut used = lock(&self.used);
        *used = used.saturating_sub(n);
        self.cv.notify_all();
    }

    fn reset(&self) {
        *lock(&self.used) = 0;
        self.cv.notify_all();
    }
}

/// What kills the tree; shared with the reader thread so the log cap can kill it too.
struct Ctl {
    pid: Option<u32>,
    /// `None` after the PTY was dropped (closes the pseudo console).
    master: Mutex<Option<Box<dyn MasterPty + Send>>>,
    /// The shell was reaped, so its pid may be recycled and must never be signalled. Held while
    /// signalling and while reaping, so a reap cannot slip in between the check and the kill.
    reaped: Mutex<bool>,
}

impl Ctl {
    /// SIGKILL to the shell's group and to the foreground group (a job of the interactive
    /// shell has its own); `taskkill /T /F` on Windows (blocks while it runs).
    fn kill_groups(&self) {
        self.kill_groups_with(kill_group);
    }

    fn kill_groups_with(&self, kill: impl Fn(u32)) {
        // the foreground lookup stays after a reap (it simply fails once the session is gone)
        #[cfg(unix)]
        if let Some(fg) = lock(&self.master)
            .as_ref()
            .and_then(|m| m.process_group_leader())
        {
            kill(fg.unsigned_abs());
        }
        let reaped = lock(&self.reaped);
        if let (false, Some(pid)) = (*reaped, self.pid) {
            kill(pid);
        }
    }

    /// `try_wait` that records the reap under the `reaped` lock.
    fn try_wait(&self, child: &mut dyn Child) -> Option<u32> {
        let mut reaped = lock(&self.reaped);
        let code = child.try_wait().ok().flatten().map(|s| s.exit_code());
        *reaped |= code.is_some();
        code
    }

    /// The shell has exited but is not reaped yet (its pid is still ours). Does not reap.
    #[cfg(unix)]
    fn exited_unreaped(&self) -> bool {
        use rustix::process::{Pid, WaitId, WaitIdOptions, waitid};
        let reaped = lock(&self.reaped);
        let Some(pid) = self
            .pid
            .and_then(|p| i32::try_from(p).ok())
            .and_then(Pid::from_raw)
        else {
            return true;
        };
        *reaped
            || waitid(
                WaitId::Pid(pid),
                WaitIdOptions::EXITED | WaitIdOptions::NOWAIT | WaitIdOptions::NOHANG,
            )
            .map_or(true, |st| st.is_some())
    }
}

type Writer = Arc<Mutex<Box<dyn Write + Send>>>;

/// Answers the terminal queries the program sends (cursor position report and the like). ConPTY
/// asks for the cursor position at start-up and does not pass output until it is answered, so
/// the engine `Term` must reply (the drawing `Term` of a shared PTY must NOT, or the program gets
/// two answers). A reply is dropped when the writer is busy: the reader thread must never block
/// on a child that is not reading its input.
struct Reply(Writer);

impl EventListener for Reply {
    fn send_event(&self, event: Event) {
        if let Event::PtyWrite(text) = event
            && let Ok(mut w) = self.0.try_lock()
        {
            let _ = w.write_all(text.as_bytes());
            let _ = w.flush();
        }
    }
}

struct Screen {
    term: Term<Reply>,
    parser: Processor,
}

struct Inner {
    screen: Mutex<Screen>,
    window: Window,
    sink: Mutex<Option<Sender<Vec<u8>>>>,
    output_limit: AtomicBool,
    /// The reader hit EOF or an error (the child side is gone).
    eof: AtomicBool,
    /// The PTY was killed or dropped; the reader must not wait for room any more.
    closed: AtomicBool,
    /// `close` or drop: the reader ends at its next wake-up and stops draining. It holds a dup of
    /// the master fd, so closing ours does not end it.
    dropped: AtomicBool,
    log_bytes: AtomicU64,
}

/// What the subscriber gets: raw output chunks. Call [`PtyOutput::ack`] with the length of each
/// chunk once it was drawn or delivered; the reader stops at [`WINDOW`] unacknowledged bytes.
pub struct PtyOutput {
    rx: Receiver<Vec<u8>>,
    inner: Arc<Inner>,
}

impl PtyOutput {
    pub fn recv_timeout(&self, within: Duration) -> Option<Vec<u8>> {
        self.rx.recv_timeout(within).ok()
    }

    pub fn ack(&self, n: usize) {
        self.inner.window.release(n);
    }
}

impl Drop for PtyOutput {
    fn drop(&mut self) {
        *lock(&self.inner.sink) = None;
        self.inner.window.reset();
    }
}

pub struct Pty {
    pub id: String,
    pub pid: Option<u32>,
    pub log: PathBuf,
    /// What the sandbox enforces; `None` = ran unconfined (`unsandboxed_by_request`).
    pub sandbox: Option<SandboxCapabilities>,
    inner: Arc<Inner>,
    ctl: Arc<Ctl>,
    writer: Writer,
    child: Mutex<Box<dyn Child + Send + Sync>>,
}

impl Pty {
    /// True when no sandbox ran: also for `BestEffort` without a policy. Read `sandbox == None`
    /// as "unconfined", whatever the name says.
    pub fn unsandboxed_by_request(&self) -> bool {
        self.sandbox.is_none()
    }

    /// The log cap or the session quota was hit and the tree was killed.
    pub fn output_limit_exceeded(&self) -> bool {
        self.inner.output_limit.load(Ordering::Relaxed)
    }

    /// Bytes written to the log so far.
    pub fn log_bytes(&self) -> u64 {
        self.inner.log_bytes.load(Ordering::Relaxed)
    }

    /// Bytes the subscriber has not acknowledged yet.
    pub fn pending_bytes(&self) -> usize {
        *lock(&self.inner.window.used)
    }

    /// The reader reached the end of the output (the shell and everything on the slave is gone).
    pub fn output_ended(&self) -> bool {
        self.inner.eof.load(Ordering::Relaxed)
    }

    /// Exit code once the shell has exited (a signal death reports 1, like portable-pty).
    pub fn exit_code(&self) -> Option<u32> {
        self.ctl.try_wait(&mut **lock(&self.child))
    }

    /// Starts forwarding output (from now on) with flow control; replaces an earlier subscriber.
    pub fn subscribe(&self) -> PtyOutput {
        let (tx, rx) = mpsc::channel();
        self.inner.window.reset();
        *lock(&self.inner.sink) = Some(tx);
        PtyOutput {
            rx,
            inner: self.inner.clone(),
        }
    }

    /// Types into the PTY. Blocks while the child does not read its input; call it off an async
    /// executor thread.
    pub fn write_input(&self, data: &[u8]) -> Result<()> {
        if data.len() > MAX_INPUT {
            return Err(Error::InvalidSpec(format!(
                "pty input of {} bytes (max {MAX_INPUT})",
                data.len()
            )));
        }
        let mut w = lock(&self.writer);
        w.write_all(data)?;
        w.flush()?;
        Ok(())
    }

    pub fn resize(&self, cols: u16, rows: u16) -> Result<()> {
        check_size(cols, rows)?;
        lock(&self.ctl.master)
            .as_ref()
            .ok_or_else(|| io_err("pty closed"))?
            .resize(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(io_err)?;
        lock(&self.inner.screen).term.resize(Dims {
            cols: cols.into(),
            rows: rows.into(),
        });
        Ok(())
    }

    /// The last `max_lines` lines of scrollback plus screen as plain text (trailing blanks cut).
    pub fn screen_text(&self, max_lines: usize) -> String {
        let s = lock(&self.inner.screen);
        let g = s.term.grid();
        let total = g.history_size() + g.screen_lines();
        let take = max_lines.clamp(1, total);
        let bottom = g.screen_lines() as i32 - 1;
        let top = bottom - (take as i32 - 1);
        let text = s.term.bounds_to_string(
            Point::new(Line(top), Column(0)),
            Point::new(Line(bottom), g.last_column()),
        );
        let lines: Vec<&str> = text.lines().map(str::trim_end).collect();
        lines.join("\n").trim_end().to_string()
    }

    /// Kills the shell's whole tree now (SIGKILL to the group; `taskkill /T /F` on Windows).
    /// Blocks on Windows while `taskkill` runs: call it off an async executor thread.
    pub fn kill_tree(&self) {
        self.inner.closed.store(true, Ordering::Relaxed);
        self.ctl.kill_groups();
        let _ = lock(&self.child).kill();
    }

    /// Orderly close: hang up the terminal, give the shell 2 s, then [`Self::kill_tree`], then
    /// reap. The kill goes out BEFORE the reap, while the pid is still ours. Blocks up to 2 s
    /// (and on `taskkill` on Windows): call it off an async executor thread.
    pub fn close(&self) {
        self.inner.dropped.store(true, Ordering::Relaxed);
        #[cfg(unix)]
        if let Some(pid) = self.pid {
            use std::time::Instant;
            if !*lock(&self.ctl.reaped) {
                signal_group(pid, rustix::process::Signal::HUP);
            }
            let end = Instant::now() + Duration::from_secs(2);
            while Instant::now() < end && !self.ctl.exited_unreaped() {
                std::thread::sleep(Duration::from_millis(10));
            }
        }
        self.kill_tree();
        self.exit_code();
    }
}

impl Drop for Pty {
    /// Session close and runtime drop end here: kill now (before any reap), then reap off this
    /// thread until it succeeds, bounded at 60 s so a stuck child cannot pin a thread forever.
    fn drop(&mut self) {
        self.kill_tree();
        self.inner.dropped.store(true, Ordering::Relaxed);
        lock(&self.ctl.master).take(); // our fd; the reader's dup closes when it sees `dropped`
        let child = std::mem::replace(&mut self.child, Mutex::new(Box::new(Gone)));
        let mut child = child.into_inner().unwrap_or_else(|e| e.into_inner());
        let ctl = self.ctl.clone();
        let _ = std::thread::Builder::new()
            .name("synt-pty-reap".into())
            .spawn(move || {
                for _ in 0..6000 {
                    if ctl.try_wait(&mut *child).is_some() {
                        return;
                    }
                    std::thread::sleep(Duration::from_millis(10));
                }
            });
    }
}

/// Placeholder left in `Pty::child` after `Drop` moved the real child out.
#[derive(Debug)]
struct Gone;

impl portable_pty::ChildKiller for Gone {
    fn kill(&mut self) -> std::io::Result<()> {
        Ok(())
    }
    fn clone_killer(&self) -> Box<dyn portable_pty::ChildKiller + Send + Sync> {
        Box::new(Gone)
    }
}

impl Child for Gone {
    fn try_wait(&mut self) -> std::io::Result<Option<portable_pty::ExitStatus>> {
        Ok(None)
    }
    fn wait(&mut self) -> std::io::Result<portable_pty::ExitStatus> {
        Ok(portable_pty::ExitStatus::with_exit_code(0))
    }
    fn process_id(&self) -> Option<u32> {
        None
    }
    #[cfg(windows)]
    fn as_raw_handle(&self) -> Option<std::os::windows::io::RawHandle> {
        None
    }
}

#[cfg(unix)]
fn signal_group(pid: u32, sig: rustix::process::Signal) {
    if let Some(p) = i32::try_from(pid)
        .ok()
        .and_then(rustix::process::Pid::from_raw)
    {
        // ESRCH (group already gone) is the expected answer after a normal exit
        let _ = rustix::process::kill_process_group(p, sig);
    }
}

#[cfg(unix)]
fn kill_group(pid: u32) {
    signal_group(pid, rustix::process::Signal::KILL);
}

#[cfg(windows)]
fn kill_group(pid: u32) {
    let _ = std::process::Command::new(synthesis_platform::system32("taskkill.exe"))
        .args(["/T", "/F", "/PID"])
        .arg(pid.to_string())
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
}

/// The session env without anything a shell the agent can type into must not see: `SYNT_*`
/// (except the agent marker), secret-shaped names, loader and shell-startup injection (`LD_*`,
/// `DYLD_*`, `BASH_ENV`, `ENV`, `PROMPT_COMMAND`, `SHELLOPTS`, `BASHOPTS`, `PS4`); plus the
/// emulator's `TERM` and no shell history file.
fn scrub_env(mut vars: BTreeMap<String, String>) -> BTreeMap<String, String> {
    const INJECT: [&str; 7] = [
        "BASH_ENV",
        "ENV",
        "PROMPT_COMMAND",
        "SHELLOPTS",
        "BASHOPTS",
        "PS4",
        "HISTFILE",
    ];
    vars.retain(|k, _| {
        k == AGENT_MARKER
            || (!k.starts_with("SYNT_")
                && !synthesis_config::is_secret_shaped(k)
                && !k.starts_with("LD_")
                && !k.starts_with("DYLD_")
                && !INJECT.contains(&k.as_str()))
    });
    vars.insert("TERM".into(), "xterm-256color".into());
    vars.insert("COLORTERM".into(), "truecolor".into());
    // bash would otherwise write ~/.bash_history: what the agent and the person type stays out
    vars.insert("HISTFILE".into(), "/dev/null".into());
    vars.insert("HISTSIZE".into(), "0".into());
    vars
}

struct ReaderCtx {
    inner: Arc<Inner>,
    ctl: Arc<Ctl>,
    log: std::fs::File,
    max_log: u64,
    quota: Arc<crate::Quota>,
}

fn read_loop(mut rd: Box<dyn Read + Send>, mut cx: ReaderCtx) {
    let mut buf = vec![0u8; CHUNK];
    loop {
        cx.inner.window.wait_room(&cx.inner.closed);
        let n = match rd.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => n,
        };
        if cx.inner.dropped.load(Ordering::Relaxed) {
            break;
        }
        if cx.inner.output_limit.load(Ordering::Relaxed) {
            continue; // killed over the cap: only drain, a dying tty waits for its output
        }
        let chunk = &buf[..n];
        {
            let mut s = lock(&cx.inner.screen);
            let Screen { term, parser } = &mut *s;
            parser.advance(term, chunk);
        }
        let total = cx.inner.log_bytes.fetch_add(n as u64, Ordering::Relaxed) + n as u64;
        let used = cx.quota.used.fetch_add(n as u64, Ordering::Relaxed) + n as u64;
        if total > cx.max_log || used > cx.quota.max {
            // like Proc::enforce: the over-cap bytes are not written, the tree dies
            cx.inner.output_limit.store(true, Ordering::Relaxed);
            cx.inner.closed.store(true, Ordering::Relaxed);
            cx.ctl.kill_groups();
            continue;
        }
        let _ = cx.log.write_all(chunk);
        let mut sink = lock(&cx.inner.sink);
        if let Some(tx) = sink.as_ref() {
            cx.inner.window.add(n);
            if tx.send(chunk.to_vec()).is_err() {
                *sink = None;
                cx.inner.window.reset();
            }
        }
    }
    // release the scrollback (most of the Term's memory); the screen stays readable
    lock(&cx.inner.screen).term.grid_mut().clear_history();
    cx.inner.eof.store(true, Ordering::Relaxed);
}

impl TerminalRuntime {
    /// Starts an interactive shell on a PTY inside the session. `spec.sandbox` and `spec.env`
    /// come from trusted code only. Same gates as a `Bash` launch: the sandbox wrap and
    /// `SandboxMode` rules (nothing is spawned when it refuses), the session log quota and
    /// `Limits::max_live_procs`, plus at most [`MAX_PTYS`] registered PTYs (exited ones count
    /// until `close_pty`). Windows: the first call hardens the process-wide DLL search path.
    pub fn open_pty(&self, session_id: &str, spec: PtySpec) -> Result<Arc<Pty>> {
        check_size(spec.cols, spec.rows)?;
        #[cfg(windows)]
        {
            // portable-pty loads `conpty.dll` by bare name: without this a planted copy in the
            // cwd or on PATH would run inside the engine. Process-wide, once.
            static DLLS: std::sync::OnceLock<std::result::Result<(), String>> =
                std::sync::OnceLock::new();
            DLLS.get_or_init(synthesis_sandbox_win::harden_dll_search)
                .clone()
                .map_err(|e| {
                    Error::InvalidSpec(format!("cannot restrict the DLL search path: {e}"))
                })?;
        }
        let (ws, mut vars, session_policy, quota) = self.context(session_id)?;
        if quota.used.load(Ordering::Relaxed) >= quota.max {
            return Err(Error::LimitExceeded(format!(
                "session log quota of {} bytes used up",
                quota.max
            )));
        }
        let live_ptys = self.count_ptys(session_id)?;
        let live_procs = self
            .procs_of(session_id)?
            .iter()
            .filter(|p| p.poll().is_none())
            .count();
        if live_ptys >= MAX_PTYS || live_ptys + live_procs >= self.limits.max_live_procs {
            return Err(Error::LimitExceeded(format!(
                "{live_ptys} ptys and {live_procs} live processes in session; close one first"
            )));
        }
        vars.extend(spec.env);
        let cwd = match &spec.cwd {
            Some(c) => ws
                .resolve(&c.to_string_lossy())
                .map_err(|e| Error::InvalidSpec(format!("cwd: {e}")))?,
            None => ws.cwd.clone(),
        };
        if !cwd.is_dir() {
            return Err(Error::InvalidSpec(format!(
                "cwd {} is not a directory",
                cwd.display()
            )));
        }
        let args: Vec<OsString> = match self.shell.kind {
            ShellKind::Posix => vec!["-i".into()],
            ShellKind::Cmd => vec![],
        };
        let base = SandboxedCommand {
            program: self.shell.program.clone().into(),
            args,
            env: scrub_env(vars)
                .into_iter()
                .map(|(k, v)| (k.into(), v.into()))
                .collect(),
            cwd: Some(cwd),
            clear_env: true,
        };
        let policy = spec.sandbox.or(session_policy);
        let (cmd, sandbox) = match self.sandboxed(policy.as_ref(), &base)? {
            Some((w, caps)) => (w, Some(caps)),
            None => (base, None),
        };

        let id = format!("pty_{}", self.next_id.fetch_add(1, Ordering::Relaxed));
        let dir = ws.artifact_root.join("terminal");
        create_private_dir(&dir)?;
        let tag = std::hash::BuildHasher::hash_one(&std::hash::RandomState::new(), &id);
        let log = dir.join(format!("{id}-{tag:016x}.log"));
        let log_file = create_log(&log)?;

        let mut b = CommandBuilder::new(&cmd.program);
        // portable-pty starts from this process's environment; the child gets exactly `cmd.env`
        b.env_clear();
        b.args(&cmd.args);
        for (k, v) in &cmd.env {
            b.env(k, v);
        }
        if let Some(d) = &cmd.cwd {
            b.cwd(d);
        }
        let pair = native_pty_system()
            .openpty(PtySize {
                rows: spec.rows,
                cols: spec.cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(io_err)?;
        let child = pair.slave.spawn_command(b).map_err(io_err)?;
        drop(pair.slave); // else the reader never sees EOF
        let reader = pair.master.try_clone_reader().map_err(io_err)?;
        let writer: Writer = Arc::new(Mutex::new(pair.master.take_writer().map_err(io_err)?));

        let term = Term::new(
            Config {
                scrolling_history: SCROLLBACK,
                osc52: Osc52::Disabled,
                ..Config::default()
            },
            &Dims {
                cols: spec.cols.into(),
                rows: spec.rows.into(),
            },
            Reply(writer.clone()),
        );
        let inner = Arc::new(Inner {
            screen: Mutex::new(Screen {
                term,
                parser: Processor::default(),
            }),
            window: Window::default(),
            sink: Mutex::new(None),
            output_limit: AtomicBool::new(false),
            eof: AtomicBool::new(false),
            closed: AtomicBool::new(false),
            dropped: AtomicBool::new(false),
            log_bytes: AtomicU64::new(0),
        });
        let pid = child.process_id();
        let ctl = Arc::new(Ctl {
            pid,
            master: Mutex::new(Some(pair.master)),
            reaped: Mutex::new(false),
        });
        let cx = ReaderCtx {
            inner: inner.clone(),
            ctl: ctl.clone(),
            log: log_file,
            max_log: self.limits.max_log_bytes,
            quota,
        };
        let pty = Arc::new(Pty {
            id: id.clone(),
            pid,
            log,
            sandbox,
            inner,
            ctl,
            writer,
            child: Mutex::new(child),
        });
        if let Err(e) = std::thread::Builder::new()
            .name("synt-pty-read".into())
            .spawn(move || read_loop(reader, cx))
        {
            return Err(Error::Io(e)); // `pty` drops here and kills the shell
        }
        self.register_pty(session_id, pty.clone())?;
        Ok(pty)
    }

    pub fn pty(&self, session_id: &str, pty_id: &str) -> Result<Arc<Pty>> {
        let s = lock(&self.sessions);
        let s = s
            .get(session_id)
            .ok_or_else(|| Error::UnknownSession(session_id.into()))?;
        s.ptys
            .get(pty_id)
            .cloned()
            .ok_or_else(|| Error::UnknownProcess(pty_id.into()))
    }

    /// Orderly close (hang up, 2 s, kill) and forget the PTY. Blocks: use `spawn_blocking`.
    pub fn close_pty(&self, session_id: &str, pty_id: &str) -> Result<()> {
        let p = {
            let mut s = lock(&self.sessions);
            let s = s
                .get_mut(session_id)
                .ok_or_else(|| Error::UnknownSession(session_id.into()))?;
            s.ptys
                .remove(pty_id)
                .ok_or_else(|| Error::UnknownProcess(pty_id.into()))?
        };
        p.close();
        Ok(())
    }

    /// Registered PTYs, exited ones too: each keeps a screen until `close_pty`.
    fn count_ptys(&self, session_id: &str) -> Result<usize> {
        let s = lock(&self.sessions);
        let s = s
            .get(session_id)
            .ok_or_else(|| Error::UnknownSession(session_id.into()))?;
        Ok(s.ptys.len())
    }

    fn register_pty(&self, session_id: &str, pty: Arc<Pty>) -> Result<()> {
        let mut s = lock(&self.sessions);
        let s = s
            .get_mut(session_id)
            .ok_or_else(|| Error::UnknownSession(session_id.into()))?;
        // checked again under the lock so concurrent opens cannot pass the cap together (the
        // loser's `pty` drops and kills its shell)
        if s.ptys.len() >= MAX_PTYS {
            return Err(Error::LimitExceeded(format!("{MAX_PTYS} ptys in session")));
        }
        s.ptys.insert(pty.id.clone(), pty);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scrub_env_drops_synt_secret_shaped_and_startup_names() {
        let env = scrub_env(
            [
                "PATH",
                "SYNT_AGENT_SESSION",
                "SYNT_HOME",
                "SYNT_API_KEY",
                "GITHUB_TOKEN",
                "AWS_SECRET_ACCESS_KEY",
                "SSH_AUTH_SOCK",
                "BASH_ENV",
                "ENV",
                "LD_PRELOAD",
                "DYLD_INSERT_LIBRARIES",
                "PROMPT_COMMAND",
                "SHELLOPTS",
                "PS4",
                "HISTFILE",
                "TERM",
            ]
            .map(|k| (k.to_string(), "v".to_string()))
            .into(),
        );
        let names: Vec<&str> = env.keys().map(String::as_str).collect();
        assert_eq!(
            names,
            [
                "COLORTERM",
                "HISTFILE",
                "HISTSIZE",
                "PATH",
                "SYNT_AGENT_SESSION",
                "TERM"
            ],
            "{names:?}"
        );
        assert_eq!(env["TERM"], "xterm-256color");
        assert_eq!(env["HISTFILE"], "/dev/null");
    }

    #[test]
    fn a_reaped_shell_pid_is_never_signalled() {
        let ctl = |reaped| Ctl {
            pid: Some(4242),
            master: Mutex::new(None),
            reaped: Mutex::new(reaped),
        };
        let sent = std::cell::RefCell::new(vec![]);
        ctl(false).kill_groups_with(|p| sent.borrow_mut().push(p));
        assert_eq!(*sent.borrow(), [4242]);
        sent.borrow_mut().clear();
        ctl(true).kill_groups_with(|p| sent.borrow_mut().push(p));
        assert!(sent.borrow().is_empty(), "{:?}", sent.borrow());
    }

    #[test]
    fn size_limits() {
        assert!(check_size(1, 1).is_ok() && check_size(MAX_COLS, MAX_ROWS).is_ok());
        for (c, r) in [(0, 24), (80, 0), (MAX_COLS + 1, 24), (80, MAX_ROWS + 1)] {
            assert!(check_size(c, r).is_err(), "{c}x{r}");
        }
    }
}
