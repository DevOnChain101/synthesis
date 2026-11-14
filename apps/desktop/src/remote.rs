//! The remote engine: a child process that speaks `synt engine --stdio` (`synthesis_app::proto`).
//! The child is not trusted: lines are bounded, rate-limited and strictly parsed, text it sends is made
//! printable, an ask is only ever answered by the person (nothing here answers one), every wait has a
//! timeout, a silent engine is killed by a watchdog, its stderr is drained, capped and sanitized, and
//! the child is closed (stdin EOF, grace) and then killed when this value is dropped.

use std::collections::{HashMap, HashSet, VecDeque};
use std::ffi::OsString;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

use futures::channel::mpsc::UnboundedSender;
use serde_json::Value;
use synthesis_app::asks::{AskInput, Choice};
use synthesis_app::host::lock;
use synthesis_app::proto::{
    Call, Incoming, Out, Reply, Request, check_hello, parse_incoming, read_line,
};
use synthesis_config::Env;
use synthesis_harness::{UserReply, sanitize_for_display};
use synthesis_permissions::Mode;

use crate::engine::UiMsg;
use serde::Deserialize;
use synthesis_view::usage::HistoryDay;
use synthesis_view::workspaces::WorkspaceList;

const HELLO_TIMEOUT: Duration = Duration::from_secs(15);
/// `start` loads config, MCP servers and memory.
const START_TIMEOUT: Duration = Duration::from_secs(120);
/// `close` runs the SessionEnd hooks and closes the MCP servers.
const CLOSE_TIMEOUT: Duration = Duration::from_secs(30);
/// After stdin EOF a dropped engine gets this long to exit by itself before it is killed.
const DROP_GRACE: Duration = Duration::from_secs(3);
/// No line from an engine that owes one (a task runs, a call waits): killed after this.
pub const IDLE_TIMEOUT: Duration = Duration::from_secs(600);
/// Unreadable lines (and repeated hellos) tolerated before the engine is considered broken.
const MAX_BAD_LINES: u32 = 100;
/// Open asks tolerated at once.
const MAX_ASKS: usize = 64;
/// The most one second of engine output may carry; past it the engine is killed. This bounds how
/// fast a hostile engine can grow the window's queue (the queue itself has no length to read).
const MAX_LINES_PER_SEC: u32 = 5_000;
const MAX_BYTES_PER_SEC: usize = 32 << 20;
/// Requests waiting for the writer thread.
const WRITE_QUEUE: usize = 256;
/// A store-backed call (lists, preview, trust state): short.
const STORE_TIMEOUT: Duration = Duration::from_secs(15);
/// What a list answer may hold, however much the engine sends.
const MAX_WORKSPACES: usize = 200;
const MAX_SESSIONS: usize = 200;
const MAX_DAYS: usize = 400;
const MAX_PREVIEW_EVENTS: usize = 50_000;
/// stderr: longest line kept, and lines logged in all.
const STDERR_LINE: usize = 2048;
const STDERR_LINES: u32 = 200;
/// Last stderr lines kept to say why an engine ended.
const TAIL_LINES: usize = 5;

/// What the engine starts on besides the folder (checked by the engine: the same folder, a stored session, a model).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct StartSpec {
    pub resume: Option<String>,
    pub model: Option<String>,
}

/// How to start the engine process. The child gets exactly `env` (see [`allowed_env`]), nothing else.
#[derive(Clone, Debug)]
pub struct Launcher {
    pub program: OsString,
    pub args: Vec<OsString>,
    pub env: Vec<(String, String)>,
    /// See [`IDLE_TIMEOUT`].
    pub idle_timeout: Duration,
    /// How long `hello` may take (a cold WSL distro needs more than a local process).
    pub hello_timeout: Duration,
    /// The folder `start` names on the engine's side when it is not the host path (WSL: a Linux path).
    pub start_cwd: Option<String>,
}

impl Launcher {
    /// `env` is the caller's snapshot (`Env::from_process()` at the boundary); only [`allowed_env`] of it
    /// reaches the child.
    pub fn new(program: impl Into<OsString>, args: Vec<OsString>, env: &Env) -> Self {
        Self {
            program: program.into(),
            args,
            env: allowed_env(env),
            idle_timeout: IDLE_TIMEOUT,
            hello_timeout: HELLO_TIMEOUT,
            start_cwd: None,
        }
    }
}

/// What the engine process may inherit: the standard non-secret names (paths, locale, proxy, TLS roots,
/// temp, terminal), `XDG_*`, `LC_*` and `SYNT_*`. Everything else (`AWS_*`, `GITHUB_TOKEN`, ...) is dropped.
/// TODO(architect decision, not made here): how the provider API key reaches a remote engine. It is never
/// argv; today a `SYNT_*` key in `env` would pass through this list, which is only the transport of PR2.
pub fn allowed_env(env: &Env) -> Vec<(String, String)> {
    const NAMES: &[&str] = &[
        "PATH",
        "HOME",
        "USERPROFILE",
        "HTTP_PROXY",
        "HTTPS_PROXY",
        "ALL_PROXY",
        "NO_PROXY",
        "http_proxy",
        "https_proxy",
        "all_proxy",
        "no_proxy",
        "SSL_CERT_FILE",
        "SSL_CERT_DIR",
        "LANG",
        "TERM",
        "TMPDIR",
        "TEMP",
        "TMP",
        "SystemRoot",
    ];
    let same = |a: &str, b: &str| {
        if cfg!(windows) {
            a.eq_ignore_ascii_case(b)
        } else {
            a == b
        }
    };
    env.iter()
        .filter(|(k, _)| {
            NAMES.iter().any(|n| same(k, n))
                || ["LC_", "XDG_", "SYNT_"].iter().any(|p| k.starts_with(p))
        })
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

type Waiting = Arc<Mutex<HashMap<u64, mpsc::Sender<Result<Value, String>>>>>;

/// State the reader and watchdog threads share with the [`Remote`].
struct Shared {
    child: Mutex<Child>,
    waiting: Waiting,
    /// Id of the `send` whose result is not in yet (0: none): a refusal ends that task.
    send_id: AtomicU64,
    /// Id of the `set_mode` whose result is not in yet (0: none).
    mode_id: AtomicU64,
    mode: Mutex<Mode>,
    running: AtomicBool,
    closing: AtomicBool,
    /// The engine's stdout ended: nothing will answer any more.
    dead: AtomicBool,
    /// Asks the engine made and nobody answered or withdrew.
    asks: Mutex<HashSet<String>>,
    /// Milliseconds since `born` of the last line read or request written.
    last: AtomicU64,
    born: Instant,
    tx: Mutex<UnboundedSender<UiMsg>>,
    /// A restart is under way: the window of the next session is this channel, set when the call is made.
    /// Lines of the session being ended are dropped until the engine says `restarted` (or answers).
    discard: AtomicBool,
    /// The last sanitized stderr lines.
    tail: Mutex<VecDeque<String>>,
}

impl Shared {
    /// `msg` plus the engine's last stderr lines, when it said any.
    fn why(&self, msg: &str) -> String {
        let t = lock(&self.tail);
        if t.is_empty() {
            msg.into()
        } else {
            format!(
                "{msg}: {}",
                t.iter().cloned().collect::<Vec<_>>().join(" | ")
            )
        }
    }

    fn touch(&self) {
        let ms = u64::try_from(self.born.elapsed().as_millis()).unwrap_or(u64::MAX);
        self.last.store(ms, Ordering::SeqCst);
    }

    fn idle(&self) -> Duration {
        self.born
            .elapsed()
            .saturating_sub(Duration::from_millis(self.last.load(Ordering::SeqCst)))
    }

    fn tell(&self, m: UiMsg) {
        let _ = lock(&self.tx).unbounded_send(m);
    }

    fn kill(&self) {
        let mut c = lock(&self.child);
        let _ = c.kill();
        let _ = c.wait();
    }
}

/// Why a restart did not give a new session: `kept` (the engine refused before ending anything: the session
/// goes on) or not (the engine is killed).
#[derive(Debug)]
pub struct RestartError {
    pub kept: bool,
    pub why: String,
}

/// `(closed, held, failed)` of a `workspace.close`.
pub type ClosedIds = (Vec<String>, Vec<String>, Vec<(String, String)>);

pub struct Remote {
    shared: Arc<Shared>,
    /// To the writer thread; dropping it closes the engine's stdin (it then shuts down).
    lines: Mutex<Option<mpsc::SyncSender<Vec<u8>>>>,
    next: AtomicU64,
}

impl Remote {
    /// Starts the process, checks `hello` (exact version) and starts the session in `project` (a path on the
    /// engine's own OS). Blocking. Returns the engine's start report.
    pub fn connect(
        l: &Launcher,
        project: &Path,
        tx: UnboundedSender<UiMsg>,
        spec: &StartSpec,
    ) -> Result<(Self, Value), String> {
        let cwd = match &l.start_cwd {
            Some(c) => c.clone(),
            None => project
                .to_str()
                .ok_or("the project path is not valid text")?
                .to_string(),
        };
        let mut cmd = Command::new(&l.program);
        cmd.args(&l.args)
            .env_clear()
            .envs(l.env.iter().map(|(k, v)| (k, v)))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        #[cfg(windows)]
        std::os::windows::process::CommandExt::creation_flags(&mut cmd, 0x0800_0000); // no console window
        let mut child = cmd
            .spawn()
            .map_err(|e| format!("cannot start the engine: {e}"))?;
        let (Some(mut stdin), Some(stdout), Some(stderr)) =
            (child.stdin.take(), child.stdout.take(), child.stderr.take())
        else {
            let _ = child.kill();
            return Err("the engine has no pipes".into());
        };
        let shared = Arc::new(Shared {
            child: Mutex::new(child),
            waiting: Waiting::default(),
            send_id: AtomicU64::new(0),
            mode_id: AtomicU64::new(0),
            mode: Mutex::new(Mode::Default),
            running: AtomicBool::new(false),
            closing: AtomicBool::new(false),
            dead: AtomicBool::new(false),
            asks: Mutex::default(),
            last: AtomicU64::new(0),
            born: Instant::now(),
            tx: Mutex::new(tx),
            discard: AtomicBool::new(false),
            tail: Mutex::default(),
        });
        let (line_tx, line_rx) = mpsc::sync_channel::<Vec<u8>>(WRITE_QUEUE);
        // from here a failure drops `remote`, which closes and then kills the child
        let remote = Self {
            shared: shared.clone(),
            lines: Mutex::new(Some(line_tx)),
            next: AtomicU64::new(1),
        };
        std::thread::spawn(move || {
            for mut line in line_rx {
                line.push(b'\n');
                if stdin.write_all(&line).and_then(|()| stdin.flush()).is_err() {
                    break;
                }
            }
        });
        let s = shared.clone();
        std::thread::spawn(move || drain_stderr(stderr, &s));
        let (hello_tx, hello_rx) = mpsc::channel();
        let s = shared.clone();
        std::thread::spawn(move || read_loop(BufReader::new(stdout), &s, &hello_tx));
        let idle = l.idle_timeout;
        std::thread::spawn(move || watchdog(&shared, idle));
        let why = |e: String| remote.shared.why(&e);
        hello_rx
            .recv_timeout(l.hello_timeout)
            .map_err(|_| why("the engine did not say hello in time".into()))?
            .map_err(why)?;
        let v = remote
            .call(
                Call::Start {
                    cwd,
                    resume: spec.resume.clone(),
                    model: spec.model.clone(),
                },
                START_TIMEOUT,
            )
            .map_err(why)?;
        // an engine that does not say its mode is not trusted to be in the default one
        let mode = v["mode"]
            .as_str()
            .and_then(Mode::parse)
            .ok_or("the engine did not report its permission mode")?;
        *lock(&remote.shared.mode) = mode;
        Ok((remote, v))
    }

    fn write(&self, req: &Request) -> Result<(), String> {
        let line = serde_json::to_vec(req).map_err(|e| e.to_string())?;
        self.shared.touch();
        match lock(&self.lines)
            .as_ref()
            .ok_or("the engine is closed")?
            .try_send(line)
        {
            Ok(()) => Ok(()),
            Err(mpsc::TrySendError::Full(_)) => Err("the engine is not reading its input".into()),
            Err(mpsc::TrySendError::Disconnected(_)) => Err("the engine process ended".into()),
        }
    }

    fn call(&self, call: Call, wait: Duration) -> Result<Value, String> {
        let id = self.next.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = mpsc::channel();
        lock(&self.shared.waiting).insert(id, tx);
        // after the insert: the reader clears `waiting` once and then sets `dead`, so a call that
        // registered too late sees `dead` here instead of waiting out its timeout
        let r = if self.shared.dead.load(Ordering::SeqCst) {
            Err("the engine process ended".to_string())
        } else {
            self.write(&Request { id, call }).and_then(|()| {
                rx.recv_timeout(wait).map_err(|e| match e {
                    mpsc::RecvTimeoutError::Timeout => {
                        "the engine did not answer in time".to_string()
                    }
                    mpsc::RecvTimeoutError::Disconnected => "the engine process ended".to_string(),
                })?
            })
        };
        lock(&self.shared.waiting).remove(&id);
        r
    }

    /// A call of the store-backed methods: bounded wait, the answer parsed by the caller.
    fn store_call(&self, call: Call) -> Result<Value, String> {
        self.call(call, STORE_TIMEOUT)
    }

    /// The workspaces of the engine's store (its own machine's), bounded and made printable.
    pub fn workspaces(&self) -> Result<WorkspaceList, String> {
        let v = self.store_call(Call::WorkspacesList)?;
        let mut l: WorkspaceList = serde_json::from_value(v["workspaces"].clone())
            .map(|workspaces| WorkspaceList { workspaces })
            .map_err(|e| format!("unreadable workspace list: {e}"))?;
        l.workspaces.truncate(MAX_WORKSPACES);
        for w in &mut l.workspaces {
            w.sessions.truncate(MAX_SESSIONS);
            (w.root, w.key, w.label) = (clean(&w.root), clean(&w.key), clean(&w.label));
            for s in &mut w.sessions {
                (s.id, s.title, s.updated_at) =
                    (clean(&s.id), clean(&s.title), clean(&s.updated_at));
            }
        }
        Ok(l)
    }

    pub fn usage(&self, tz_min: i32) -> Result<Vec<HistoryDay>, String> {
        let v = self.store_call(Call::UsageSummary { tz_min })?;
        let mut d: Vec<HistoryDay> = serde_json::from_value(v["days"].clone())
            .map_err(|e| format!("unreadable usage: {e}"))?;
        d.truncate(MAX_DAYS);
        Ok(d)
    }

    /// The stored conversation of `id`, read-only, as events (unreadable ones are skipped, the count is bounded).
    pub fn preview(&self, id: &str) -> Result<Vec<synthesis_events::Envelope>, String> {
        let v = self.store_call(Call::SessionPreview { id: id.into() })?;
        Ok(v["events"]
            .as_array()
            .map(|a| {
                a.iter()
                    .take(MAX_PREVIEW_EVENTS)
                    .filter_map(|e| serde_json::from_value(e.clone()).ok())
                    .collect()
            })
            .unwrap_or_default())
    }

    pub fn trust_state(&self) -> Result<TrustState, String> {
        TrustState::parse(self.store_call(Call::TrustState)?)
    }

    pub fn trust_revoke(&self, root: &str) -> Result<bool, String> {
        let v = self.store_call(Call::TrustRevoke { root: root.into() })?;
        Ok(v["revoked"].as_bool().unwrap_or(false)
            || v["session_grant_ended"].as_bool().unwrap_or(false))
    }

    pub fn rename(&self, id: &str, title: &str) -> Result<String, String> {
        let v = self.store_call(Call::SessionRename {
            id: id.into(),
            title: title.into(),
        })?;
        Ok(v["title"].as_str().map(clean).unwrap_or_default())
    }

    pub fn clear(&self) -> Result<(), String> {
        self.store_call(Call::Clear).map(|_| ())
    }

    /// `Ok(true)`: the harness compacts at its next safe point; `Ok(false)`: now, `UiMsg::Compacted` follows.
    pub fn compact(&self, instructions: Option<String>) -> Result<bool, String> {
        let v = self.store_call(Call::Compact { instructions })?;
        Ok(v["started"].as_bool().unwrap_or(false))
    }

    /// `workspace.close` of a folder other than the engine's own: `(closed, held, failed)`.
    pub fn close_other_workspace(&self, key: &str) -> Result<ClosedIds, String> {
        let v = self.store_call(Call::WorkspaceClose {
            workspace: key.into(),
        })?;
        let ids = |k: &str| -> Vec<String> {
            v[k].as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(Value::as_str)
                        .map(clean)
                        .take(MAX_SESSIONS)
                        .collect()
                })
                .unwrap_or_default()
        };
        let failed = v["failed"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|p| Some((clean(p[0].as_str()?), clean(p[1].as_str()?))))
                    .take(MAX_SESSIONS)
                    .collect()
            })
            .unwrap_or_default();
        Ok((ids("closed"), ids("held"), failed))
    }

    /// A call that ends the session and starts another in this same process (`trust.set`, `model.set`,
    /// `session.*`, `workspace.close` of the own folder). From the moment it is made the events go to `tx`
    /// (the next session's window); the old session's trailing lines are dropped. `Ok`: the start report of
    /// the new session. An `Err` leaves the session running (nothing was ended) or, when the engine could not
    /// continue at all, a dead engine (the caller sees it as ended).
    pub fn restart(&self, call: Call, tx: UnboundedSender<UiMsg>) -> Result<Value, RestartError> {
        debug_assert!(call.is_restart());
        let refused = |why: &str| RestartError {
            kept: true,
            why: why.into(),
        };
        if !lock(&self.shared.asks).is_empty() || self.running() {
            return Err(refused(
                "busy: finish or cancel the task and answer the open dialogs first",
            ));
        }
        *lock(&self.shared.tx) = tx;
        self.shared.discard.store(true, Ordering::SeqCst);
        let r = self.call(call, START_TIMEOUT);
        self.shared.discard.store(false, Ordering::SeqCst);
        // fail closed: only an error the engine marked as a refusal BEFORE it ended anything leaves the session
        // as it was; any other (a fatal restart, a timeout, an unmarked error, a dead process) ends this engine
        let dead = |why: String| {
            self.shared.kill();
            RestartError { kept: false, why }
        };
        let v = match r {
            Ok(v) => v,
            Err(e) if e.starts_with(synthesis_app::proto::REFUSED) => {
                return Err(refused(&e[synthesis_app::proto::REFUSED.len()..]));
            }
            Err(e) => return Err(dead(e)),
        };
        // an engine that does not say its mode is not trusted to be in the default one
        let Some(mode) = v["mode"].as_str().and_then(Mode::parse) else {
            return Err(dead("the engine did not report its permission mode".into()));
        };
        *lock(&self.shared.mode) = mode;
        Ok(v)
    }

    pub fn is_dead(&self) -> bool {
        self.shared.dead.load(Ordering::SeqCst)
    }

    pub fn running(&self) -> bool {
        self.shared.running.load(Ordering::SeqCst)
    }

    /// The permission mode the engine reported (at start, and after each accepted change).
    pub fn mode(&self) -> Mode {
        *lock(&self.shared.mode)
    }

    /// Asks the engine to change mode (never to bypassPermissions); the shown mode changes when it says yes.
    pub fn set_mode(&self, mode: Mode) -> Result<(), String> {
        if mode == Mode::BypassPermissions {
            return Err("bypassPermissions cannot be entered in remote mode".into());
        }
        let id = self.next.fetch_add(1, Ordering::SeqCst);
        self.shared.mode_id.store(id, Ordering::SeqCst);
        self.write(&Request {
            id,
            call: Call::SetMode {
                mode: mode.as_str().into(),
            },
        })
    }

    /// Starts a task; `UiMsg::TaskDone` follows. `Err`: one is already running or the engine is gone.
    pub fn send(&self, text: String) -> Result<(), &'static str> {
        if self.shared.running.swap(true, Ordering::SeqCst) {
            return Err("a task is already running");
        }
        self.write_send(text, vec![])
    }

    /// [`Remote::send`] with images the engine holds (`image_put` made them). A worker thread's call: the
    /// upload waits for the engine. A failed upload ends the task like any other refusal.
    pub fn send_images(
        &self,
        text: String,
        images: &[synthesis_harness::ImageAttachment],
    ) -> Result<(), &'static str> {
        if images.is_empty() {
            return self.send(text);
        }
        if self.shared.running.swap(true, Ordering::SeqCst) {
            return Err("a task is already running");
        }
        let mut ids = vec![];
        for i in images {
            match self.image_put(&i.media_type, &i.data_base64) {
                Ok(id) => ids.push(id),
                Err(e) => {
                    for id in ids {
                        let _ = self.store_call(Call::ImageDrop { id });
                    }
                    self.shared.running.store(false, Ordering::SeqCst);
                    self.shared
                        .tell(UiMsg::TaskDone(Err(format!("image not sent: {e}"))));
                    return Ok(());
                }
            }
        }
        self.write_send(text, ids)
    }

    fn write_send(&self, text: String, images: Vec<String>) -> Result<(), &'static str> {
        let id = self.next.fetch_add(1, Ordering::SeqCst);
        self.shared.send_id.store(id, Ordering::SeqCst);
        let sent = self.write(&Request {
            id,
            call: Call::Send { text, images },
        });
        // `dead` is re-read after `running` is set: the reader's last act cannot miss this task
        if sent.is_err() || self.shared.dead.load(Ordering::SeqCst) {
            self.shared.running.store(false, Ordering::SeqCst);
            return Err("the engine process ended");
        }
        Ok(())
    }

    pub fn cancel(&self) {
        let id = self.next.fetch_add(1, Ordering::SeqCst);
        let _ = self.write(&Request {
            id,
            call: Call::Cancel,
        });
    }

    /// False when the engine has no such ask waiting.
    pub fn answer(&self, id: &str, choice: Choice) -> bool {
        self.reply(id, |id| Call::Answer { id, choice })
    }

    pub fn answer_input(&self, id: &str, reply: Option<UserReply>) -> bool {
        let reply = reply.map(|r| match r {
            UserReply::Answers(v) => Reply::Answers(v),
            UserReply::Plan {
                approved,
                message,
                mode,
            } => Reply::Plan {
                approved,
                message,
                mode: mode.map(|m| m.as_str().to_string()),
            },
        });
        self.reply(id, |id| Call::AnswerInput { id, reply })
    }

    fn reply(&self, id: &str, call: impl FnOnce(String) -> Call) -> bool {
        if !lock(&self.shared.asks).remove(id) {
            return false;
        }
        let rid = self.next.fetch_add(1, Ordering::SeqCst);
        self.write(&Request {
            id: rid,
            call: call(id.to_string()),
        })
        .is_ok()
    }

    /// Asks the engine to end the session and waits for its report (the hooks and MCP servers take time);
    /// the drop then closes stdin, gives it a grace period and kills what is left.
    pub fn close(self) -> Option<String> {
        self.shared.closing.store(true, Ordering::SeqCst);
        self.call(Call::Close, CLOSE_TIMEOUT).ok()?["worktree"]
            .as_str()
            .map(sanitize_for_display)
    }
}

impl Drop for Remote {
    fn drop(&mut self) {
        self.shared.closing.store(true, Ordering::SeqCst);
        lock(&self.lines).take(); // stdin EOF: the engine ends its session by itself
        let t0 = Instant::now();
        while t0.elapsed() < DROP_GRACE {
            if matches!(lock(&self.shared.child).try_wait(), Ok(Some(_))) {
                return;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        self.shared.kill();
    }
}

/// Logs the engine's stderr: line length and line count capped, made printable, never forwarded raw.
fn drain_stderr(r: std::process::ChildStderr, s: &Shared) {
    let mut r = BufReader::new(r);
    let (mut logged, mut buf) = (0, vec![]);
    loop {
        buf.clear();
        match r
            .by_ref()
            .take(STDERR_LINE as u64)
            .read_until(b'\n', &mut buf)
        {
            Ok(0) | Err(_) => return,
            Ok(_) => {}
        }
        if buf.last() != Some(&b'\n') && buf.len() == STDERR_LINE {
            // a longer line: the rest of it is dropped
            let mut rest = vec![];
            while !matches!(r.read_until(b'\n', &mut rest), Ok(0) | Err(_))
                && rest.len() < (1 << 20)
            {
                if rest.last() == Some(&b'\n') {
                    break;
                }
            }
        }
        // wsl.exe's own messages are UTF-16LE: dropping the NULs keeps their ASCII text readable
        buf.retain(|b| *b != 0);
        let text = sanitize_for_display(String::from_utf8_lossy(&buf).trim_end());
        if logged < STDERR_LINES {
            logged += 1;
            eprintln!("synthesis-desktop: engine: {text}");
        }
        let mut t = lock(&s.tail);
        if t.len() == TAIL_LINES {
            t.pop_front();
        }
        t.push_back(
            synthesis_view::toolview::redact_text(&text)
                .chars()
                .take(200)
                .collect(),
        );
    }
}

/// Kills an engine that owes a line (a task runs, a call waits) and has been silent for `idle`.
fn watchdog(s: &Arc<Shared>, idle: Duration) {
    while !s.dead.load(Ordering::SeqCst) {
        std::thread::sleep(Duration::from_millis(250));
        let owes = s.running.load(Ordering::SeqCst) || !lock(&s.waiting).is_empty();
        if owes && !s.closing.load(Ordering::SeqCst) && s.idle() > idle {
            s.tell(UiMsg::BadEvent("the engine stopped answering".into()));
            s.kill();
            return;
        }
    }
}

/// Reads the engine's stdout until it ends or breaks; then kills the child (a stream that broke cannot be
/// trusted) and tells the window.
fn read_loop(
    mut r: BufReader<std::process::ChildStdout>,
    s: &Arc<Shared>,
    hello: &mpsc::Sender<Result<(), String>>,
) {
    let first = match read_line(&mut r) {
        Ok(Some(l)) => check_hello(&l),
        Ok(None) => Err("the engine exited before hello".into()),
        Err(e) => Err(format!("the engine's first line: {e}")),
    };
    let ok = first.is_ok();
    let _ = hello.send(first);
    let (mut bad, mut window, mut lines, mut bytes) = (0, Instant::now(), 0u32, 0usize);
    loop {
        if !ok {
            break;
        }
        let line = match read_line(&mut r) {
            Ok(Some(line)) => line,
            Ok(None) | Err(_) => break,
        };
        s.touch();
        if window.elapsed() >= Duration::from_secs(1) {
            (window, lines, bytes) = (Instant::now(), 0, 0);
        }
        lines += 1;
        bytes += line.len();
        if lines > MAX_LINES_PER_SEC || bytes > MAX_BYTES_PER_SEC {
            s.tell(UiMsg::BadEvent("the engine sent too much too fast".into()));
            break;
        }
        let verdict = match parse_incoming(&line) {
            Ok(m) => dispatch(s, m),
            Err(e) => {
                s.tell(UiMsg::BadEvent(sanitize_for_display(&e)));
                Verdict::Bad
            }
        };
        match verdict {
            Verdict::Fine => {}
            Verdict::Bad => {
                bad += 1;
                if bad > MAX_BAD_LINES {
                    break;
                }
            }
            Verdict::Hostile(why) => {
                s.tell(UiMsg::BadEvent(why.into()));
                break;
            }
        }
    }
    s.kill();
    lock(&s.waiting).clear(); // every waiting call sees the process ended
    s.dead.store(true, Ordering::SeqCst);
    if ok && !s.closing.load(Ordering::SeqCst) {
        let m = s.why("the engine process ended");
        s.tell(UiMsg::BadEvent(m.clone()));
        if s.running.swap(false, Ordering::SeqCst) {
            s.tell(UiMsg::TaskDone(Err(m)));
        }
    }
}

enum Verdict {
    Fine,
    /// Counts toward [`MAX_BAD_LINES`].
    Bad,
    /// The engine is killed.
    Hostile(&'static str),
}

const TOO_MANY_ASKS: &str = "the engine opened too many asks";

/// Records an ask the person has to answer; false past [`MAX_ASKS`].
fn open_ask(s: &Shared, id: &str) -> bool {
    let mut asks = lock(&s.asks);
    asks.len() < MAX_ASKS && {
        asks.insert(id.to_string());
        true
    }
}

fn dispatch(s: &Shared, m: Incoming) -> Verdict {
    let discarding = s.discard.load(Ordering::SeqCst);
    let m = match m {
        Incoming::Event(e) => {
            if !discarding {
                s.tell(UiMsg::Event(e));
            }
            return Verdict::Fine;
        }
        Incoming::Msg(m) => *m,
    };
    // while a restart is under way every line of the session being ended is dropped (task ends, asks, notices,
    // compaction results): only the marker and the answer to the call are of this moment
    if discarding && !matches!(m, Out::Restarted | Out::Result { .. } | Out::Hello { .. }) {
        return Verdict::Fine;
    }
    match m {
        Out::Hello { .. } => {
            s.tell(UiMsg::BadEvent("unexpected hello".into()));
            return Verdict::Bad;
        }
        // only while a restart that this side asked for is under way: the next lines are the new session's
        Out::Restarted => {
            if !s.discard.swap(false, Ordering::SeqCst) {
                return Verdict::Hostile("the engine restarted without being asked");
            }
            lock(&s.asks).clear();
        }
        Out::Compacted { error } => s.tell(UiMsg::Compacted(
            error.map_or(Ok(()), |e| Err(sanitize_for_display(&e))),
        )),
        Out::Notice { text } => s.tell(UiMsg::BadEvent(sanitize_for_display(&text))),
        Out::Result { id, ok, error } => {
            if let Some(w) = lock(&s.waiting).remove(&id) {
                let _ = w.send(match error {
                    Some(e) => Err(sanitize_for_display(&e)),
                    None => Ok(ok.unwrap_or(Value::Null)),
                });
            } else if id == s.send_id.load(Ordering::SeqCst)
                && let Some(e) = error
            {
                let e = sanitize_for_display(&e);
                // a refused duplicate says nothing about the task that is running
                if e.starts_with(synthesis_app::host::ALREADY_RUNNING) {
                    s.tell(UiMsg::BadEvent(e));
                } else {
                    s.running.store(false, Ordering::SeqCst);
                    s.tell(UiMsg::TaskDone(Err(e)));
                }
            } else if id == s.mode_id.load(Ordering::SeqCst) {
                match (error, ok.as_ref().and_then(|o| o["mode"].as_str())) {
                    (Some(e), _) => s.tell(UiMsg::BadEvent(sanitize_for_display(&e))),
                    (None, Some(m)) => {
                        if let Some(m) = Mode::parse(m) {
                            *lock(&s.mode) = m;
                        }
                    }
                    _ => {}
                }
            }
        }
        m @ Out::Approval { .. } => {
            if let Some(a) = m.into_approval() {
                if !open_ask(s, &a.id) {
                    return Verdict::Hostile(TOO_MANY_ASKS);
                }
                s.tell(UiMsg::Approval(Box::new(a)));
            }
        }
        Out::Input { id, req } => {
            if !open_ask(s, &id) {
                return Verdict::Hostile(TOO_MANY_ASKS);
            }
            s.tell(UiMsg::Input(Box::new(AskInput { id, req })));
        }
        Out::Withdraw { id } => {
            lock(&s.asks).remove(&id);
            s.tell(UiMsg::Withdraw(id));
        }
        Out::TaskDone { error } => {
            s.running.store(false, Ordering::SeqCst);
            s.tell(UiMsg::TaskDone(
                error.map_or(Ok(()), |e| Err(sanitize_for_display(&e))),
            ));
        }
    }
    Verdict::Fine
}

fn clean(s: &str) -> String {
    sanitize_for_display(s)
}

// ---- files, images, rewind, clean, memory and settings (protocol 3). The engine does the work on its own
// machine; what it answers is bounded and made printable again here (it is not trusted), and a path in an
// answer is display text, never something this side opens.

/// A scan, a doctor run or a clean plan can take a while on the engine's side.
const SLOW_TIMEOUT: Duration = Duration::from_secs(30);
/// A rewind or a deletion is not cut short: the engine finishes it (and says so).
const APPLY_TIMEOUT: Duration = Duration::from_secs(300);
const MAX_TREE_ROWS: usize = 5000;
const MAX_TREE_PAGES: usize = 30;
const MAX_LINES: usize = 2000;
const MAX_RECORDS: usize = 100;
const MAX_ENTRIES: usize = 2000;
/// Base64 characters per `image.put` chunk (a multiple of 4: a chunk is whole 3-byte groups).
const IMAGE_CHUNK: usize = 400_000;

fn text(v: &Value) -> String {
    clean(v.as_str().unwrap_or_default())
}

fn rows_of(v: &Value, key: &str, max: usize) -> Result<Vec<Value>, String> {
    Ok(v[key]
        .as_array()
        .ok_or_else(|| format!("unreadable answer: no {key}"))?
        .iter()
        .take(max)
        .cloned()
        .collect())
}

impl Remote {
    /// The project tree of the session folder, as the engine scanned it (git in a trusted folder, a plain walk
    /// otherwise: the engine decides). A row that could corrupt a path (a `/` in a name) is dropped.
    pub fn fs_list(&self) -> Result<synthesis_view::project::ProjectData, String> {
        use synthesis_view::project::{ProjectData, TreeRow};
        let mut data = ProjectData {
            loaded: true,
            ..Default::default()
        };
        let mut offset = 0u64;
        for _ in 0..MAX_TREE_PAGES {
            let call = || {
                self.call(
                    Call::FsList {
                        dir: String::new(),
                        offset: offset as u32,
                    },
                    SLOW_TIMEOUT,
                )
            };
            let mut v = call();
            // scans are a few a second: one short wait, once
            if offset == 0 && v.as_ref().is_err_and(|e| e.contains("too many requests")) {
                std::thread::sleep(Duration::from_millis(350));
                v = call();
            }
            let v = v?;
            for r in rows_of(&v, "rows", MAX_TREE_ROWS)? {
                let name = text(&r[1]);
                if data.rows.len() >= MAX_TREE_ROWS || name.is_empty() || name.contains('/') {
                    continue;
                }
                data.rows.push(TreeRow {
                    depth: u16::try_from(r[0].as_u64().unwrap_or(0))
                        .unwrap_or(0)
                        .min(64),
                    name,
                    dir: r[2].as_bool().unwrap_or(false),
                    status: r[3]
                        .as_str()
                        .and_then(|s| s.chars().next())
                        .filter(|c| "MADR?".contains(*c)),
                });
            }
            data.note = v["note"].as_str().map(clean);
            data.truncated |= v["truncated"].as_bool().unwrap_or(false);
            match v["next"].as_u64() {
                Some(n) if data.rows.len() < MAX_TREE_ROWS => offset = n,
                _ => return Ok(data),
            }
        }
        data.truncated = true;
        Ok(data)
    }

    /// A text preview of `rel` (a path of the tree): `(lines, size, cut)`. The engine refuses secrets, links,
    /// binary files and anything outside the folder; the path is sent as the tree listed it.
    pub fn fs_read(&self, rel: &str) -> Result<(Vec<String>, u64, bool), String> {
        let v = self.call(Call::FsRead { path: rel.into() }, STORE_TIMEOUT)?;
        let lines = v["lines"]
            .as_array()
            .ok_or("unreadable file preview")?
            .iter()
            .take(MAX_LINES)
            .map(text)
            .collect();
        Ok((
            lines,
            v["size"].as_u64().unwrap_or(0),
            v["cut"].as_bool().unwrap_or(false),
        ))
    }

    /// Hands one image (`data_base64`, the whole file) to the engine in bounded chunks; the id it issues is what
    /// `send` names.
    pub fn image_put(&self, mime: &str, data_base64: &str) -> Result<String, String> {
        let pad = data_base64.bytes().rev().take_while(|b| *b == b'=').count();
        let size = (data_base64.len() / 4 * 3).saturating_sub(pad) as u64;
        let chunks: Vec<&str> = data_base64
            .as_bytes()
            .chunks(IMAGE_CHUNK)
            .map(|c| std::str::from_utf8(c).unwrap_or_default())
            .collect();
        let mut upload: Option<String> = None;
        for (i, c) in chunks.iter().enumerate() {
            let last = i + 1 == chunks.len();
            let v = self.store_call(Call::ImagePut {
                upload: upload.clone(),
                seq: i as u32,
                mime: (i == 0).then(|| mime.to_string()),
                size: (i == 0).then_some(size),
                data: (*c).to_string(),
                last,
            })?;
            if last {
                return v["id"]
                    .as_str()
                    .map(clean)
                    .ok_or_else(|| "the engine did not name the image".into());
            }
            upload = v["upload"].as_str().map(clean);
        }
        Err("there is no image data".into())
    }

    pub fn image_drop(&self, id: &str) {
        let _ = self.store_call(Call::ImageDrop { id: id.into() });
    }

    pub fn rewind_list(&self) -> Result<Vec<crate::rewind::CheckpointRow>, String> {
        let v = self.store_call(Call::RewindList)?;
        Ok(rows_of(&v, "checkpoints", 500)?
            .iter()
            .map(|c| crate::rewind::CheckpointRow {
                name: text(&c["name"]),
                when: text(&c["when"]),
                after_message: c["after_message"].as_u64().unwrap_or(0),
                files: c["files"]
                    .as_array()
                    .map(|a| a.iter().take(200).map(text).collect())
                    .unwrap_or_default(),
            })
            .collect())
    }

    /// The dry run. The entries are display rows (the paths are the engine's own text); `token` is what
    /// applies exactly this.
    pub fn rewind_dry_run(
        &self,
        checkpoint: Option<String>,
    ) -> Result<crate::rewind::Preview, String> {
        use synthesis_app::rewind::{Entry, Step};
        let v = self.call(
            Call::RewindDryRun {
                checkpoint: checkpoint.clone(),
            },
            SLOW_TIMEOUT,
        )?;
        let token = v["token"]
            .as_str()
            .map(clean)
            .ok_or("the engine issued no token")?;
        let entries = rows_of(&v, "entries", MAX_ENTRIES)?
            .iter()
            .map(|e| Entry {
                path: std::path::PathBuf::new(),
                shown: text(&e["shown"]),
                step: match e["kind"].as_str() {
                    Some("restore") => Step::Restore {
                        artifact_id: String::new(),
                        sha256: None,
                    },
                    Some("remove") => Step::Remove,
                    _ => Step::Skip {
                        reason: text(&e["reason"]),
                        forceable: false,
                    },
                },
                size: e["size"].as_u64().unwrap_or(0),
                state: String::new(),
            })
            .collect();
        Ok(crate::rewind::Preview {
            checkpoint,
            entries,
            token: Some(token),
        })
    }

    pub fn rewind_apply(&self, token: &str) -> Result<crate::rewind::Outcome, String> {
        let v = self.call(
            Call::RewindApply {
                token: token.into(),
            },
            APPLY_TIMEOUT,
        )?;
        let n = |k: &str| usize::try_from(v[k].as_u64().unwrap_or(0)).unwrap_or(0);
        Ok(crate::rewind::Outcome {
            restored: n("restored"),
            removed: n("removed"),
            skipped: n("skipped"),
            failed: rows_of(&v, "failed", 100)?.iter().map(text).collect(),
            changed: vec![],
        })
    }

    /// The three scopes with counts; each plan's `ids` holds the ONE token the engine issued for it.
    pub fn clean_plan(
        &self,
    ) -> Result<Vec<(synthesis_session::CleanScope, synthesis_session::CleanPlan)>, String> {
        use synthesis_session::{CleanCounts, CleanPlan, CleanScope};
        let v = self.call(Call::CleanPlan, SLOW_TIMEOUT)?;
        rows_of(&v, "plans", 3)?
            .iter()
            .map(|p| {
                let scope = match p["scope"].as_str() {
                    Some("ClosedEmpty") => CleanScope::ClosedEmpty,
                    Some("Inactive") => CleanScope::Inactive,
                    Some("All") => CleanScope::All,
                    _ => return Err("unreadable clean plan".to_string()),
                };
                let c = &p["counts"];
                let n = |k: &str| c[k].as_u64().unwrap_or(0);
                let u = |k: &str| usize::try_from(n(k)).unwrap_or(0);
                Ok((
                    scope,
                    CleanPlan {
                        ids: vec![text(&p["token"])],
                        counts: CleanCounts {
                            sessions: u("sessions"),
                            workspaces: u("workspaces"),
                            messages: n("messages"),
                            events: n("events"),
                            disk_bytes: n("disk_bytes"),
                            screenshot_bytes: n("screenshot_bytes"),
                            snapshot_bytes: n("snapshot_bytes"),
                            log_bytes: n("log_bytes"),
                            skipped_running: u("skipped_running"),
                            skipped_other: u("skipped_other"),
                        },
                    },
                ))
            })
            .collect()
    }

    pub fn clean_apply(
        &self,
        token: &str,
        backup: bool,
    ) -> Result<synthesis_session::CleanResult, String> {
        let v = self.call(
            Call::CleanApply {
                token: token.into(),
                backup,
            },
            APPLY_TIMEOUT,
        )?;
        let n = |k: &str| usize::try_from(v[k].as_u64().unwrap_or(0)).unwrap_or(0);
        Ok(synthesis_session::CleanResult {
            deleted: n("deleted"),
            events: v["events"].as_u64().unwrap_or(0),
            skipped_running: n("skipped_running"),
            skipped_other: n("skipped_other"),
            failed: rows_of(&v, "failed", 100)?.iter().map(text).collect(),
            freed_bytes: v["freed_bytes"].as_u64().unwrap_or(0),
            backup: v["backup"]
                .as_str()
                .map(|b| std::path::PathBuf::from(clean(b))),
        })
    }

    /// `/memory list|search`: one text row per record, then the engine's note when project memory is off.
    pub fn memory_rows(&self, query: Option<&str>) -> Result<Vec<String>, String> {
        let v = match query {
            Some(q) => self.store_call(Call::MemoryRetrieve { query: q.into() })?,
            None => self.store_call(Call::MemoryList)?,
        };
        let mut rows: Vec<String> = rows_of(&v, "records", MAX_RECORDS)?
            .iter()
            .map(|m| {
                let title: String = text(&m["title"]).chars().take(100).collect();
                format!(
                    "{}  {}  {}  {}  {title}",
                    text(&m["id"]),
                    text(&m["kind"]),
                    text(&m["scope"]),
                    text(&m["trust"])
                )
            })
            .collect();
        rows.push(format!("{} record(s); /memory show ID for one", rows.len()));
        rows.extend(v["note"].as_str().map(|n| format!("note: {}", clean(n))));
        Ok(rows)
    }

    pub fn memory_show(&self, id: &str) -> Result<String, String> {
        Ok(text(
            &self.store_call(Call::MemoryShow { id: id.into() })?["text"],
        ))
    }

    pub fn memory_add(&self, text_: &str, user: bool) -> Result<String, String> {
        Ok(text(
            &self.store_call(Call::MemoryAdd {
                text: text_.into(),
                user,
            })?["text"],
        ))
    }

    pub fn memory_forget(&self, id: &str) -> Result<String, String> {
        Ok(text(
            &self.store_call(Call::MemoryForget { id: id.into() })?["text"],
        ))
    }

    /// A `#` note, appended to an instruction file on the engine's machine.
    pub fn memory_note(&self, text_: &str, user: bool) -> Result<String, String> {
        Ok(text(
            &self.store_call(Call::MemoryNote {
                text: text_.into(),
                user,
            })?["text"],
        ))
    }

    pub fn config_effort(&self, effort: Option<String>) -> Result<(), String> {
        self.store_call(Call::ConfigEffort { effort }).map(|_| ())
    }

    /// `/context`: `Some(tokens)` when set, `None` when removed.
    pub fn context_set(&self, value: &str) -> Result<Option<u64>, String> {
        Ok(self.store_call(Call::ContextSet {
            value: value.into(),
        })?["tokens"]
            .as_u64())
    }

    pub fn doctor_rows(&self) -> Result<Vec<synthesis_app::doctor::Row>, String> {
        use synthesis_app::doctor::{Level, Row};
        let v = self.call(Call::DoctorSummary, SLOW_TIMEOUT + Duration::from_secs(10))?;
        Ok(rows_of(&v, "rows", 300)?
            .iter()
            .map(|r| Row {
                level: match r["level"].as_str() {
                    Some("ok") => Level::Ok,
                    Some("warn") => Level::Warn,
                    _ => Level::Fail,
                },
                text: text(&r["text"]),
            })
            .collect())
    }

    /// `/mcp`: the report lines, as the local one prints them.
    pub fn mcp_report(&self) -> Result<Vec<String>, String> {
        let v = self.store_call(Call::McpList)?;
        let servers = rows_of(&v, "servers", 100)?;
        if servers.is_empty() {
            return Ok(vec![
                "MCP: no servers are configured for this session".into(),
            ]);
        }
        let mut out = vec![format!("MCP servers ({}):", servers.len())];
        for s in &servers {
            out.push(format!(
                "  {} \u{b7} {} \u{b7} {} \u{b7} {} tools",
                text(&s["name"]),
                text(&s["state"]),
                text(&s["transport"]),
                s["tool_count"].as_u64().unwrap_or(0)
            ));
            if let Some(e) = s["error"].as_str() {
                out.push(format!("    error: {}", clean(e)));
            }
            for t in s["tools"].as_array().into_iter().flatten().take(100) {
                out.push(format!("    {} \u{b7} {}", text(&t[0]), text(&t[1])));
            }
        }
        Ok(out)
    }
}

/// What `trust.state` says (display text and the engine's verdicts; the client decides nothing from it).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct TrustState {
    pub root: String,
    pub cwd: String,
    pub trusted: bool,
    pub session_grant: Option<String>,
    pub refusal: Option<String>,
    pub unfit: Option<String>,
    /// `no` | `trusted` | `changed`
    pub remembered: String,
    pub changed_files: Vec<String>,
    pub since: Option<String>,
    /// The parent folder the engine would trust for "all subfolders" (offered only when it allows it).
    pub parent: Option<String>,
    pub enforcing: bool,
    pub records: Vec<TrustRow>,
    pub unverified: u64,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct TrustRow {
    pub root: String,
    pub scope: String,
    pub trusted_at: String,
    pub state: String,
}

impl TrustState {
    /// The engine's state, bounded and made printable.
    pub fn parse(v: Value) -> Result<Self, String> {
        let mut t: Self =
            serde_json::from_value(v).map_err(|e| format!("unreadable trust state: {e}"))?;
        t.clean();
        Ok(t)
    }

    fn clean(&mut self) {
        let c = |s: &mut String| *s = clean(s);
        c(&mut self.root);
        c(&mut self.cwd);
        c(&mut self.remembered);
        self.changed_files.truncate(20);
        for f in &mut self.changed_files {
            c(f);
        }
        for t in [
            &mut self.session_grant,
            &mut self.refusal,
            &mut self.unfit,
            &mut self.since,
            &mut self.parent,
        ]
        .into_iter()
        .flatten()
        {
            c(t);
        }
        self.records.truncate(200);
        for r in &mut self.records {
            (r.root, r.scope, r.trusted_at, r.state) = (
                clean(&r.root),
                clean(&r.scope),
                clean(&r.trusted_at),
                clean(&r.state),
            );
        }
    }
}
