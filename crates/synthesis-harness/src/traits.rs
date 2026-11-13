//! Seams the interface layer fills in: approvals, user questions and context assembly.
//! Headless implementations never block and never grant anything.

use std::future::Future;
use std::pin::Pin;

use serde_json::Value;
use synthesis_core::WorkspaceContext;
use synthesis_events::ArtifactRef;
use synthesis_permissions::{Mode, Suggestion};
use synthesis_providers::{Message, System};
use synthesis_tools::NeedsUser;
use tokio_util::sync::CancellationToken;

pub type BoxFut<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Shown to the model when a person (or policy) refuses a call. Same text as Node.
pub const DENY_MESSAGE: &str = "The user doesn't want to proceed with this tool use. The tool use was rejected (eg. if it was a file edit, the new_string was NOT written to the file). STOP what you are doing and wait for the user to tell you how to proceed.";

// ---- approvals ----------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct ApprovalRequest {
    pub approval_id: String,
    pub agent_id: Option<String>,
    pub tool_call_id: String,
    pub tool: String,
    pub input: Value,
    /// `permission` or `outside_read`.
    pub dialog: &'static str,
    pub prompt: String,
    pub reason: String,
    pub suggestions: Vec<Suggestion>,
    /// `Some` when the call is work done for another session (messaging.md J4.4): the dialog says
    /// so. Such a request carries no "allow always" suggestions.
    pub origin: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApprovalDecision {
    Allow,
    Deny,
    Expired,
    /// Nobody could be asked (headless).
    AutoDenied,
}

impl ApprovalDecision {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Allow => "allow",
            Self::Deny => "deny",
            Self::Expired => "expired",
            Self::AutoDenied => "auto_denied",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Approval {
    pub decision: ApprovalDecision,
    /// `user`, `policy`, `hook`, `classifier` or `headless`.
    pub reviewer: &'static str,
    /// The approver edited the input: the harness re-runs the policy on it.
    pub updated_input: Option<Value>,
    /// Accepted "allow always" options; only ones that were offered are applied.
    pub accepted: Vec<Suggestion>,
    pub message: Option<String>,
}

impl Approval {
    pub fn allow() -> Self {
        Self {
            decision: ApprovalDecision::Allow,
            reviewer: "user",
            updated_input: None,
            accepted: vec![],
            message: None,
        }
    }

    pub fn deny(message: impl Into<String>) -> Self {
        Self {
            decision: ApprovalDecision::Deny,
            reviewer: "user",
            updated_input: None,
            accepted: vec![],
            message: Some(message.into()),
        }
    }

    pub fn auto_denied() -> Self {
        Self {
            decision: ApprovalDecision::AutoDenied,
            reviewer: "headless",
            updated_input: None,
            accepted: vec![],
            message: Some(
                "Approval is required but no one can approve in this run (headless). The tool was NOT run. Choose an approach that needs no approval, or stop and report what is blocked."
                    .into(),
            ),
        }
    }
}

pub trait Approver: Send + Sync {
    /// The harness races this against cancellation.
    fn approve<'a>(
        &'a self,
        req: ApprovalRequest,
        cancel: &'a CancellationToken,
    ) -> BoxFut<'a, Approval>;
}

/// Headless: every `ask` is auto-denied and the denial goes back to the model as evidence.
pub struct HeadlessApprover;

impl Approver for HeadlessApprover {
    fn approve<'a>(&'a self, _: ApprovalRequest, _: &'a CancellationToken) -> BoxFut<'a, Approval> {
        Box::pin(async { Approval::auto_denied() })
    }
}

// ---- changed files ------------------------------------------------------------------------

/// Workspace diff hook: which files a task changed through tools whose effects the harness
/// cannot read from their input (`Bash`). Implemented over the workspace snapshot/diff in the
/// interface layer; the default reports nothing.
pub trait ChangeTracker: Send + Sync {
    /// Called once when a task starts: take the baseline.
    fn begin<'a>(&'a self, _ws: &'a WorkspaceContext, _task_id: &'a str) -> BoxFut<'a, ()> {
        Box::pin(async {})
    }

    /// Called after every execute-class tool: paths (relative to the workspace root) that differ
    /// from the baseline of `task_id`, cumulative.
    fn changed<'a>(
        &'a self,
        _ws: &'a WorkspaceContext,
        _task_id: &'a str,
    ) -> BoxFut<'a, Vec<String>> {
        Box::pin(async { vec![] })
    }
}

pub struct NoChangeTracker;

impl ChangeTracker for NoChangeTracker {}

// ---- user input (AskUserQuestion / ExitPlanMode) -------------------------------------------

#[derive(Debug, Clone)]
pub enum UserReply {
    /// `answers` object for `AskUserQuestion`.
    Answers(Value),
    /// Verdict on a plan; `mode` is the mode to continue in (default: leave plan mode).
    Plan {
        approved: bool,
        message: Option<String>,
        mode: Option<Mode>,
    },
}

pub trait InputProvider: Send + Sync {
    /// `None` = nobody can answer.
    fn ask<'a>(
        &'a self,
        req: &'a NeedsUser,
        cancel: &'a CancellationToken,
    ) -> BoxFut<'a, Option<UserReply>>;
}

pub struct HeadlessInput;

impl InputProvider for HeadlessInput {
    fn ask<'a>(
        &'a self,
        _: &'a NeedsUser,
        _: &'a CancellationToken,
    ) -> BoxFut<'a, Option<UserReply>> {
        Box::pin(async { None })
    }
}

// ---- context ------------------------------------------------------------------------------

pub struct Built {
    pub system: System,
    pub messages: Vec<Message>,
}

pub struct Compaction {
    pub messages: Vec<Message>,
    pub summary_ref: ArtifactRef,
    pub tokens_before: Option<u64>,
    /// Length of the summary text, for `context.compacted.summary_chars`.
    pub summary_chars: u64,
}

/// Builds what is sent to the model and compacts on demand: before a request that would pass the
/// auto-compact threshold, on a provider context overflow, or when the user asks (`/compact`).
/// `synthesis-context` is wired in behind this trait; [`crate::StaticContext`] is the dependency-free
/// default and cannot compact.
pub trait ContextProvider: Send + Sync {
    /// `agent_prompt` is the sub-agent definition's system prompt (`None` for the main agent).
    fn build(&self, history: &[Message], agent_prompt: Option<&str>) -> Result<Built, String>;

    /// Summarises the head of `history` (the tail stays verbatim, tool pairing stays valid).
    /// `instructions` are extra guidance for the summary (manual `/compact <text>`, PreCompact
    /// hook output); empty for none. Must not touch `history` on failure.
    fn compact<'a>(
        &'a self,
        history: &'a [Message],
        instructions: &'a str,
    ) -> BoxFut<'a, Result<Compaction, String>>;

    /// False when `compact` can never succeed: the harness then skips proactive attempts.
    fn can_compact(&self) -> bool {
        true
    }

    /// Model usage `compact` spent since the last call (summarizer requests, a failed one too);
    /// the harness charges it to the task counters.
    fn take_usage(&self) -> Option<synthesis_providers::Usage> {
        None
    }
}
