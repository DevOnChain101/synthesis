//! The conductor's `WindowPort` over the Super Agent window (phase C1 wiring): sessions are the ones `SessionHost`
//! hosts. Reads come from a snapshot the view refreshes whenever frames or chats change; effects go to the view
//! as commands and run on the UI thread, where the entities live. Nothing here builds the person's click: every
//! effect arrives with the harness's proof from the card the person confirmed.

use std::collections::HashMap;
use std::sync::mpsc::{Sender, channel};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::channel::mpsc::UnboundedSender;
use futures::channel::oneshot;
use synthesis_app::conductor::{GoalDraft, SessionReport, SessionRow, SessionStart};
use synthesis_harness::PersonApproved;
use synthesis_tools::BoxFuture;

use super::chat::glue::WindowPort;

/// How long the conductor's thread waits for the UI thread to open the goal form.
const FORM_WAIT: Duration = Duration::from_secs(5);
const GONE: &str = "the window is closed";

#[derive(Default)]
pub struct Snapshot {
    pub rows: Vec<SessionRow>,
    /// By handle.
    pub reports: HashMap<String, SessionReport>,
}

type Reply<T> = oneshot::Sender<Result<T, String>>;

pub enum Cmd {
    Start(SessionStart, Reply<String>),
    Send(String, String, PersonApproved, Reply<()>),
    Interrupt(String, Reply<()>),
    Close(String, Reply<()>),
    Goal(GoalDraft, Sender<Result<String, String>>),
}

pub struct HostPort {
    pub shared: Arc<Mutex<Snapshot>>,
    tx: UnboundedSender<Cmd>,
}

impl HostPort {
    pub fn new(tx: UnboundedSender<Cmd>) -> Self {
        Self {
            shared: Arc::default(),
            tx,
        }
    }

    fn ask<'a, T: Send + 'a>(
        &self,
        make: impl FnOnce(Reply<T>) -> Cmd,
    ) -> BoxFuture<'a, Result<T, String>> {
        let (tx, rx) = oneshot::channel();
        let sent = self.tx.unbounded_send(make(tx)).is_ok();
        Box::pin(async move {
            if !sent {
                return Err(GONE.into());
            }
            rx.await.unwrap_or_else(|_| Err(GONE.into()))
        })
    }
}

impl WindowPort for HostPort {
    fn sessions(&self) -> Vec<SessionRow> {
        self.shared
            .lock()
            .map(|s| s.rows.clone())
            .unwrap_or_default()
    }
    fn report(&self, handle: &str) -> Option<SessionReport> {
        self.shared.lock().ok()?.reports.get(handle).cloned()
    }
    fn start<'a>(
        &'a self,
        req: &'a SessionStart,
        _approval: PersonApproved,
    ) -> BoxFuture<'a, Result<String, String>> {
        self.ask(|r| Cmd::Start(req.clone(), r))
    }
    fn send<'a>(
        &'a self,
        sid: &'a str,
        text: &'a str,
        approval: PersonApproved,
    ) -> BoxFuture<'a, Result<(), String>> {
        self.ask(|r| Cmd::Send(sid.into(), text.into(), approval, r))
    }
    fn interrupt<'a>(&'a self, sid: &'a str) -> BoxFuture<'a, Result<(), String>> {
        self.ask(|r| Cmd::Interrupt(sid.into(), r))
    }
    fn close<'a>(&'a self, sid: &'a str) -> BoxFuture<'a, Result<(), String>> {
        self.ask(|r| Cmd::Close(sid.into(), r))
    }
    fn open_goal_form(&self, goal: &GoalDraft) -> Result<String, String> {
        let (tx, rx) = channel();
        self.tx
            .unbounded_send(Cmd::Goal(goal.clone(), tx))
            .map_err(|_| GONE.to_string())?;
        rx.recv_timeout(FORM_WAIT)
            .unwrap_or_else(|_| Err(GONE.into()))
    }
}
