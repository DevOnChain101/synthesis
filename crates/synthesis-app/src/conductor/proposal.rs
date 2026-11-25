//! Proposals (super-agent-s1.md 4.3): held in memory, never an action. A proposal is bound to the sha256 of its
//! payload; the person's click re-validates it and, if it no longer holds, it becomes `stale` and nothing is sent.

use serde::Serialize;
use sha2::{Digest, Sha256};

use super::config::Action;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum State {
    Shown,
    Accepted,
    Edited,
    Cancelled,
    Stale,
    Expired,
    Failed,
}

/// Another live session the card offers as a one-click "not this one?".
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Alt {
    pub handle: String,
    pub title: String,
}

/// What the person's click would do, exactly. Card text is built from this struct alone (T3).
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Payload {
    CreateSession {
        first_message: String,
        folder: Option<String>,
        model: Option<String>,
        title: Option<String>,
    },
    CreateGoal {
        goal_kind: String,
        contract: String,
        folder: String,
        base_commit: Option<String>,
    },
    SendMessage {
        session: String,
        sid: String,
        text: String,
        basis: String,
        alternatives: Vec<Alt>,
    },
    Interrupt {
        session: String,
        sid: String,
    },
    Close {
        session: String,
        sid: String,
    },
    PauseOutcome {
        outcome: String,
    },
    /// Text to the worker of a work item of an outcome this conductor can read (never automatic).
    MessageWorker {
        outcome: String,
        work_item: String,
        text: String,
    },
}

impl Payload {
    /// The action the confirm knob's auto list may name; `None` for ones that can never be automatic.
    pub fn action(&self) -> Option<Action> {
        match self {
            Payload::CreateSession { .. } => Some(Action::CreateSession),
            Payload::SendMessage { .. } => Some(Action::SendMessage),
            Payload::Interrupt { .. } => Some(Action::Interrupt),
            Payload::CreateGoal { .. }
            | Payload::Close { .. }
            | Payload::PauseOutcome { .. }
            | Payload::MessageWorker { .. } => None,
        }
    }

    pub fn sha256(&self) -> String {
        let bytes = serde_json::to_vec(self).unwrap_or_default();
        Sha256::digest(bytes)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect()
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Proposal {
    pub id: String,
    pub payload: Payload,
    pub payload_sha256: String,
    /// The turn read session content (or a host note carried some) before this was proposed.
    pub tainted: bool,
    pub taint_sources: Vec<String>,
    /// Why a card is forced even if the action is on the auto list (taint, a described target, a model-chosen folder).
    pub card_forced: Vec<String>,
    /// On the auto list and nothing forces a card. P3b still runs nothing without a click.
    pub auto_eligible: bool,
    pub state: State,
    pub created: u64,
    pub detail: Option<String>,
}
