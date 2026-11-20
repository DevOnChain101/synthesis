//! The only path from a proposal to an effect (super-agent-s1.md 4.3, 4.7): `ProposalExecutor::execute(id,
//! PersonClick)`. The host's outcome and session access is read-only (`ConductorReads`); every mutation is a
//! method of [`Effects`], and only this file calls them. Effects are the person's own commands from surface
//! `superagent` (`via: proposal`): there is no conductor principal.

use std::sync::Arc;

use serde_json::{Value, json};
use synthesis_harness::PersonApproved;
use synthesis_tools::BoxFuture;

use super::click::PersonClick;
use super::host::ConductorHost;
use super::proposal::{Payload, Proposal, State};

/// Audit-only tag carried by the person command (not an authority).
#[derive(Debug, Clone)]
pub struct Via {
    pub proposal_id: String,
    pub proposal_sha256: String,
}

impl Via {
    pub fn to_json(&self) -> Value {
        json!({"via": "proposal", "proposal_id": self.proposal_id, "proposal_sha256": self.proposal_sha256})
    }
}

#[derive(Debug, Clone)]
pub struct SessionStart {
    pub first_message: String,
    pub folder: Option<String>,
    pub model: Option<String>,
    pub title: Option<String>,
}

#[derive(Debug, Clone)]
pub struct GoalDraft {
    pub kind: String,
    pub contract: String,
    pub folder: String,
    pub base_commit: Option<String>,
}

/// What the window does once the person clicked. Implemented by the desktop over its engines and the outcome
/// client; a session send carries the harness's proof that the person approved exactly these bytes.
pub trait Effects: Send + Sync {
    /// Starts a session and sends its first message; returns the handle.
    fn start_session<'a>(
        &'a self,
        req: &'a SessionStart,
        approval: PersonApproved,
        via: &'a Via,
    ) -> BoxFuture<'a, Result<String, String>>;
    fn send<'a>(
        &'a self,
        sid: &'a str,
        text: &'a str,
        approval: PersonApproved,
        via: &'a Via,
    ) -> BoxFuture<'a, Result<(), String>>;
    fn interrupt<'a>(&'a self, sid: &'a str, via: &'a Via) -> BoxFuture<'a, Result<(), String>>;
    fn close<'a>(&'a self, sid: &'a str, via: &'a Via) -> BoxFuture<'a, Result<(), String>>;
    /// A draft goal for the person to finish in the outcomes form; returns its id.
    fn create_goal<'a>(
        &'a self,
        goal: &'a GoalDraft,
        via: &'a Via,
    ) -> BoxFuture<'a, Result<String, String>>;
    fn pause_outcome<'a>(&'a self, id: &'a str, via: &'a Via) -> BoxFuture<'a, Result<(), String>>;
}

pub struct ProposalExecutor {
    host: Arc<ConductorHost>,
    effects: Arc<dyn Effects>,
}

impl ProposalExecutor {
    pub fn new(host: Arc<ConductorHost>, effects: Arc<dyn Effects>) -> Self {
        Self { host, effects }
    }

    /// Runs the stored proposal after the person's click. The proposal is re-validated first; if it no longer
    /// holds it becomes `stale` and nothing is sent. The returned state is `Accepted` or `Failed`.
    pub async fn execute(
        &self,
        id: &str,
        shown_sha256: &str,
        click: PersonClick,
    ) -> Result<State, String> {
        self.run(id, shown_sha256, None, click).await
    }

    /// `shown_sha256` is the hash of the payload the card displayed. As `execute`, with the text the person edited on the card (state `Edited`). Invalid text leaves the card open.
    pub async fn execute_edited(
        &self,
        id: &str,
        shown_sha256: &str,
        text: String,
        click: PersonClick,
    ) -> Result<State, String> {
        self.run(id, shown_sha256, Some(text), click).await
    }

    async fn run(
        &self,
        id: &str,
        shown_sha256: &str,
        edit: Option<String>,
        _click: PersonClick,
    ) -> Result<State, String> {
        let p = self.host.claim(id, shown_sha256, edit)?;
        let via = Via {
            proposal_id: p.id.clone(),
            proposal_sha256: p.payload_sha256.clone(),
        };
        match self.apply(&p, &via).await {
            Ok(()) => Ok(p.state),
            Err(e) => {
                self.host.failed(id, &e);
                Ok(State::Failed)
            }
        }
    }

    async fn apply(&self, p: &Proposal, via: &Via) -> Result<(), String> {
        let fx = &self.effects;
        match &p.payload {
            Payload::CreateSession {
                first_message,
                folder,
                model,
                title,
            } => {
                let req = SessionStart {
                    first_message: first_message.clone(),
                    folder: folder.clone(),
                    model: model.clone(),
                    title: title.clone(),
                };
                fx.start_session(&req, PersonApproved::for_text(first_message), via)
                    .await
                    .map(|_| ())
            }
            Payload::SendMessage { sid, text, .. } => {
                fx.send(sid, text, PersonApproved::for_text(text), via)
                    .await
            }
            Payload::Interrupt { sid, .. } => fx.interrupt(sid, via).await,
            Payload::Close { sid, .. } => fx.close(sid, via).await,
            Payload::CreateGoal {
                goal_kind,
                contract,
                folder,
                base_commit,
            } => {
                let g = GoalDraft {
                    kind: goal_kind.clone(),
                    contract: contract.clone(),
                    folder: folder.clone(),
                    base_commit: base_commit.clone(),
                };
                fx.create_goal(&g, via).await.map(|_| ())
            }
            Payload::PauseOutcome { outcome } => fx.pause_outcome(outcome, via).await,
        }
    }
}
