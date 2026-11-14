//! State machines (ADR-017, ADR-019 section 4), in the style of `SessionStatus::can_transition_to`:
//! one table per machine, anything not listed is rejected. A second layer of *guards* holds the
//! conditions the table cannot see (verdict, retry budget, who acts). The caller computes the guard
//! inputs from stored facts; nothing here reads a clock or a database.

use serde::{Deserialize, Serialize};

/// Why a transition was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    /// The pair is not in the table.
    NotAllowed,
    /// The pair is in the table but a guard failed (the text names it).
    Guard(&'static str),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TransitionError<S> {
    pub from: S,
    pub to: S,
    pub refusal: Refusal,
}

impl<S: std::fmt::Debug> std::fmt::Display for TransitionError<S> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.refusal {
            Refusal::NotAllowed => write!(f, "invalid transition {:?} -> {:?}", self.from, self.to),
            Refusal::Guard(g) => write!(
                f,
                "transition {:?} -> {:?} refused: {g}",
                self.from, self.to
            ),
        }
    }
}
impl<S: std::fmt::Debug> std::error::Error for TransitionError<S> {}

fn run<S: Copy>(
    from: S,
    to: S,
    in_table: bool,
    guard: Result<(), &'static str>,
) -> Result<S, TransitionError<S>> {
    let err = |refusal| TransitionError { from, to, refusal };
    if !in_table {
        return Err(err(Refusal::NotAllowed));
    }
    guard.map(|()| to).map_err(|g| err(Refusal::Guard(g)))
}

fn need(ok: bool, what: &'static str) -> Result<(), &'static str> {
    if ok { Ok(()) } else { Err(what) }
}

// ------------------------------------------------------------------ outcome

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OutcomeState {
    Draft,
    ContractPending,
    Ready,
    Running,
    Blocked,
    Verifying,
    ReworkRequired,
    Verified,
    Completed,
    Failed,
    Cancelled,
}

/// Facts the outcome guards need. All false by default: nothing is allowed by omission.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct OutcomeGuards {
    /// The current contract version is approved (hash recorded).
    pub contract_approved: bool,
    /// The current plan version passed `validate_plan`.
    pub plan_valid: bool,
    /// The ADR-020 verdict function said `Verified` for the current candidate commit.
    pub verdict_verified: bool,
    /// A delivery condition holds (MVP: patch or branch ready; no merge).
    pub delivery_ready: bool,
}

impl OutcomeState {
    pub const ALL: [OutcomeState; 11] = [
        Self::Draft,
        Self::ContractPending,
        Self::Ready,
        Self::Running,
        Self::Blocked,
        Self::Verifying,
        Self::ReworkRequired,
        Self::Verified,
        Self::Completed,
        Self::Failed,
        Self::Cancelled,
    ];

    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Completed | Self::Failed | Self::Cancelled)
    }

    pub fn can_transition_to(self, to: Self) -> bool {
        use OutcomeState::*;
        match (self, to) {
            (Draft, ContractPending)
            | (ContractPending, Ready)
            | (Ready, Running)
            | (Running, Verifying | Blocked | Failed)
            | (Blocked, Running)
            | (Verifying, Verified | ReworkRequired)
            | (ReworkRequired, Running)
            | (Verified, Completed) => true,
            (from, Cancelled) => !from.is_terminal(),
            _ => false,
        }
    }

    pub fn transition(self, to: Self, g: &OutcomeGuards) -> Result<Self, TransitionError<Self>> {
        use OutcomeState::*;
        let guard = match (self, to) {
            (ContractPending, Ready) => need(
                g.contract_approved && g.plan_valid,
                "contract not approved or plan not valid",
            ),
            // Verified is set only by the verdict function (ADR-017).
            (Verifying, Verified) => need(g.verdict_verified, "verdict is not Verified"),
            (Verified, Completed) => need(g.delivery_ready, "no delivery condition holds"),
            _ => Ok(()),
        };
        run(self, to, self.can_transition_to(to), guard)
    }
}

// ---------------------------------------------------------------- work item

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkItemState {
    Created,
    Ready,
    Leased,
    Running,
    Verifying,
    WaitingDecision,
    RetryableFailure,
    Recovering,
    Blocked,
    CancelRequested,
    Succeeded,
    TerminalFailure,
    Cancelled,
}

/// Facts the work item guards need.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct WorkItemGuards {
    /// Retries already started (`work_items.retries_used`, bumped when a retry leaves
    /// `RetryableFailure` for `Ready`).
    pub retries_used: u32,
    /// Retries allowed after the first attempt (`max_attempts_per_work_item - 1`).
    pub max_retries: u32,
    /// `independent_check` evidence (ADR-020) covers this item's checks for the current commit.
    pub independent_evidence: bool,
}

impl WorkItemState {
    pub const ALL: [WorkItemState; 13] = [
        Self::Created,
        Self::Ready,
        Self::Leased,
        Self::Running,
        Self::Verifying,
        Self::WaitingDecision,
        Self::RetryableFailure,
        Self::Recovering,
        Self::Blocked,
        Self::CancelRequested,
        Self::Succeeded,
        Self::TerminalFailure,
        Self::Cancelled,
    ];

    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Succeeded | Self::TerminalFailure | Self::Cancelled
        )
    }

    pub fn can_transition_to(self, to: Self) -> bool {
        use WorkItemState::*;
        match (self, to) {
            (Created, Ready)
            | (Ready, Leased)
            | (Leased, Running)
            | (Running, Verifying | WaitingDecision)
            | (WaitingDecision, Ready)
            | (Running | Verifying, RetryableFailure)
            | (RetryableFailure, Ready)
            | (Running | Verifying | RetryableFailure | Recovering, TerminalFailure)
            | (Leased | Running, Recovering)
            | (Recovering, Ready | Blocked)
            // not in ADR-017's list: a blocked item must be able to come back after a decision
            | (Blocked, Ready)
            | (Verifying, Succeeded)
            | (CancelRequested, Cancelled) => true,
            (from, CancelRequested) => !from.is_terminal() && from != CancelRequested,
            _ => false,
        }
    }

    pub fn transition(self, to: Self, g: &WorkItemGuards) -> Result<Self, TransitionError<Self>> {
        use WorkItemState::*;
        let budget = || need(g.retries_used < g.max_retries, "retry budget exhausted");
        let guard = match (self, to) {
            (_, RetryableFailure) | (RetryableFailure, Ready) => budget(),
            (Verifying, Succeeded) => need(
                g.independent_evidence,
                "no independent evidence for the current commit",
            ),
            _ => Ok(()),
        };
        run(self, to, self.can_transition_to(to), guard)
    }
}

// ------------------------------------------------------------------ attempt

/// Coarse; the fine-grained state is the session's. `Finished` keeps the session task's
/// `TaskStatus` in its own column.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AttemptState {
    Starting,
    Running,
    Finished,
    Lost,
    Cancelled,
}

impl AttemptState {
    pub const ALL: [AttemptState; 5] = [
        Self::Starting,
        Self::Running,
        Self::Finished,
        Self::Lost,
        Self::Cancelled,
    ];

    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Finished | Self::Lost | Self::Cancelled)
    }

    pub fn can_transition_to(self, to: Self) -> bool {
        use AttemptState::*;
        matches!(
            (self, to),
            (Starting, Running | Lost | Cancelled) | (Running, Finished | Lost | Cancelled)
        )
    }

    pub fn transition(self, to: Self) -> Result<Self, TransitionError<Self>> {
        run(self, to, self.can_transition_to(to), Ok(()))
    }
}

// ----------------------------------------------------------------- decision

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DecisionState {
    Preparing,
    Pending,
    NeedsMoreInfo,
    Approved,
    Rejected,
    Expired,
    Invalidated,
}

/// Who asks for a transition (ADR-019 section 1: a principal never authorizes itself).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Actor {
    /// A person, through a surface command.
    User,
    /// The orchestrator process (timers, invalidation, preparing briefs).
    Orchestrator,
    /// A running attempt. It has no channel to decisions; every transition refuses it.
    Attempt,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DecisionGuards {
    pub actor: Actor,
    /// Contract version, plan version, subject commit, policy hash and attempt ordinal still equal
    /// the decision's binding.
    pub binding_current: bool,
    /// `expires_at` has passed.
    pub expiry_reached: bool,
}

impl DecisionState {
    pub const ALL: [DecisionState; 7] = [
        Self::Preparing,
        Self::Pending,
        Self::NeedsMoreInfo,
        Self::Approved,
        Self::Rejected,
        Self::Expired,
        Self::Invalidated,
    ];

    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Approved | Self::Rejected | Self::Expired | Self::Invalidated
        )
    }

    pub fn can_transition_to(self, to: Self) -> bool {
        use DecisionState::*;
        matches!(
            (self, to),
            (Preparing, Pending | Invalidated)
                | (
                    Pending,
                    Approved | Rejected | Expired | Invalidated | NeedsMoreInfo
                )
                | (NeedsMoreInfo, Pending | Expired | Invalidated)
        )
    }

    pub fn transition(self, to: Self, g: &DecisionGuards) -> Result<Self, TransitionError<Self>> {
        use DecisionState::*;
        let by = |who: Actor, what| need(g.actor == who, what);
        let guard = match (self, to) {
            // Only a person resolves, on a current binding, before expiry (ADR-019 sections 4, 5).
            (_, Approved | Rejected | NeedsMoreInfo) => by(Actor::User, "only a person resolves")
                .and_then(|()| need(g.binding_current, "binding is stale"))
                .and_then(|()| need(!g.expiry_reached, "decision expired")),
            (_, Expired) => by(Actor::Orchestrator, "only the orchestrator expires")
                .and_then(|()| need(g.expiry_reached, "not expired yet")),
            (_, Invalidated) => by(Actor::Orchestrator, "only the orchestrator invalidates")
                .and_then(|()| need(!g.binding_current, "binding is still current")),
            (_, Pending) => by(Actor::Orchestrator, "only the orchestrator prepares briefs"),
            _ => Ok(()),
        };
        run(self, to, self.can_transition_to(to), guard)
    }
}

#[cfg(test)]
mod tests;
