//! The real engine in this process (docs/specs/desktop.md section 1, E1): `synthesis_app::run::prepare`
//! builds the harness, the window talks to it through channels. Events arrive as typed envelopes,
//! approvals wait in a map until the person answers, a task runs on the engine's own tokio runtime.
//! The API key only lives in the `Env` snapshot handed to `prepare`, never in the process env.

use std::collections::HashMap;
use std::io;
use std::path::Path;
use std::sync::{Arc, Mutex};

use clap::Parser;
use futures::channel::mpsc::{UnboundedSender, unbounded};
use synthesis_app::run::{Common, Front, Limits, Native, prepare};
use synthesis_config::Env;
use synthesis_core::AgentSession;
use synthesis_events::Envelope;
use synthesis_harness::{
    Approval, ApprovalRequest, Approver, BoxFut, Harness, InputProvider, TaskOutcome, TaskRequest,
    UserReply,
};
use synthesis_permissions::{Mode, Suggestion};
use synthesis_providers::Message;
use synthesis_tools::NeedsUser;
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

/// What the engine sends to the window.
pub enum UiMsg {
    Event(Box<Envelope>),
    /// The task ended (`Err`: it could not run).
    TaskDone(Result<(), String>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Choice {
    Once,
    Always,
    Deny,
}

const DENY_MESSAGE: &str = "The user denied this tool call. Do not retry it unchanged.";

/// Allow once carries no rule; Always carries the rules the dialog offered; anything else denies.
pub fn approval_for(choice: Choice, offered: &[Suggestion]) -> Approval {
    match choice {
        Choice::Once => Approval::allow(),
        Choice::Always => Approval {
            accepted: offered.to_vec(),
            ..Approval::allow()
        },
        Choice::Deny => Approval::deny(DENY_MESSAGE),
    }
}

struct Pending {
    reply: oneshot::Sender<Approval>,
    offered: Vec<Suggestion>,
}

type PendingMap = Arc<Mutex<HashMap<String, Pending>>>;

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

struct ChanApprover(PendingMap);

impl Approver for ChanApprover {
    fn approve<'a>(
        &'a self,
        req: ApprovalRequest,
        cancel: &'a CancellationToken,
    ) -> BoxFut<'a, Approval> {
        Box::pin(async move {
            let id = req.approval_id.clone();
            let (reply, answer) = oneshot::channel();
            lock(&self.0).insert(
                id.clone(),
                Pending {
                    reply,
                    offered: req.suggestions,
                },
            );
            let r = tokio::select! {
                a = answer => a.unwrap_or_else(|_| Approval::auto_denied()),
                () = cancel.cancelled() => Approval::deny("Cancelled before the approval was answered."),
            };
            lock(&self.0).remove(&id);
            r
        })
    }
}

/// Plan and question dialogs are not built yet: nobody can answer them.
struct NoInput;

impl InputProvider for NoInput {
    fn ask<'a>(
        &'a self,
        _: &'a NeedsUser,
        _: &'a CancellationToken,
    ) -> BoxFut<'a, Option<UserReply>> {
        Box::pin(async { None })
    }
}

/// `Write` that turns the JSONL event lines of the sink into `UiMsg::Event`.
struct Lines {
    tx: UnboundedSender<UiMsg>,
    buf: Vec<u8>,
}

impl io::Write for Lines {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.buf.extend_from_slice(bytes);
        while let Some(i) = self.buf.iter().position(|b| *b == b'\n') {
            let line: Vec<u8> = self.buf.drain(..=i).collect();
            if let Ok(e) = serde_json::from_slice::<Envelope>(&line[..line.len() - 1]) {
                let _ = self.tx.unbounded_send(UiMsg::Event(Box::new(e)));
            }
        }
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[derive(Parser)]
struct Defaults {
    #[command(flatten)]
    c: Common,
}

pub struct Engine {
    pub sid: String,
    pub label: String,
    pub models: Vec<String>,
    pub effort_tiers: Vec<String>,
    pub notices: Vec<String>,
    pub cwd: String,
    harness: Arc<Harness>,
    tokio: Option<tokio::runtime::Runtime>,
    session: Arc<Mutex<Option<AgentSession>>>,
    history: Arc<Mutex<Vec<Message>>>,
    last: Arc<Mutex<Option<TaskOutcome>>>,
    pending: PendingMap,
    cancel: Mutex<Option<CancellationToken>>,
    tx: UnboundedSender<UiMsg>,
    mcp: Option<Arc<synthesis_mcp::McpManager>>,
    pub sandbox: synthesis_tui::SandboxStatus,
    pub memory_on: bool,
}

impl Engine {
    /// Builds the harness and starts the session. Blocking: call it from a worker thread, never
    /// from inside a tokio runtime.
    pub fn start(
        project: &Path,
        env: &Env,
        model: Option<&str>,
        provider: Option<&str>,
        sandbox: Option<&str>,
    ) -> Result<(Self, futures::channel::mpsc::UnboundedReceiver<UiMsg>), String> {
        let (tx, rx) = unbounded();
        let pending: PendingMap = PendingMap::default();
        // `sandbox` is `--sandbox`'s value (enforce|best-effort|off); `None` keeps the default (enforce).
        let mut args = vec!["synthesis-desktop"];
        args.extend(sandbox.into_iter().flat_map(|m| ["--sandbox", m]));
        let mut common = Defaults::try_parse_from(args).map_err(|e| e.to_string())?.c;
        common.model = model.map(str::to_string);
        common.provider = provider.map(str::to_string);
        let native = Native {
            out: Box::new(Lines {
                tx: tx.clone(),
                buf: vec![],
            }),
            approver: Arc::new(ChanApprover(pending.clone())),
            input: Arc::new(NoInput),
        };
        let p = prepare(
            &common,
            project,
            env,
            &Limits::default(),
            Front::Native(native),
        )?;
        let mut session = p.session;
        p.harness
            .start_session(&mut session, "desktop")
            .map_err(|e| e.to_string())?;
        let cwd = session.workspace.cwd.to_string_lossy().into_owned();
        Ok((
            Self {
                sid: p.sid,
                label: p.label,
                models: p.models,
                effort_tiers: p.effort_tiers,
                notices: p.notices,
                cwd,
                harness: p.harness,
                tokio: Some(p.tokio),
                session: Arc::new(Mutex::new(Some(session))),
                history: Arc::default(),
                last: Arc::default(),
                pending,
                cancel: Mutex::new(None),
                tx,
                mcp: p.mcp,
                sandbox: p.sandbox,
                memory_on: p.memory.is_some(),
            },
            rx,
        ))
    }

    pub fn mcp(&self) -> Option<&Arc<synthesis_mcp::McpManager>> {
        self.mcp.as_ref()
    }

    pub fn set_mode(&self, mode: Mode) {
        let _ = self.harness.set_mode(mode, false);
    }

    pub fn set_effort(&self, effort: Option<String>) {
        self.harness.set_reasoning_effort(effort);
    }

    /// Starts a task. `Err`: one is already running.
    pub fn send(&self, text: String) -> Result<(), &'static str> {
        let Some(mut session) = lock(&self.session).take() else {
            return Err("a task is already running");
        };
        let token = CancellationToken::new();
        *lock(&self.cancel) = Some(token.clone());
        let mode = lock(&self.harness.permissions()).mode();
        session.permission_mode = synthesis_harness::from_mode(mode);
        let req = TaskRequest {
            history: lock(&self.history).clone(),
            ..TaskRequest::new(text)
        };
        let (h, tx) = (self.harness.clone(), self.tx.clone());
        let (slot, history, last) = (
            self.session.clone(),
            self.history.clone(),
            self.last.clone(),
        );
        let Some(rt) = &self.tokio else {
            return Err("the engine is closed");
        };
        rt.spawn(async move {
            let r = h.run_task(&mut session, req, token).await;
            let done = match r {
                Ok(o) => {
                    let warn = h.to_idle(&mut session).err().map(|e| e.to_string());
                    *lock(&history) = o.messages.clone();
                    *lock(&last) = Some(o);
                    warn.map_or(Ok(()), Err)
                }
                Err(e) => Err(e.to_string()),
            };
            *lock(&slot) = Some(session);
            let _ = tx.unbounded_send(UiMsg::TaskDone(done));
        });
        Ok(())
    }

    pub fn cancel(&self) {
        if let Some(t) = lock(&self.cancel).as_ref() {
            t.cancel();
        }
    }

    /// Answers the approval `id`; false when it is no longer waiting.
    pub fn answer(&self, id: &str, choice: Choice) -> bool {
        let Some(p) = lock(&self.pending).remove(id) else {
            return false;
        };
        p.reply.send(approval_for(choice, &p.offered)).is_ok()
    }

    /// Ends the session like the TUI host does and stops the runtime.
    pub fn close(mut self) {
        self.cancel();
        let outcome = lock(&self.last).take().unwrap_or(TaskOutcome {
            task_id: String::new(),
            status: synthesis_events::TaskStatus::Completed,
            reason: "closed".into(),
            verification_report_id: None,
            final_text: String::new(),
            usage: synthesis_events::Usage {
                input_tokens: 0,
                output_tokens: 0,
                cache_read_input_tokens: None,
                cache_creation_input_tokens: None,
                cost_usd: None,
            },
            changed_files: vec![],
            needed_permissions: vec![],
            messages: vec![],
        });
        self.harness.end_session(&outcome);
        if let (Some(rt), Some(m)) = (&self.tokio, &self.mcp) {
            rt.block_on(m.close());
        }
        if let Some(rt) = self.tokio.take() {
            rt.shutdown_background();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use synthesis_harness::ApprovalDecision;
    use synthesis_permissions::RuleSpec;

    fn offered() -> Vec<Suggestion> {
        vec![Suggestion::AddRules {
            rules: vec![RuleSpec {
                tool_name: "Bash".into(),
                rule_content: Some("ls".into()),
            }],
            behavior: synthesis_permissions::Decision::Allow,
        }]
    }

    #[test]
    fn choices_map_to_decisions_and_only_always_carries_rules() {
        let s = offered();
        let once = approval_for(Choice::Once, &s);
        assert_eq!(once.decision, ApprovalDecision::Allow);
        assert!(once.accepted.is_empty());
        let always = approval_for(Choice::Always, &s);
        assert_eq!(always.decision, ApprovalDecision::Allow);
        assert_eq!(always.accepted, s);
        let deny = approval_for(Choice::Deny, &s);
        assert_eq!(deny.decision, ApprovalDecision::Deny);
        assert!(deny.accepted.is_empty());
    }
}
