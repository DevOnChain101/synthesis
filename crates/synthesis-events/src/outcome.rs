//! Event kinds and payloads of outcome orchestration (ADR-016 item 2, docs/architecture).
//!
//! Additive: they are NOT variants of [`crate::Event`] (an exhaustive `match` in a consumer must not
//! break), they travel as untyped store events like `legacy.imported`, and `v` stays 1. A consumer
//! that does not know them sees `Record::Unknown` and ignores it (events.md section 7). All of them
//! live in the outcome's hidden control session.

use serde::{Deserialize, Serialize};

pub const OUTCOME_CREATED: &str = "outcome.created";
pub const PLAN_CREATED: &str = "plan.created";
pub const ATTEMPT_STARTED: &str = "attempt.started";
pub const DECISION_CREATED: &str = "decision.created";
pub const EVIDENCE_RECORDED: &str = "evidence.recorded";
pub const COMMAND_FINISHED: &str = "outcome.command_finished";

/// The four mutable aggregates (each row carries a `revision`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Aggregate {
    Outcome,
    WorkItem,
    Attempt,
    Decision,
}

impl Aggregate {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Outcome => "outcome",
            Self::WorkItem => "work_item",
            Self::Attempt => "attempt",
            Self::Decision => "decision",
        }
    }

    /// `outcome.state_changed`, `work_item.state_changed`, ...
    pub fn state_changed_kind(self) -> String {
        format!("{}.state_changed", self.as_str())
    }
}

/// Payload of every `<aggregate>.state_changed`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StateChanged {
    pub aggregate: Aggregate,
    pub aggregate_id: String,
    /// The aggregate's revision after the change.
    pub revision: i64,
    pub from: String,
    pub to: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}
