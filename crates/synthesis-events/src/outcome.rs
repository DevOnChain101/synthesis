//! Event kinds and payloads of outcome orchestration (ADR-016 item 2, docs/architecture).
//!
//! Additive: they are NOT variants of [`crate::Event`] (an exhaustive `match` in a consumer must not
//! break), they travel as untyped store events like `legacy.imported`, and `v` stays 1. A consumer
//! that does not know them sees `Record::Unknown` and ignores it (events.md section 7). All of them
//! live in the outcome's hidden control session.

use serde::{Deserialize, Serialize};

pub const OUTCOME_CREATED: &str = "outcome.created";
pub const CONTRACT_CREATED: &str = "contract.created";
pub const PLAN_CREATED: &str = "plan.created";
/// A model planning run reserved its budget, and later settled (state, cost, hashes; never plan text).
pub const PLAN_RUN_RESERVED: &str = "plan.run.reserved";
pub const PLAN_RUN_SETTLED: &str = "plan.run.settled";
/// A person approved a `plan_approval` decision and the plan of its stored action was installed.
pub const PLAN_APPROVED: &str = "plan.approved";
pub const ATTEMPT_STARTED: &str = "attempt.started";
pub const DECISION_CREATED: &str = "decision.created";
pub const DECISION_RESOLVED: &str = "decision.resolved";
/// A `request_evidence` answer closed a decision and the orchestrator stored its successor (`supersedes`).
pub const DECISION_REBRIEFED: &str = "decision.rebriefed";
pub const GRANT_CREATED: &str = "grant.created";
pub const GRANT_CONSUMED: &str = "grant.consumed";
pub const GRANT_REVOKED: &str = "grant.revoked";
/// Every authorization the orchestrator makes or refuses (grants, expiry, invalidation): action, result,
/// reason and the policy hash. Denials are never suppressed.
pub const POLICY_DECISION: &str = "policy.decision";
pub const EVIDENCE_RECORDED: &str = "evidence.recorded";
pub const COMMAND_FINISHED: &str = "outcome.command_finished";
/// The person's line to a running worker (S1 4.6); `queued` when no attempt runs, `consumed` when the
/// next attempt's first prompt took the queued ones. All three are chained events of the control session.
pub const ATTEMPT_MESSAGE: &str = "attempt.message";
pub const ATTEMPT_MESSAGE_QUEUED: &str = "attempt.message.queued";
pub const ATTEMPT_MESSAGE_CONSUMED: &str = "attempt.message.consumed";
/// Written by the worker in its OWN session once the harness took the message `seq` (the ack).
pub const OWNER_MESSAGE_DELIVERED: &str = "owner_message.delivered";

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
