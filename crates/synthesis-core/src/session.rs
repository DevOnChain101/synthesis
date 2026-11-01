use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Instant, SystemTime};

use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

use crate::WorkspaceContext;
use synthesis_events::Budget;

pub type SessionId = String;
pub type TaskId = String;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionStatus {
    Created,
    Preparing,
    Idle,
    Running,
    WaitingApproval,
    Verifying,
    Completed,
    Unverified,
    Blocked,
    Failed,
    Cancelling,
    Cancelled,
    Interrupted,
    Recovering,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InvalidTransition {
    pub from: SessionStatus,
    pub to: SessionStatus,
}

impl std::fmt::Display for InvalidTransition {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "invalid session transition {:?} -> {:?}",
            self.from, self.to
        )
    }
}
impl std::error::Error for InvalidTransition {}

impl SessionStatus {
    /// Terminal per task (the session itself ends with `session.*`).
    pub fn is_task_terminal(self) -> bool {
        use SessionStatus::*;
        matches!(self, Completed | Unverified | Blocked | Failed | Cancelled)
    }

    /// Spec §3 table; anything not listed is rejected.
    pub fn can_transition_to(self, to: SessionStatus) -> bool {
        use SessionStatus::*;
        match (self, to) {
            (Created, Preparing)
            | (Preparing, Idle | Running)
            | (Idle, Running)
            | (Running, WaitingApproval | Verifying | Failed | Blocked)
            | (WaitingApproval, Running | Verifying)
            | (Verifying, WaitingApproval | Running | Completed | Unverified | Blocked | Failed)
            | (Running | WaitingApproval | Verifying, Cancelling | Interrupted)
            | (Cancelling, Cancelled | Interrupted)
            | (Interrupted, Recovering)
            | (Recovering, Running | Blocked) => true,
            (from, Idle) => from.is_task_terminal(),
            _ => false,
        }
    }

    pub fn transition(self, to: SessionStatus) -> Result<SessionStatus, InvalidTransition> {
        if self.can_transition_to(to) {
            Ok(to)
        } else {
            Err(InvalidTransition { from: self, to })
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")] // wire values identical to Node
pub enum PermissionMode {
    Default,
    AcceptEdits,
    Plan,
    Auto,
    DontAsk,
    BypassPermissions,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct BudgetUsage {
    pub turns: u32,
    pub tool_calls: u32,
    pub cost_usd: f64,
    pub wall_ms: u64,
    pub verify_attempts: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ParentRef {
    pub session_id: SessionId,
    pub tool_call_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentSession {
    pub id: SessionId,
    pub task_id: Option<TaskId>,
    pub status: SessionStatus,
    pub workspace: WorkspaceContext,
    pub model_config_id: String,
    pub permission_policy_id: String,
    pub permission_mode: PermissionMode,
    pub event_cursor: u64,
    pub budget: Budget,
    pub usage: BudgetUsage,
    pub created_at: String,
    pub updated_at: String,
    pub parent: Option<ParentRef>,
}

impl AgentSession {
    /// Applies a transition; returns `(from, to)` for the caller to emit `session.status_changed`.
    pub fn transition(
        &mut self,
        to: SessionStatus,
    ) -> Result<(SessionStatus, SessionStatus), InvalidTransition> {
        let from = self.status;
        self.status = from.transition(to)?;
        Ok((from, to))
    }
}

/// Everything a tool may know about location and identity; tools never read process cwd/env.
/// `Clone` is cheap (shared `Arc`s) and lets tools move file IO onto `spawn_blocking`.
#[derive(Clone)]
pub struct ToolContext {
    pub session_id: SessionId,
    pub task_id: TaskId,
    pub turn_id: String,
    pub tool_call_id: String,
    pub workspace: Arc<WorkspaceContext>,
    pub env: Arc<BTreeMap<String, String>>,
    pub read_ledger: Arc<Mutex<HashMap<PathBuf, SystemTime>>>,
    pub permission_mode: PermissionMode,
    pub approval_token: Option<String>,
    pub deadline: Option<Instant>,
    pub cancel: CancellationToken,
    /// Tree-walking readers (Grep) skip secret-shaped files and links to them (peer responder).
    pub skip_secrets: bool,
    /// A sub-agent made the call (memory.md §4: it sees pinned and its own notes, not the workspace).
    pub sub_agent: bool,
}

#[cfg(test)]
mod tests {
    use super::SessionStatus::{self, *};

    const ALL: [SessionStatus; 14] = [
        Created,
        Preparing,
        Idle,
        Running,
        WaitingApproval,
        Verifying,
        Completed,
        Unverified,
        Blocked,
        Failed,
        Cancelling,
        Cancelled,
        Interrupted,
        Recovering,
    ];

    #[test]
    fn valid_transitions() {
        let valid = [
            (Created, Preparing),
            (Preparing, Idle),
            (Preparing, Running),
            (Idle, Running),
            (Running, WaitingApproval),
            (WaitingApproval, Running),
            (Running, Verifying),
            (Verifying, Running),
            (Verifying, WaitingApproval),
            (WaitingApproval, Verifying),
            (Verifying, Completed),
            (Verifying, Unverified),
            (Verifying, Blocked),
            (Verifying, Failed),
            (Running, Failed),
            (Running, Blocked),
            (Running, Cancelling),
            (WaitingApproval, Cancelling),
            (Verifying, Cancelling),
            (Cancelling, Cancelled),
            (Cancelling, Interrupted),
            (Running, Interrupted),
            (WaitingApproval, Interrupted),
            (Verifying, Interrupted),
            (Interrupted, Recovering),
            (Recovering, Running),
            (Recovering, Blocked),
            (Completed, Idle),
            (Unverified, Idle),
            (Blocked, Idle),
            (Failed, Idle),
            (Cancelled, Idle),
        ];
        for (a, b) in valid {
            assert_eq!(a.transition(b), Ok(b), "{a:?}->{b:?}");
        }
    }

    #[test]
    fn everything_else_is_rejected() {
        let valid_count = ALL
            .iter()
            .flat_map(|&a| ALL.iter().map(move |&b| (a, b)))
            .filter(|&(a, b)| a.can_transition_to(b))
            .count();
        assert_eq!(valid_count, 32);
        for (a, b) in [
            (Created, Running),
            (Idle, Completed),
            (Running, Idle),
            (Running, Cancelled),
            (Cancelling, Running),
            (Completed, Running),
            (Interrupted, Running),
            (Idle, Idle),
            (Cancelled, Created),
            (Recovering, Failed),
        ] {
            let err = a.transition(b).unwrap_err();
            assert_eq!((err.from, err.to), (a, b));
        }
    }
}
