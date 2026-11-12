//! The remote engine: a child process that speaks `synt engine --stdio` (`synthesis_app::proto`).
//! The child is not trusted: lines are bounded and strictly parsed, text it sends is made printable, an
//! ask is only ever answered by the person (nothing here answers one), every wait has a timeout and
//! the child is killed when this value is dropped, when the stream breaks or when it floods bad lines.

use std::collections::{HashMap, HashSet};
use std::ffi::OsString;
use std::io::{BufReader, Write};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::Duration;

use futures::channel::mpsc::UnboundedSender;
use serde_json::Value;
use synthesis_app::asks::{AskInput, Choice};
use synthesis_app::host::lock;
use synthesis_app::proto::{
    Call, Incoming, Out, Reply, Request, check_hello, parse_incoming, read_line,
};
use synthesis_harness::{UserReply, sanitize_for_display};

use crate::engine::UiMsg;

const HELLO_TIMEOUT: Duration = Duration::from_secs(15);
/// `start` loads config, MCP servers and memory.
const START_TIMEOUT: Duration = Duration::from_secs(120);
const CLOSE_TIMEOUT: Duration = Duration::from_secs(10);
/// Unreadable lines tolerated before the engine is considered broken.
const MAX_BAD_LINES: u32 = 100;

/// How to start the engine process: a program and its arguments (no environment is read here).
#[derive(Clone, Debug)]
pub struct Launcher {
    pub program: OsString,
    pub args: Vec<OsString>,
}

type Waiting = Arc<Mutex<HashMap<u64, mpsc::Sender<Result<Value, String>>>>>;

/// State the reader thread shares with the [`Remote`].
struct Shared {
    child: Mutex<Child>,
    waiting: Waiting,
    /// Id of the `send` whose result is not in yet (0: none): a refusal ends that task.
    send_id: AtomicU64,
    running: AtomicBool,
    closing: AtomicBool,
    /// The engine's stdout ended: nothing will answer any more.
    dead: AtomicBool,
    /// Asks the engine made and nobody answered or withdrew.
    asks: Mutex<HashSet<String>>,
    tx: UnboundedSender<UiMsg>,
}

pub struct Remote {
    shared: Arc<Shared>,
    /// To the writer thread; dropping it closes the engine's stdin (it then shuts down).
    lines: Mutex<Option<mpsc::Sender<Vec<u8>>>>,
    next: AtomicU64,
}

impl Remote {
    /// Starts the process, checks `hello` (exact version) and starts the session in `project` (a path on the
    /// engine's own OS). Blocking. Returns the engine's start report.
    pub fn connect(
        l: &Launcher,
        project: &Path,
        tx: UnboundedSender<UiMsg>,
    ) -> Result<(Self, Value), String> {
        let cwd = project
            .to_str()
            .ok_or("the project path is not valid text")?
            .to_string();
        let mut cmd = Command::new(&l.program);
        cmd.args(&l.args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit());
        #[cfg(windows)]
        std::os::windows::process::CommandExt::creation_flags(&mut cmd, 0x0800_0000); // no console window
        let mut child = cmd
            .spawn()
            .map_err(|e| format!("cannot start the engine: {e}"))?;
        let (Some(mut stdin), Some(stdout)) = (child.stdin.take(), child.stdout.take()) else {
            let _ = child.kill();
            return Err("the engine has no pipes".into());
        };
        let shared = Arc::new(Shared {
            child: Mutex::new(child),
            waiting: Waiting::default(),
            send_id: AtomicU64::new(0),
            running: AtomicBool::new(false),
            closing: AtomicBool::new(false),
            dead: AtomicBool::new(false),
            asks: Mutex::default(),
            tx,
        });
        let (line_tx, line_rx) = mpsc::channel::<Vec<u8>>();
        // from here a failure drops `remote`, which kills the child
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
        let (hello_tx, hello_rx) = mpsc::channel();
        std::thread::spawn(move || read_loop(BufReader::new(stdout), &shared, &hello_tx));
        hello_rx
            .recv_timeout(HELLO_TIMEOUT)
            .map_err(|_| "the engine did not say hello in time".to_string())??;
        let v = remote.call(Call::Start { cwd }, START_TIMEOUT)?;
        Ok((remote, v))
    }

    fn write(&self, req: &Request) -> Result<(), String> {
        let line = serde_json::to_vec(req).map_err(|e| e.to_string())?;
        lock(&self.lines)
            .as_ref()
            .ok_or("the engine is closed")?
            .send(line)
            .map_err(|_| "the engine process ended".to_string())
    }

    fn call(&self, call: Call, wait: Duration) -> Result<Value, String> {
        let id = self.next.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = mpsc::channel();
        lock(&self.shared.waiting).insert(id, tx);
        let r = self.write(&Request { id, call }).and_then(|()| {
            rx.recv_timeout(wait).map_err(|e| match e {
                mpsc::RecvTimeoutError::Timeout => "the engine did not answer in time".to_string(),
                mpsc::RecvTimeoutError::Disconnected => "the engine process ended".to_string(),
            })?
        });
        lock(&self.shared.waiting).remove(&id);
        r
    }

    pub fn running(&self) -> bool {
        self.shared.running.load(Ordering::SeqCst)
    }

    /// Starts a task; `UiMsg::TaskDone` follows. `Err`: one is already running or the engine is gone.
    pub fn send(&self, text: String) -> Result<(), &'static str> {
        if self.shared.running.swap(true, Ordering::SeqCst) {
            return Err("a task is already running");
        }
        let id = self.next.fetch_add(1, Ordering::SeqCst);
        self.shared.send_id.store(id, Ordering::SeqCst);
        let sent = self.write(&Request {
            id,
            call: Call::Send { text },
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

    /// Asks the engine to end the session, waits (bounded) for its report, then the drop kills what is left.
    pub fn close(self) -> Option<String> {
        self.shared.closing.store(true, Ordering::SeqCst);
        let r = self.call(Call::Close, CLOSE_TIMEOUT);
        lock(&self.lines).take(); // stdin EOF
        r.ok()?["worktree"].as_str().map(sanitize_for_display)
    }
}

impl Drop for Remote {
    fn drop(&mut self) {
        let mut c = lock(&self.shared.child);
        let _ = c.kill();
        let _ = c.wait();
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
    let mut bad = 0;
    loop {
        if !ok {
            break;
        }
        match read_line(&mut r) {
            Ok(Some(line)) => match parse_incoming(&line) {
                Ok(m) => dispatch(s, m),
                Err(e) => {
                    bad += 1;
                    let _ =
                        s.tx.unbounded_send(UiMsg::BadEvent(sanitize_for_display(&e)));
                    if bad > MAX_BAD_LINES {
                        break;
                    }
                }
            },
            Ok(None) | Err(_) => break,
        }
    }
    {
        let mut c = lock(&s.child);
        let _ = c.kill();
        let _ = c.wait();
    }
    s.dead.store(true, Ordering::SeqCst);
    lock(&s.waiting).clear(); // every waiting call sees the process ended
    if ok && !s.closing.load(Ordering::SeqCst) {
        let _ =
            s.tx.unbounded_send(UiMsg::BadEvent("the engine process ended".into()));
        if s.running.swap(false, Ordering::SeqCst) {
            let _ =
                s.tx.unbounded_send(UiMsg::TaskDone(Err("the engine process ended".into())));
        }
    }
}

fn dispatch(s: &Shared, m: Incoming) {
    let tell = |m: UiMsg| {
        let _ = s.tx.unbounded_send(m);
    };
    let m = match m {
        Incoming::Event(e) => return tell(UiMsg::Event(e)),
        Incoming::Msg(m) => *m,
    };
    match m {
        Out::Hello { .. } => tell(UiMsg::BadEvent("unexpected hello".into())),
        Out::Result { id, ok, error } => {
            if let Some(w) = lock(&s.waiting).remove(&id) {
                let _ = w.send(match error {
                    Some(e) => Err(sanitize_for_display(&e)),
                    None => Ok(ok.unwrap_or(Value::Null)),
                });
            } else if id == s.send_id.load(Ordering::SeqCst)
                && let Some(e) = error
            {
                s.running.store(false, Ordering::SeqCst);
                tell(UiMsg::TaskDone(Err(sanitize_for_display(&e))));
            }
        }
        m @ Out::Approval { .. } => {
            if let Some(a) = m.into_approval() {
                lock(&s.asks).insert(a.id.clone());
                tell(UiMsg::Approval(Box::new(a)));
            }
        }
        Out::Input { id, req } => {
            lock(&s.asks).insert(id.clone());
            tell(UiMsg::Input(Box::new(AskInput { id, req })));
        }
        Out::Withdraw { id } => {
            lock(&s.asks).remove(&id);
            tell(UiMsg::Withdraw(id));
        }
        Out::TaskDone { error } => {
            s.running.store(false, Ordering::SeqCst);
            tell(UiMsg::TaskDone(
                error.map_or(Ok(()), |e| Err(sanitize_for_display(&e))),
            ));
        }
    }
}
