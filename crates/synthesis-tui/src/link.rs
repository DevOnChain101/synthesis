//! The only way the harness reaches the UI: events, approvals and questions all travel as
//! [`Msg`]s over one channel, so the UI thread owns every piece of UI state and the harness
//! side never blocks on it except by awaiting an answer (always raced against cancellation).

use std::io::{self, Write};
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::Duration;

use synthesis_core::AgentSession;
use synthesis_events::Envelope;
use synthesis_harness::{
    Approval, ApprovalDecision, ApprovalRequest, Approver, BoxFut, InputProvider, TaskOutcome,
    UserReply,
};
use synthesis_tools::NeedsUser;
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

pub(crate) struct ApprovalAsk {
    pub req: ApprovalRequest,
    pub reply: oneshot::Sender<Approval>,
    /// Absolute expiry (UI clock, ms), shown as a countdown; the approver enforces it.
    pub deadline_ms: Option<i64>,
}

pub(crate) struct InputAsk {
    pub id: String,
    pub req: NeedsUser,
    pub reply: oneshot::Sender<Option<UserReply>>,
}

pub(crate) struct TaskDone {
    pub result: Result<TaskOutcome, String>,
    /// A problem after the task ended (the session could not return to idle).
    pub warn: Option<String>,
    /// `None` when the task stopped without handing the session back.
    pub session: Option<AgentSession>,
}

pub(crate) struct UserDone {
    /// The conversation with the command and its output appended, and what happened.
    pub result: Result<
        (
            Vec<synthesis_providers::Message>,
            synthesis_harness::ToolOutcome,
        ),
        String,
    >,
    pub record_only: bool,
}

pub(crate) enum Msg {
    Bytes(Vec<u8>),
    InputClosed,
    /// Something may have changed (SIGWINCH): re-read the size.
    #[cfg_attr(not(unix), allow(dead_code))] // only the unix signal watcher sends it
    Wake,
    /// SIGINT delivered to the process (raw mode turns ctrl-c into a key instead).
    #[cfg_attr(not(unix), allow(dead_code))]
    Interrupt,
    Event(Box<Envelope>),
    BadEvent(String),
    Approval(Box<ApprovalAsk>),
    Input(Box<InputAsk>),
    /// The approval/question with this id was cancelled or expired on the harness side.
    Withdraw(String),
    TaskDone(Box<TaskDone>),
    /// A `!cmd` finished (the model turn that follows is started by the runtime).
    UserCommandDone(Box<UserDone>),
    /// A line typed to an agent was refused (text, why).
    SendFailed(String, String),
    /// `/compact` between tasks finished: the compacted history, or why not.
    Compacted(Result<Vec<synthesis_providers::Message>, String>),
}

pub(crate) type Tx = Sender<Msg>;

/// Handles for wiring a [`synthesis_harness::Harness`] to the UI. Create it first, give the
/// approver, input provider and event writer to the harness, then pass it to `run_interactive`.
pub struct UiLink {
    pub(crate) tx: Tx,
    pub(crate) rx: Receiver<Msg>,
    timeout: Option<Duration>,
    now_ms: fn() -> i64,
}

impl Default for UiLink {
    fn default() -> Self {
        Self::new()
    }
}

impl UiLink {
    pub fn new() -> Self {
        let (tx, rx) = mpsc::channel();
        Self {
            tx,
            rx,
            timeout: None,
            now_ms: crate::wall_clock_ms,
        }
    }

    /// Unanswered approvals expire (decision `expired`) after this long. `ApprovalRequest`
    /// carries no deadline today (the harness emits `deadline: null`), so the UI imposes one.
    pub fn with_approval_timeout(mut self, d: Duration) -> Self {
        self.timeout = Some(d);
        self
    }

    pub fn approver(&self) -> std::sync::Arc<dyn Approver> {
        std::sync::Arc::new(UiApprover {
            tx: self.tx.clone(),
            timeout: self.timeout,
            now_ms: self.now_ms,
        })
    }

    pub fn input(&self) -> std::sync::Arc<dyn InputProvider> {
        std::sync::Arc::new(UiInput {
            tx: self.tx.clone(),
        })
    }

    /// Output for `EventSink::new`: JSONL lines become UI events. Tee it with the session
    /// store writer; the UI never persists anything itself.
    pub fn event_writer(&self) -> EventWriter {
        EventWriter {
            tx: self.tx.clone(),
            buf: Vec::new(),
        }
    }
}

pub struct EventWriter {
    tx: Tx,
    buf: Vec<u8>,
}

impl Write for EventWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.buf.extend_from_slice(bytes);
        while let Some(i) = self.buf.iter().position(|b| *b == b'\n') {
            let line: Vec<u8> = self.buf.drain(..=i).collect();
            let line = &line[..line.len() - 1];
            if line.is_empty() {
                continue;
            }
            let msg = match serde_json::from_slice::<Envelope>(line) {
                Ok(env) => Msg::Event(Box::new(env)),
                // An event this build cannot read must be visible, not skipped.
                Err(e) => Msg::BadEvent(format!("unreadable event: {e}")),
            };
            // The UI is gone (quit): the harness keeps running to its own end, nobody to tell.
            let _ = self.tx.send(msg);
        }
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Tells the UI to drop a dialog when its future is dropped (cancel, expiry, or answered).
struct Withdraw {
    tx: Tx,
    id: String,
}

impl Drop for Withdraw {
    fn drop(&mut self) {
        let _ = self.tx.send(Msg::Withdraw(std::mem::take(&mut self.id)));
    }
}

struct UiApprover {
    tx: Tx,
    timeout: Option<Duration>,
    now_ms: fn() -> i64,
}

impl Approver for UiApprover {
    fn approve<'a>(
        &'a self,
        req: ApprovalRequest,
        cancel: &'a CancellationToken,
    ) -> BoxFut<'a, Approval> {
        Box::pin(async move {
            let id = req.approval_id.clone();
            let (reply, answer) = oneshot::channel();
            let deadline_ms = self
                .timeout
                .map(|d| (self.now_ms)().saturating_add(d.as_millis() as i64));
            let ask = ApprovalAsk {
                req,
                reply,
                deadline_ms,
            };
            if self.tx.send(Msg::Approval(Box::new(ask))).is_err() {
                return Approval::auto_denied(); // the UI is gone: nobody can approve
            }
            let _withdraw = Withdraw {
                tx: self.tx.clone(),
                id,
            };
            let expiry = async {
                match self.timeout {
                    Some(d) => tokio::time::sleep(d).await,
                    None => std::future::pending().await,
                }
            };
            tokio::select! {
                a = answer => a.unwrap_or_else(|_| Approval::auto_denied()),
                () = cancel.cancelled() => Approval {
                    reviewer: "policy",
                    ..Approval::deny("Cancelled before the approval was answered.")
                },
                () = expiry => Approval {
                    decision: ApprovalDecision::Expired,
                    reviewer: "policy",
                    updated_input: None,
                    accepted: vec![],
                    message: Some("The approval request expired unanswered. The tool was NOT run.".into()),
                },
            }
        })
    }
}

struct UiInput {
    tx: Tx,
}

impl InputProvider for UiInput {
    fn ask<'a>(
        &'a self,
        req: &'a NeedsUser,
        cancel: &'a CancellationToken,
    ) -> BoxFut<'a, Option<UserReply>> {
        Box::pin(async move {
            let id = uuid_like();
            let (reply, answer) = oneshot::channel();
            let ask = InputAsk {
                id: id.clone(),
                req: req.clone(),
                reply,
            };
            if self.tx.send(Msg::Input(Box::new(ask))).is_err() {
                return None;
            }
            let _withdraw = Withdraw {
                tx: self.tx.clone(),
                id,
            };
            tokio::select! {
                a = answer => a.unwrap_or(None),
                () = cancel.cancelled() => None,
            }
        })
    }
}

fn uuid_like() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(1);
    format!("ask-{}", N.fetch_add(1, Ordering::Relaxed))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_writer_splits_lines_and_reports_unreadable_ones() {
        let link = UiLink::new();
        let mut w = link.event_writer();
        let good = r#"{"v":1,"seq":1,"ts":"2026-01-01T00:00:00Z","session_id":"s","task_id":null,"turn_id":null,"agent_id":null,"type":"notice","payload":{"level":"info","text":"hi"}}"#;
        // one line split across writes, then a broken one, then a blank
        w.write_all(&good.as_bytes()[..40]).unwrap();
        assert!(link.rx.try_recv().is_err(), "no newline yet");
        w.write_all(&good.as_bytes()[40..]).unwrap();
        w.write_all(b"\nnot json\n\n").unwrap();
        assert!(matches!(link.rx.try_recv().unwrap(), Msg::Event(e) if e.seq == 1));
        assert!(
            matches!(link.rx.try_recv().unwrap(), Msg::BadEvent(m) if m.contains("unreadable event"))
        );
        assert!(link.rx.try_recv().is_err());
    }

    #[test]
    fn a_gone_ui_denies_instead_of_hanging() {
        let link = UiLink::new();
        let approver = link.approver();
        let input = link.input();
        drop(link); // receiver dropped
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let cancel = CancellationToken::new();
        let a = rt.block_on(approver.approve(
            ApprovalRequest {
                approval_id: "a".into(),
                agent_id: None,
                tool_call_id: "t".into(),
                tool: "Bash".into(),
                input: serde_json::json!({}),
                dialog: "permission",
                prompt: String::new(),
                reason: String::new(),
                suggestions: vec![],
            },
            &cancel,
        ));
        assert_eq!(a.decision, ApprovalDecision::AutoDenied);
        let q = NeedsUser::ExitPlanMode { plan: "p".into() };
        assert!(rt.block_on(input.ask(&q, &cancel)).is_none());
    }
}
