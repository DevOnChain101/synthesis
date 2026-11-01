//! Per-session registry of named sub-agents and the FIFO inbox of every agent, `main` included
//! (messaging.md Part A). Plain data behind one mutex: the harness emits the events and decides
//! when a message reaches a model (`comm.rs`).

use std::collections::{BTreeSet, VecDeque};
use std::sync::{Arc, Mutex};

use synthesis_permissions::{Mode, Permissions};
use synthesis_providers::Message as ChatMessage;
use tokio::sync::{mpsc, watch};
use tokio_util::sync::CancellationToken;

use crate::agents::stricter;
use crate::util::lock;

pub(crate) const MAIN: &str = "main";
pub(crate) const MAX_QUEUED: usize = 32;
/// Harness-authored (forced) posts may exceed `MAX_QUEUED` by this much, never without bound.
const MAX_FORCED_EXTRA: usize = 8;
pub(crate) const MAX_BYTES: usize = 32 * 1024;
pub(crate) const MAX_KEPT: usize = 16;
pub(crate) const MAX_NAME: usize = 32;

pub(crate) const RUNNING: &str = "running";
pub(crate) const WAITING: &str = "waiting_approval";
pub(crate) const IDLE: &str = "idle";
pub(crate) const CANCELLED: &str = "cancelled";
pub(crate) const EVICTED: &str = "evicted";

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Origin {
    User,
    Agent,
    System,
}

impl Origin {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Agent => "agent",
            Self::System => "system",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Kind {
    Message,
    Result,
    Notice,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Message => "message",
            Self::Result => "result",
            Self::Notice => "notice",
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct Msg {
    pub id: String,
    /// Instance name of the sender (`user` for the person).
    pub from: String,
    /// Definition name of the sender's agent.
    pub from_kind: String,
    pub to: String,
    pub origin: Origin,
    pub kind: Kind,
    pub text: String,
    pub summary: Option<String>,
    pub notify_when_idle: bool,
}

/// What an idle sub-agent keeps: enough to run again.
pub(crate) struct Retained {
    pub depth: u32,
    pub mode: Mode,
    pub perms: Arc<Mutex<Permissions>>,
    pub allowed: Option<BTreeSet<String>>,
    pub prompt: Option<String>,
    pub limits: crate::agents::AgentLimits,
    pub messages: Vec<ChatMessage>,
}

/// A sub-agent run waiting for the task loop to poll it.
pub(crate) struct Job {
    pub name: String,
    pub agent_id: String,
    pub kind: String,
    pub parent: String,
    pub retained: Retained,
    pub cancel: CancellationToken,
    /// First run of a background Task: `(turn, tool_call_id)` for its events and a `result` for
    /// the parent. `None` = a resumed agent.
    pub first: Option<(String, String)>,
}

struct Entry {
    agent_id: String,
    name: String,
    kind: String,
    status: &'static str,
    parent: String,
    inbox: VecDeque<Msg>,
    cancel: CancellationToken,
    retained: Option<Retained>,
    /// Senders that asked for the final text when this agent goes idle.
    notify: Vec<String>,
    /// Last use, for the eviction order.
    used: u64,
}

struct Active {
    tx: mpsc::UnboundedSender<Job>,
    cancel: CancellationToken,
    task_id: String,
}

#[derive(Default)]
struct Inner {
    agents: Vec<Entry>,
    main: VecDeque<Msg>,
    active: Option<Active>,
    tick: u64,
}

pub(crate) struct Hub {
    inner: Mutex<Inner>,
    changed: watch::Sender<u64>,
}

impl Default for Hub {
    fn default() -> Self {
        Self {
            inner: Mutex::default(),
            changed: watch::channel(0).0,
        }
    }
}

pub(crate) enum PostError {
    TooLarge(usize),
    Full(String),
    NotFound(String),
}

impl std::fmt::Display for PostError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TooLarge(n) => write!(
                f,
                "too_large: the message is {n} bytes, the limit is {MAX_BYTES}."
            ),
            Self::Full(to) => write!(
                f,
                "inbox_full: @{to} already has {MAX_QUEUED} queued messages; wait for it to catch up."
            ),
            Self::NotFound(m) => f.write_str(m),
        }
    }
}

pub(crate) enum Posted {
    Queued,
    /// An idle agent was started again; `(agent_id, from_status)`.
    Woken(String, &'static str),
}

pub(crate) struct Row {
    pub name: String,
    pub kind: String,
    pub status: &'static str,
    pub queued: usize,
}

pub(crate) struct Evicted {
    pub agent_id: String,
    pub name: String,
    /// Queued messages that went down with it (the caller reports them as dropped).
    pub lost: Vec<Msg>,
}

/// Addresses that mean something else (`from: "user"`, harness notices, `parent`, the main agent).
const RESERVED: [&str; 4] = ["user", "system", "parent", MAIN];

/// Lower-case charset of an instance name.
pub(crate) fn valid_name(n: &str) -> bool {
    !n.is_empty()
        && n.len() <= MAX_NAME
        && n.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_')
}

/// A definition name as an instance name.
fn slug(def: &str) -> String {
    let s: String = def
        .to_ascii_lowercase()
        .chars()
        .map(|c| {
            if c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .take(MAX_NAME - 3)
        .collect();
    if s.is_empty() { "agent".into() } else { s }
}

fn wakeable(s: &str) -> bool {
    s == IDLE || s == CANCELLED
}

impl Hub {
    fn bump(&self) {
        self.changed.send_modify(|v| *v += 1);
    }

    /// Wakes whoever waits on [`Hub::subscribe`].
    pub fn subscribe(&self) -> watch::Receiver<u64> {
        self.changed.subscribe()
    }

    pub fn begin_task(
        &self,
        tx: mpsc::UnboundedSender<Job>,
        cancel: CancellationToken,
        task_id: String,
    ) {
        lock(&self.inner).active = Some(Active {
            tx,
            cancel,
            task_id,
        });
    }

    pub fn end_task(&self) {
        lock(&self.inner).active = None;
    }

    pub fn task_id(&self) -> Option<String> {
        lock(&self.inner).active.as_ref().map(|a| a.task_id.clone())
    }

    pub fn spawn(&self, job: Job) -> bool {
        lock(&self.inner)
            .active
            .as_ref()
            .is_some_and(|a| a.tx.send(job).is_ok())
    }

    /// Creates a running entry. A taken name gets `-2`, `-3`, ...; at the cap the oldest idle
    /// agent is evicted (returned so the caller can emit the event).
    pub fn register(
        &self,
        kind: &str,
        want: Option<&str>,
        parent: &str,
        agent_id: &str,
        cancel: CancellationToken,
    ) -> Result<(String, Option<Evicted>), String> {
        let base = match want {
            Some(n) if valid_name(n) && !RESERVED.contains(&n) => n.to_string(),
            Some(_) => {
                return Err(format!(
                    "Task: name must be 1-{MAX_NAME} characters of a-z, 0-9, - and _, and not user, system, parent or main."
                ));
            }
            None => slug(kind),
        };
        let mut g = lock(&self.inner);
        let mut evicted = None;
        if g.agents.len() >= MAX_KEPT {
            let oldest = g
                .agents
                .iter()
                .enumerate()
                .filter(|(_, e)| wakeable(e.status))
                .min_by_key(|(_, e)| e.used)
                .map(|(i, _)| i);
            let Some(i) = oldest else {
                return Err(format!(
                    "Task: {MAX_KEPT} subagents are alive and none is idle; wait for one to finish."
                ));
            };
            let e = g.agents.remove(i);
            evicted = Some(Evicted {
                agent_id: e.agent_id,
                name: e.name,
                lost: e.inbox.into(),
            });
        }
        let taken =
            |g: &Inner, n: &str| RESERVED.contains(&n) || g.agents.iter().any(|e| e.name == n);
        let mut name = base.clone();
        let mut k = 2;
        while taken(&g, &name) {
            let suffix = format!("-{k}");
            name = format!(
                "{}{suffix}",
                &base[..base.len().min(MAX_NAME - suffix.len())]
            );
            k += 1;
        }
        g.tick += 1;
        let used = g.tick;
        g.agents.push(Entry {
            agent_id: agent_id.into(),
            name: name.clone(),
            kind: kind.into(),
            status: RUNNING,
            parent: parent.into(),
            inbox: VecDeque::new(),
            cancel,
            retained: None,
            notify: vec![],
            used,
        });
        drop(g);
        self.bump();
        Ok((name, evicted))
    }

    #[cfg(test)]
    pub fn status(&self, name: &str) -> Option<&'static str> {
        lock(&self.inner)
            .agents
            .iter()
            .find(|e| e.name == name)
            .map(|e| e.status)
    }

    /// Returns `(agent_id, from)` when the status changed.
    pub fn set_status(&self, name: &str, to: &'static str) -> Option<(String, &'static str)> {
        let mut g = lock(&self.inner);
        let e = g.agents.iter_mut().find(|e| e.name == name)?;
        if e.status == to {
            return None;
        }
        let from = std::mem::replace(&mut e.status, to);
        let id = e.agent_id.clone();
        drop(g);
        self.bump();
        Some((id, from))
    }

    pub fn kind_of(&self, name: &str) -> String {
        if name == MAIN {
            return MAIN.into();
        }
        lock(&self.inner)
            .agents
            .iter()
            .find(|e| e.name == name)
            .map_or_else(|| "user".into(), |e| e.kind.clone())
    }

    pub fn parent_of(&self, name: &str) -> Option<String> {
        lock(&self.inner)
            .agents
            .iter()
            .find(|e| e.name == name)
            .map(|e| e.parent.clone())
    }

    pub fn rows(&self) -> Vec<Row> {
        let g = lock(&self.inner);
        std::iter::once(Row {
            name: MAIN.into(),
            kind: MAIN.into(),
            status: RUNNING,
            queued: g.main.len(),
        })
        .chain(g.agents.iter().map(|e| Row {
            name: e.name.clone(),
            kind: e.kind.clone(),
            status: e.status,
            queued: e.inbox.len(),
        }))
        .collect()
    }

    /// Exactly one agent or an error that lists the candidates. `me` resolves `parent`.
    pub fn resolve(&self, to: &str, me: &str) -> Result<String, String> {
        let to = to.trim();
        let to = to.strip_prefix("agent:").unwrap_or(to);
        if to == MAIN {
            return Ok(MAIN.into());
        }
        if to == "parent" {
            return Ok(self.parent_of(me).unwrap_or_else(|| MAIN.into()));
        }
        let g = lock(&self.inner);
        if let Some(e) = g.agents.iter().find(|e| e.name == to) {
            return Ok(e.name.clone());
        }
        let list = |hits: &[&Entry]| {
            hits.iter()
                .take(10)
                .map(|e| {
                    format!(
                        "{} ({}, {}, id {})",
                        e.name,
                        e.kind,
                        e.status,
                        &e.agent_id[..8.min(e.agent_id.len())]
                    )
                })
                .collect::<Vec<_>>()
                .join("; ")
        };
        let by_id: Vec<&Entry> = if to.len() >= 6 {
            g.agents
                .iter()
                .filter(|e| e.agent_id.starts_with(to))
                .collect()
        } else {
            vec![]
        };
        match by_id.as_slice() {
            [one] => Ok(one.name.clone()),
            [] => {
                let all: Vec<&Entry> = g.agents.iter().collect();
                Err(format!(
                    "not_found: no agent \"{to}\" in this session. Known: main{}{}",
                    if all.is_empty() { "" } else { "; " },
                    list(&all)
                ))
            }
            many => Err(format!(
                "ambiguous: \"{to}\" matches {} agents: {}",
                many.len(),
                list(many)
            )),
        }
    }

    /// Queues `m` for `m.to`. An idle (or cancelled) sub-agent is started again when a task is
    /// running; its mode becomes the stricter of its own and `sender_mode`.
    pub fn post(&self, mut m: Msg, sender_mode: Mode, force: bool) -> Result<Posted, PostError> {
        if m.text.len() > MAX_BYTES {
            if !force {
                return Err(PostError::TooLarge(m.text.len()));
            }
            let mut n = MAX_BYTES;
            while !m.text.is_char_boundary(n) {
                n -= 1;
            }
            m.text.truncate(n);
        }
        // forced posts get the headroom, not a free pass
        let cap = MAX_QUEUED + if force { MAX_FORCED_EXTRA } else { 0 };
        let mut g = lock(&self.inner);
        let to = m.to.clone();
        if to == MAIN {
            if g.main.len() >= cap {
                return Err(PostError::Full(to));
            }
            g.main.push_back(m);
            drop(g);
            self.bump();
            return Ok(Posted::Queued);
        }
        let g = &mut *g;
        let Some(e) = g.agents.iter_mut().find(|e| e.name == to) else {
            return Err(PostError::NotFound(format!(
                "not_found: no agent \"{to}\" in this session (it may have been evicted)."
            )));
        };
        if e.inbox.len() >= cap {
            return Err(PostError::Full(to));
        }
        let notify = m.notify_when_idle.then(|| m.from.clone());
        e.inbox.push_back(m);
        if let Some(n) = notify.filter(|n| !e.notify.contains(n)) {
            e.notify.push(n);
        }
        let mut posted = Posted::Queued;
        if wakeable(e.status)
            && let (Some(active), Some(mut r)) = (&g.active, e.retained.take())
        {
            r.mode = stricter(r.mode, sender_mode);
            lock(&r.perms).set_mode(r.mode);
            let cancel = active.cancel.child_token();
            e.cancel = cancel.clone();
            let job = Job {
                name: e.name.clone(),
                agent_id: e.agent_id.clone(),
                kind: e.kind.clone(),
                parent: e.parent.clone(),
                retained: r,
                cancel,
                first: None,
            };
            match active.tx.send(job) {
                Ok(()) => {
                    let from = std::mem::replace(&mut e.status, RUNNING);
                    posted = Posted::Woken(e.agent_id.clone(), from);
                }
                // the task loop is gone: keep the conversation for the next wake-up
                Err(mpsc::error::SendError(job)) => e.retained = Some(job.retained),
            }
        }
        self.bump();
        Ok(posted)
    }

    pub fn drain(&self, name: &str) -> Vec<Msg> {
        let mut g = lock(&self.inner);
        let q = if name == MAIN {
            Some(&mut g.main)
        } else {
            g.agents
                .iter_mut()
                .find(|e| e.name == name)
                .map(|e| &mut e.inbox)
        };
        q.map(|q| q.drain(..).collect()).unwrap_or_default()
    }

    pub fn has_mail(&self, name: &str) -> bool {
        let g = lock(&self.inner);
        if name == MAIN {
            !g.main.is_empty()
        } else {
            g.agents
                .iter()
                .find(|e| e.name == name)
                .is_some_and(|e| !e.inbox.is_empty())
        }
    }

    /// Some sub-agent other than `me` is running or waiting for an approval.
    pub fn any_running(&self, me: &str) -> bool {
        lock(&self.inner)
            .agents
            .iter()
            .any(|e| e.name != me && (e.status == RUNNING || e.status == WAITING))
    }

    /// Cancels one agent; false when it is not running.
    pub fn cancel(&self, name: &str) -> bool {
        lock(&self.inner)
            .agents
            .iter()
            .find(|e| e.name == name && (e.status == RUNNING || e.status == WAITING))
            .map(|e| e.cancel.cancel())
            .is_some()
    }

    pub fn cancel_all(&self) {
        for e in &lock(&self.inner).agents {
            e.cancel.cancel();
        }
    }

    /// An idle agent leaves the hub's custody to be driven as a task (`status` becomes running).
    pub fn take_idle(
        &self,
        name: &str,
        cancel: CancellationToken,
    ) -> Result<(Retained, String, String, String), String> {
        let mut g = lock(&self.inner);
        let Some(e) = g.agents.iter_mut().find(|e| e.name == name) else {
            return Err(format!("no agent \"{name}\" in this session"));
        };
        if !wakeable(e.status) {
            return Err(format!("@{name} is {}", e.status));
        }
        let Some(r) = e.retained.take() else {
            return Err(format!("@{name} has no saved conversation"));
        };
        e.status = RUNNING;
        e.cancel = cancel;
        let out = (r, e.agent_id.clone(), e.kind.clone(), e.parent.clone());
        drop(g);
        self.bump();
        Ok(out)
    }

    /// The agent stops running: keeps its conversation. Returns the senders to notify.
    pub fn finish(&self, name: &str, r: Retained, status: &'static str) -> Vec<String> {
        let mut g = lock(&self.inner);
        g.tick += 1;
        let used = g.tick;
        let notify = g
            .agents
            .iter_mut()
            .find(|e| e.name == name)
            .map(|e| {
                e.status = status;
                e.retained = Some(r);
                e.used = used;
                std::mem::take(&mut e.notify)
            })
            .unwrap_or_default();
        drop(g);
        self.bump();
        notify
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn msg(to: &str, text: &str) -> Msg {
        Msg {
            id: "m".into(),
            from: "main".into(),
            from_kind: "main".into(),
            to: to.into(),
            origin: Origin::Agent,
            kind: Kind::Message,
            text: text.into(),
            summary: None,
            notify_when_idle: false,
        }
    }

    fn reg(h: &Hub, kind: &str, want: Option<&str>) -> String {
        h.register(
            kind,
            want,
            MAIN,
            &format!("id-{kind}-{want:?}-xxxxxxxx"),
            CancellationToken::new(),
        )
        .unwrap()
        .0
    }

    #[test]
    fn names_are_unique_and_validated() {
        let h = Hub::default();
        assert_eq!(reg(&h, "Developer", None), "developer");
        assert_eq!(reg(&h, "developer", None), "developer-2");
        for n in ["user", "system", "parent", "main"] {
            assert!(
                h.register("d", Some(n), MAIN, "x", CancellationToken::new())
                    .is_err(),
                "{n}"
            );
        }
        assert_eq!(
            reg(&h, "System", None),
            "system-2",
            "a derived name is suffixed"
        );
        assert!(
            h.register("d", Some("Bad Name"), MAIN, "x", CancellationToken::new())
                .is_err()
        );
        assert!(!valid_name("a.b") && !valid_name("") && valid_name("a_b-9"));
    }

    #[test]
    fn inbox_is_fifo_and_capped() {
        let h = Hub::default();
        reg(&h, "w", Some("w"));
        for i in 0..MAX_QUEUED {
            assert!(
                h.post(msg("w", &i.to_string()), Mode::Default, false)
                    .is_ok()
            );
        }
        assert!(matches!(
            h.post(msg("w", "x"), Mode::Default, false),
            Err(PostError::Full(_))
        ));
        assert!(matches!(
            h.post(
                msg("main", &"x".repeat(MAX_BYTES + 1)),
                Mode::Default,
                false
            ),
            Err(PostError::TooLarge(_))
        ));
        // forced posts get a little headroom, then are refused too
        for _ in MAX_QUEUED..MAX_QUEUED + MAX_FORCED_EXTRA {
            assert!(h.post(msg("w", "sys"), Mode::Plan, true).is_ok());
        }
        assert!(matches!(
            h.post(msg("w", "sys"), Mode::Plan, true),
            Err(PostError::Full(_))
        ));
        let got: Vec<String> = h.drain("w").into_iter().map(|m| m.text).collect();
        assert_eq!(got.first().map(String::as_str), Some("0"));
        assert_eq!(got.len(), MAX_QUEUED + MAX_FORCED_EXTRA);
        assert!(!h.has_mail("w"));
    }

    #[test]
    fn at_the_cap_the_oldest_idle_is_evicted_else_register_fails() {
        let h = Hub::default();
        for i in 0..MAX_KEPT {
            reg(&h, "w", Some(&format!("w{i}")));
        }
        assert!(
            h.register("w", None, MAIN, "z", CancellationToken::new())
                .is_err()
        );
        let r = || Retained {
            depth: 1,
            mode: Mode::Default,
            perms: Arc::new(Mutex::new(Permissions::new(
                Default::default(),
                synthesis_permissions::Env::new("/", "/", Mode::Default),
            ))),
            allowed: None,
            prompt: None,
            limits: Default::default(),
            messages: vec![],
        };
        h.finish("w3", r(), IDLE);
        h.finish("w5", r(), IDLE);
        assert!(h.post(msg("w3", "lost"), Mode::Default, false).is_ok());
        let (_, ev) = h
            .register("w", None, MAIN, "z", CancellationToken::new())
            .unwrap();
        let ev = ev.unwrap();
        assert_eq!(ev.name, "w3");
        assert_eq!(
            ev.lost.len(),
            1,
            "the queued message is reported, not lost silently"
        );
        assert_eq!(h.status("w3"), None);
    }

    #[test]
    fn a_failed_wake_up_keeps_the_conversation() {
        let h = Hub::default();
        reg(&h, "w", Some("w"));
        let r = Retained {
            depth: 1,
            mode: Mode::Default,
            perms: Arc::new(Mutex::new(Permissions::new(
                Default::default(),
                synthesis_permissions::Env::new("/", "/", Mode::Default),
            ))),
            allowed: None,
            prompt: None,
            limits: Default::default(),
            messages: vec![],
        };
        h.finish("w", r, IDLE);
        let (tx, rx) = mpsc::unbounded_channel();
        h.begin_task(tx, CancellationToken::new(), "t".into());
        drop(rx); // the task loop has gone
        assert!(matches!(
            h.post(msg("w", "hi"), Mode::Default, false),
            Ok(Posted::Queued)
        ));
        assert_eq!(h.status("w"), Some(IDLE));
        h.end_task();
        assert!(h.take_idle("w", CancellationToken::new()).is_ok());
    }

    #[test]
    fn resolve_lists_candidates() {
        let h = Hub::default();
        reg(&h, "dev", Some("dev"));
        assert_eq!(h.resolve("agent:dev", MAIN).unwrap(), "dev");
        assert_eq!(h.resolve("main", "dev").unwrap(), "main");
        assert_eq!(h.resolve("parent", "dev").unwrap(), "main");
        let e = h.resolve("nobody", MAIN).unwrap_err();
        assert!(e.starts_with("not_found") && e.contains("dev"), "{e}");
    }
}
