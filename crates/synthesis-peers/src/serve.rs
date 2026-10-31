//! Request server (receiver) and client (sender) for the v1 protocol (messaging.md B5-B7).
//! The server enforces policy and limits and hands accepted requests to a [`Responder`], which the
//! harness implements as a read-only sub-agent (B6). Nothing a peer sends is a grant.

use std::collections::VecDeque;
use std::future::Future;
use std::io;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::mpsc;

use crate::mailbox::Envelope;
use crate::net::{self, Conn, Listener};
use crate::{Addr, Config, Entry, Key};

/// Reply and partial text cap (B6 Outgoing).
pub const MAX_REPLY: usize = 32 * 1024;
const MAX_RESPONDERS: usize = 2;
const RATE_LIMIT: usize = 10;
const RATE_WINDOW: Duration = Duration::from_secs(600);
const PARTIAL_EVERY: Duration = Duration::from_millis(500);
const DEFAULT_TIMEOUT_S: u64 = 300;
const MAX_TIMEOUT_S: u64 = 1800;
/// Connections served at once; a same-user process cannot exhaust tasks by connecting in a loop.
const MAX_CONNECTIONS: usize = 16;
/// Time an authenticated peer has to send its first frame.
const FIRST_FRAME_TIMEOUT: Duration = Duration::from_secs(5);
/// Partial frames kept by the client per request.
const MAX_PARTIALS: usize = 64;
const MAX_PARTIAL_BYTES: usize = 64 * 1024;
const STATUS_TIMEOUT: Duration = Duration::from_millis(500);
const ACCEPT_TIMEOUT: Duration = Duration::from_secs(10);

/// The owner session's redactor, applied to every outgoing byte of text.
pub type Redact = Arc<dyn Fn(&str) -> String + Send + Sync>;
/// Called once per parsed request with `None` (accepted) or the rejection reason.
pub type Observer = Arc<dyn Fn(&Request, Option<&str>) + Send + Sync>;

pub type BoxFut<T> = Pin<Box<dyn Future<Output = T> + Send>>;

#[derive(Debug, Clone)]
pub struct Request {
    pub id: String,
    pub from: Addr,
    pub to_agent: String,
    pub text: String,
    pub chain: Vec<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Reply {
    /// `answered` | `declined` | `failed`
    pub status: String,
    pub text: String,
}

/// Implemented by the harness. `answer` must be read-only (B6) and is dropped on cancel/timeout.
pub trait Responder: Send + Sync + 'static {
    /// Body of the `status` frame (`session`, `status`, `task_summary`, `model`, `accepts`,
    /// `agents`); `task_summary` strings must already be redacted by the owner.
    fn status(&self) -> Value;
    fn has_agent(&self, instance: &str) -> bool;
    fn answer(&self, req: Request, partial: mpsc::UnboundedSender<String>) -> BoxFut<Reply>;
    /// `confirm_replies` (D11): the owner sees the request and the reply and decides. Fails
    /// closed: a responder that cannot ask anybody (headless) declines.
    fn confirm(&self, _req: &Request, _reply: &Reply) -> BoxFut<bool> {
        Box::pin(async { false })
    }
    /// A late `reply` envelope pushed by a peer; true when it was kept (or already had).
    fn deliver_reply(&self, _env: Envelope) -> bool {
        false
    }
}

/// Outcome of a queued request (mailbox, R10c).
#[derive(Debug, PartialEq)]
pub enum Answered {
    Reply(Reply),
    /// Policy refused it; the asker is told.
    Rejected(&'static str),
    /// `rate_limited` / `busy`: leave it queued and try again later.
    Retry(&'static str),
}

pub struct Server {
    pub key: Key,
    pub config: Config,
    /// This session's main agent.
    pub me: Addr,
    pub responder: Arc<dyn Responder>,
    pub redact: Redact,
    pub observer: Observer,
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    active: usize,
    /// Accepted requests in the window, all senders together: `from` is self-asserted.
    sent: VecDeque<Instant>,
}

struct Slot<'a>(&'a Server);
impl Drop for Slot<'_> {
    fn drop(&mut self) {
        self.0.state.lock().unwrap().active -= 1;
    }
}

fn cap(mut s: String, n: usize) -> String {
    if s.len() > n {
        let mut i = n;
        while !s.is_char_boundary(i) {
            i -= 1;
        }
        s.truncate(i);
    }
    s
}

impl Server {
    pub fn new(
        key: Key,
        config: Config,
        me: Addr,
        responder: Arc<dyn Responder>,
        redact: Redact,
        observer: Observer,
    ) -> Arc<Self> {
        Arc::new(Self {
            key,
            config,
            me,
            responder,
            redact,
            observer,
            state: Mutex::default(),
        })
    }

    /// Accept loop; ends when the listener fails or the task is aborted.
    pub async fn run(self: Arc<Self>, mut l: Listener) {
        let cap = Arc::new(tokio::sync::Semaphore::new(MAX_CONNECTIONS));
        while let Ok(conn) = l.accept().await {
            // Over the cap the connection is dropped before any byte is read.
            let Ok(permit) = cap.clone().try_acquire_owned() else {
                continue;
            };
            let s = self.clone();
            tokio::spawn(async move {
                let _permit = permit;
                let _ = s.handle(conn).await;
            });
        }
    }

    /// First reason to refuse, in the order of B5/B6.
    fn check(&self, r: &Request, text_len: usize) -> Option<&'static str> {
        if !self.config.enabled || !self.config.answer {
            return Some("disabled");
        }
        if text_len > MAX_REPLY {
            return Some("too_large");
        }
        if !self.config.permits(&r.from) {
            return Some("denied");
        }
        if r.chain.contains(&self.me.session_id) || r.chain.len() >= self.config.max_hops {
            return Some("cycle");
        }
        if !self.responder.has_agent(&r.to_agent) {
            return Some("no_agent");
        }
        None
    }

    fn admit(&self) -> Result<Slot<'_>, &'static str> {
        let mut st = self.state.lock().unwrap();
        let q = &mut st.sent;
        let now = Instant::now();
        while q
            .front()
            .is_some_and(|t| now.duration_since(*t) > RATE_WINDOW)
        {
            q.pop_front();
        }
        if q.len() >= RATE_LIMIT {
            return Err("rate_limited");
        }
        if st.active >= MAX_RESPONDERS {
            return Err("busy");
        }
        st.sent.push_back(now);
        st.active += 1;
        Ok(Slot(self))
    }

    async fn handle(&self, mut c: Conn) -> io::Result<()> {
        net::handshake_server(&mut c, &self.key).await?;
        let Ok(first) = tokio::time::timeout(FIRST_FRAME_TIMEOUT, c.recv()).await else {
            return Ok(());
        };
        let Some(f) = first? else {
            return Ok(());
        };
        match f.get("t").and_then(Value::as_str) {
            Some("status") => {
                let mut v = self.responder.status();
                if let Some(o) = v.as_object_mut() {
                    o.insert("t".into(), json!("status"));
                }
                c.send(&v).await
            }
            Some("request") => self.request(&mut c, f).await,
            Some("deliver") => {
                let ok = f
                    .get("env")
                    .and_then(|e| Envelope::deserialize(e).ok())
                    .is_some_and(|e| self.accept_reply(e));
                c.send(&json!({"t": if ok { "delivered" } else { "refused" }}))
                    .await
            }
            _ => Ok(()),
        }
    }

    async fn request(&self, c: &mut Conn, f: Value) -> io::Result<()> {
        let Some(req) = parse_request(&f) else {
            return Ok(());
        };
        let id = req.id.clone();
        let reject = |reason: &str| json!({"t":"rejected","id":id,"reason":reason});
        let slot = match self.check(&req, req.text.len()) {
            Some(reason) => Err(reason),
            None => self.admit(),
        };
        (self.observer)(&req, slot.as_ref().err().copied());
        let _slot = match slot {
            Ok(s) => s,
            Err(reason) => return c.send(&reject(reason)).await,
        };
        c.send(&json!({"t":"accepted","id":id})).await?;
        let secs = f
            .get("timeout_s")
            .and_then(Value::as_u64)
            .unwrap_or(DEFAULT_TIMEOUT_S)
            .clamp(1, MAX_TIMEOUT_S);
        let (tx, mut rx) = mpsc::unbounded_channel();
        let req2 = req.clone();
        let answering = self.responder.answer(req, tx);
        let deadline = tokio::time::sleep(Duration::from_secs(secs));
        tokio::pin!(deadline);
        let (mut seq, mut last, mut pending) = (0u64, None::<Instant>, String::new());
        let mut fut = Box::pin(async {
            let r = answering.await;
            self.finish(&req2, r).await
        });
        let reply = loop {
            tokio::select! {
                r = &mut fut => break r,
                () = &mut deadline => break Reply { status: "timeout".into(), text: String::new() },
                // Closing the connection or a cancel frame ends the request; the answer is dropped.
                f = c.recv() => match f {
                    Ok(Some(f)) if f.get("t") != Some(&json!("cancel")) => {}
                    _ => return Ok(()),
                },
                Some(p) = rx.recv() => {
                    // Text arriving inside the 500 ms window is joined to the next frame, not lost.
                    pending.push_str(&p);
                    if last.is_none_or(|t| t.elapsed() >= PARTIAL_EVERY) {
                        last = Some(Instant::now());
                        seq += 1;
                        let text = cap((self.redact)(&std::mem::take(&mut pending)), MAX_REPLY);
                        c.send(&json!({"t":"partial","id":id,"seq":seq,"text":text})).await?;
                    }
                }
            }
        };
        // Text that arrived as the answer finished still goes out as a partial (same 500 ms rule).
        while let Ok(p) = rx.try_recv() {
            pending.push_str(&p);
        }
        if !pending.is_empty() && last.is_none_or(|t| t.elapsed() >= PARTIAL_EVERY) {
            seq += 1;
            let text = cap((self.redact)(&pending), MAX_REPLY);
            c.send(&json!({"t":"partial","id":id,"seq":seq,"text":text}))
                .await?;
        }
        c.send(&json!({"t":"reply","id":id,"status":reply.status,"text":reply.text,"from":self.me}))
            .await
    }

    /// `confirm_replies`, then the owner's redactor and the size cap: the last gate before a
    /// reply leaves this session.
    async fn finish(&self, req: &Request, mut r: Reply) -> Reply {
        if self.config.confirm_replies
            && r.status != "timeout"
            && !self.responder.confirm(req, &r).await
        {
            r = Reply {
                status: "declined".into(),
                text: "the owner of this session did not approve sending a reply".into(),
            };
        }
        r.text = cap((self.redact)(&r.text), MAX_REPLY);
        r
    }

    /// A request read from the mailbox: same policy, limits, responder and reply gate as a live one.
    pub async fn answer_queued(&self, req: Request) -> Answered {
        let slot = match self.check(&req, req.text.len()) {
            Some(reason) => Err(reason),
            None => self.admit(),
        };
        (self.observer)(&req, slot.as_ref().err().copied());
        let _slot = match slot {
            Ok(s) => s,
            Err(r @ ("rate_limited" | "busy")) => return Answered::Retry(r),
            Err(r) => return Answered::Rejected(r),
        };
        let (tx, _rx) = mpsc::unbounded_channel();
        let run = async {
            let r = self.responder.answer(req.clone(), tx).await;
            self.finish(&req, r).await
        };
        match tokio::time::timeout(Duration::from_secs(DEFAULT_TIMEOUT_S), run).await {
            Ok(r) => Answered::Reply(r),
            Err(_) => Answered::Reply(Reply {
                status: "timeout".into(),
                text: String::new(),
            }),
        }
    }

    /// A `deliver` frame: a reply envelope for this session, from a permitted peer.
    fn accept_reply(&self, env: Envelope) -> bool {
        self.config.enabled
            && self.config.initiate
            && env.kind == "reply"
            && env.to_session == self.me.session_id
            && env.verified(&self.key)
            && self.config.permits(&env.from)
            && self.responder.deliver_reply(env)
    }
}

fn parse_request(f: &Value) -> Option<Request> {
    let s = |k| f.get(k)?.as_str().map(str::to_string);
    Some(Request {
        id: s("id")?,
        from: Addr::deserialize(f.get("from")?).ok()?,
        to_agent: s("to_agent")?,
        text: s("text")?,
        chain: f
            .get("chain")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default(),
    })
}

#[derive(Debug)]
pub enum PeerError {
    /// The endpoint did not accept the connection (the caller probes the writer lock to tell
    /// `offline` from `unreachable`).
    Connect,
    /// Handshake failed: wrong key or an impostor server. Nothing was sent.
    Auth,
    /// `rejected` with its reason (`disabled|denied|busy|rate_limited|cycle|too_large|no_agent`).
    Rejected(String),
    Io(io::Error),
}

impl From<io::Error> for PeerError {
    fn from(e: io::Error) -> Self {
        if e.kind() == io::ErrorKind::PermissionDenied {
            Self::Auth
        } else {
            Self::Io(e)
        }
    }
}

async fn open(endpoint: &str, key: &Key) -> Result<Conn, PeerError> {
    let mut c = net::connect(endpoint)
        .await
        .map_err(|_| PeerError::Connect)?;
    net::handshake_client(&mut c, key).await?;
    Ok(c)
}

/// The peer's `status` frame (500 ms budget on top of the handshake).
pub async fn status(endpoint: &str, key: &Key) -> Result<Value, PeerError> {
    tokio::time::timeout(STATUS_TIMEOUT + net::HANDSHAKE_TIMEOUT, async {
        let mut c = open(endpoint, key).await?;
        c.send(&json!({"t":"status"})).await?;
        c.recv()
            .await?
            .ok_or_else(|| PeerError::Io(io::ErrorKind::UnexpectedEof.into()))
    })
    .await
    .map_err(|_| PeerError::Io(io::ErrorKind::TimedOut.into()))?
}

#[derive(Debug, PartialEq)]
pub struct ReplyFrame {
    /// `answered|declined|failed|timeout`
    pub status: String,
    pub text: String,
    pub from: Option<Addr>,
}

pub struct Outgoing<'a> {
    pub id: &'a str,
    pub from: &'a Addr,
    pub to_agent: &'a str,
    pub text: &'a str,
    pub chain: &'a [String],
    pub timeout_s: u64,
}

/// A request the receiver accepted. Dropping it closes the connection, which cancels the request.
pub struct Call {
    c: Conn,
    id: String,
    timeout_s: u64,
}

fn eof() -> PeerError {
    PeerError::Io(io::ErrorKind::UnexpectedEof.into())
}

/// Connect, authenticate, send the request and wait for `accepted` / `rejected`.
pub async fn begin(endpoint: &str, key: &Key, o: &Outgoing<'_>) -> Result<Call, PeerError> {
    let mut c = open(endpoint, key).await?;
    let timeout_s = o.timeout_s.clamp(1, MAX_TIMEOUT_S);
    c.send(
        &json!({"t":"request","id":o.id,"from":o.from,"to_agent":o.to_agent,
        "text":o.text,"chain":o.chain,"timeout_s":timeout_s}),
    )
    .await?;
    let first = async {
        loop {
            let f = c.recv().await?.ok_or_else(eof)?;
            if f.get("id").and_then(Value::as_str) != Some(o.id) {
                continue;
            }
            match f.get("t").and_then(Value::as_str) {
                Some("accepted") => return Ok(()),
                Some("rejected") => {
                    let r = f.get("reason").and_then(Value::as_str).unwrap_or("denied");
                    return Err(PeerError::Rejected(r.to_string()));
                }
                _ => {}
            }
        }
    };
    tokio::time::timeout(ACCEPT_TIMEOUT, first)
        .await
        .map_err(|_| PeerError::Io(io::ErrorKind::TimedOut.into()))??;
    Ok(Call {
        c,
        id: o.id.to_string(),
        timeout_s,
    })
}

impl Call {
    /// Waits for the reply; `on_partial(seq, text)` sees every partial.
    pub async fn wait(
        mut self,
        mut on_partial: impl FnMut(u64, &str),
    ) -> Result<ReplyFrame, PeerError> {
        let id = self.id.clone();
        let (mut kept, mut kept_bytes) = (0usize, 0usize);
        let wait = async {
            loop {
                let f = self.c.recv().await?.ok_or_else(eof)?;
                if f.get("id").and_then(Value::as_str) != Some(id.as_str()) {
                    continue;
                }
                let s = |k| {
                    f.get(k)
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string()
                };
                match f.get("t").and_then(Value::as_str) {
                    Some("partial") => {
                        // A hostile or buggy peer cannot make us hold more than this.
                        let t = cap(s("text"), MAX_REPLY);
                        if kept < MAX_PARTIALS && kept_bytes + t.len() <= MAX_PARTIAL_BYTES {
                            kept += 1;
                            kept_bytes += t.len();
                            on_partial(f.get("seq").and_then(Value::as_u64).unwrap_or(0), &t);
                        }
                    }
                    Some("reply") => {
                        return Ok(ReplyFrame {
                            status: cap(s("status"), 256),
                            text: cap(s("text"), MAX_REPLY),
                            from: f
                                .get("from")
                                .and_then(|a| Addr::deserialize(a).ok())
                                .map(Addr::clamped),
                        });
                    }
                    _ => {}
                }
            }
        };
        tokio::time::timeout(Duration::from_secs(self.timeout_s + 5), wait)
            .await
            .map_err(|_| PeerError::Io(io::ErrorKind::TimedOut.into()))?
    }
}

/// Pushes a signed reply envelope to a live session. `Ok(false)` = it refused (disabled, not
/// permitted, not expected); the caller then stores it in that session's mailbox.
pub async fn deliver(endpoint: &str, key: &Key, env: &Envelope) -> Result<bool, PeerError> {
    tokio::time::timeout(ACCEPT_TIMEOUT, async {
        let mut c = open(endpoint, key).await?;
        c.send(&json!({"t":"deliver","env":env})).await?;
        let f = c.recv().await?.ok_or_else(eof)?;
        Ok(f.get("t") == Some(&json!("delivered")))
    })
    .await
    .map_err(|_| PeerError::Io(io::ErrorKind::TimedOut.into()))?
}

/// Send one request and wait for its reply; `on_partial(seq, text)` sees every partial.
pub async fn ask(
    endpoint: &str,
    key: &Key,
    o: &Outgoing<'_>,
    on_partial: impl FnMut(u64, &str),
) -> Result<ReplyFrame, PeerError> {
    begin(endpoint, key, o).await?.wait(on_partial).await
}

#[derive(Debug)]
pub struct Peer {
    pub entry: Entry,
    /// `Ok(status frame)`, or the state of an entry that did not answer.
    pub state: Result<Value, Down>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Down {
    /// Writer lock free: the holder is dead (the entry was deleted).
    Offline,
    Unreachable,
}

/// Live sessions the allow list permits, with fresh `status` frames fetched in parallel. A
/// connect failure goes through `lock_free` (the session writer-lock probe): free means dead, and
/// the stale entry is deleted. `self_session` is excluded.
pub async fn discover(
    run_dir: &std::path::Path,
    key: &Key,
    config: &Config,
    self_session: &str,
    lock_free: impl Fn(&Entry) -> bool,
) -> Vec<Peer> {
    let mut set = tokio::task::JoinSet::new();
    for e in crate::read_entries(run_dir).into_iter().filter(|e| {
        e.session_id != self_session
            && config.permits(&e.addr("main", ""))
            && crate::endpoint_ok(run_dir, &e.endpoint)
    }) {
        let key = key.clone();
        set.spawn(async move {
            let r = status(&e.endpoint, &key).await;
            (e, r)
        });
    }
    let mut out = Vec::new();
    while let Some(Ok((entry, r))) = set.join_next().await {
        let state = match r {
            Ok(v) => Ok(v),
            Err(PeerError::Connect) if lock_free(&entry) => {
                crate::remove_entry(run_dir, &entry.session_id);
                Err(Down::Offline)
            }
            Err(_) => Err(Down::Unreachable),
        };
        out.push(Peer { entry, state });
    }
    out.sort_by(|a, b| a.entry.session_id.cmp(&b.entry.session_id));
    out
}
