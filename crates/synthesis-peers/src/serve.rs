//! Request server (receiver) and client (sender) for the v1 protocol (messaging.md B5-B7).
//! The server enforces policy and limits and hands accepted requests to a [`Responder`], which the
//! harness implements as a read-only sub-agent (B6). Nothing a peer sends is a grant.

use std::collections::{HashMap, VecDeque};
use std::future::Future;
use std::io;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::mpsc;

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
const STATUS_TIMEOUT: Duration = Duration::from_millis(500);

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
}

pub struct Server {
    pub key: Key,
    pub config: Config,
    /// This session's main agent.
    pub me: Addr,
    pub responder: Arc<dyn Responder>,
    /// The owner session's `Redactor`, applied to every outgoing byte of text.
    pub redact: fn(&str) -> String,
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    active: usize,
    sent: HashMap<String, VecDeque<Instant>>,
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
        redact: fn(&str) -> String,
    ) -> Arc<Self> {
        Arc::new(Self {
            key,
            config,
            me,
            responder,
            redact,
            state: Mutex::default(),
        })
    }

    /// Accept loop; ends when the listener fails or the task is aborted.
    pub async fn run(self: Arc<Self>, mut l: Listener) {
        while let Ok(conn) = l.accept().await {
            let s = self.clone();
            tokio::spawn(async move {
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

    fn admit(&self, from: &str) -> Result<Slot<'_>, &'static str> {
        let mut st = self.state.lock().unwrap();
        let q = st.sent.entry(from.to_string()).or_default();
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
        st.sent.get_mut(from).unwrap().push_back(now);
        st.active += 1;
        Ok(Slot(self))
    }

    async fn handle(&self, mut c: Conn) -> io::Result<()> {
        net::handshake_server(&mut c, &self.key).await?;
        let Some(f) = c.recv().await? else {
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
            _ => Ok(()),
        }
    }

    async fn request(&self, c: &mut Conn, f: Value) -> io::Result<()> {
        let Some(req) = parse_request(&f) else {
            return Ok(());
        };
        let id = req.id.clone();
        let reject = |reason: &str| json!({"t":"rejected","id":id,"reason":reason});
        if let Some(reason) = self.check(&req, req.text.len()) {
            return c.send(&reject(reason)).await;
        }
        let _slot = match self.admit(&req.from.session_id) {
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
        let mut fut = self.responder.answer(req, tx);
        let deadline = tokio::time::sleep(Duration::from_secs(secs));
        tokio::pin!(deadline);
        let (mut seq, mut last) = (0u64, None::<Instant>);
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
                    if last.is_none_or(|t| t.elapsed() >= PARTIAL_EVERY) {
                        last = Some(Instant::now());
                        seq += 1;
                        let text = cap((self.redact)(&p), MAX_REPLY);
                        c.send(&json!({"t":"partial","id":id,"seq":seq,"text":text})).await?;
                    }
                }
            }
        };
        let text = cap((self.redact)(&reply.text), MAX_REPLY);
        c.send(&json!({"t":"reply","id":id,"status":reply.status,"text":text,"from":self.me}))
            .await
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

/// Send one request and wait for its reply; `on_partial(seq, text)` sees every partial.
pub async fn ask(
    endpoint: &str,
    key: &Key,
    o: &Outgoing<'_>,
    mut on_partial: impl FnMut(u64, &str),
) -> Result<ReplyFrame, PeerError> {
    let mut c = open(endpoint, key).await?;
    let timeout_s = o.timeout_s.clamp(1, MAX_TIMEOUT_S);
    c.send(
        &json!({"t":"request","id":o.id,"from":o.from,"to_agent":o.to_agent,
        "text":o.text,"chain":o.chain,"timeout_s":timeout_s}),
    )
    .await?;
    let wait = async {
        loop {
            let f = c
                .recv()
                .await?
                .ok_or_else(|| PeerError::Io(io::ErrorKind::UnexpectedEof.into()))?;
            if f.get("id").and_then(Value::as_str) != Some(o.id) {
                continue;
            }
            let s = |k| {
                f.get(k)
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string()
            };
            match f.get("t").and_then(Value::as_str) {
                Some("rejected") => return Err(PeerError::Rejected(s("reason"))),
                Some("partial") => on_partial(
                    f.get("seq").and_then(Value::as_u64).unwrap_or(0),
                    &s("text"),
                ),
                Some("reply") => {
                    return Ok(ReplyFrame {
                        status: s("status"),
                        text: s("text"),
                        from: f.get("from").and_then(|a| Addr::deserialize(a).ok()),
                    });
                }
                _ => {}
            }
        }
    };
    tokio::time::timeout(Duration::from_secs(timeout_s + 5), wait)
        .await
        .map_err(|_| PeerError::Io(io::ErrorKind::TimedOut.into()))?
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
    for e in crate::read_entries(run_dir)
        .into_iter()
        .filter(|e| e.session_id != self_session && config.permits(&e.addr("main", "")))
    {
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
