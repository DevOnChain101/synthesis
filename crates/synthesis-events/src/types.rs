use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Envelope. `type` + `payload` come from the flattened [`Event`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Envelope {
    pub v: u16,
    pub seq: u64,
    pub ts: String,
    pub session_id: String,
    pub task_id: Option<String>,
    pub turn_id: Option<String>,
    #[serde(default)]
    pub agent_id: Option<String>,
    #[serde(flatten)]
    pub event: Event,
}

/// Same envelope with an untyped `type`/`payload`; used for unknown event types.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RawEnvelope {
    pub v: u16,
    pub seq: u64,
    pub ts: String,
    pub session_id: String,
    pub task_id: Option<String>,
    pub turn_id: Option<String>,
    #[serde(default)]
    pub agent_id: Option<String>,
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(default)]
    pub payload: Value,
}

/// An agent in this or another session (messaging.md B10). `workspace_root` only in peer events.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PeerAddr {
    pub workspace_key: String,
    pub workspace_label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_root: Option<String>,
    pub session_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<String>,
    pub instance: String,
    #[serde(default)]
    pub kind: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ArtifactRef {
    pub artifact_id: String,
    /// Relative to the session artifact root.
    pub path: String,
    pub size: u64,
    pub sha256: String,
    pub mime: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_read_input_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_creation_input_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost_usd: Option<f64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Budget {
    pub max_turns: u32,
    pub max_tool_calls: u32,
    pub max_cost_usd: Option<f64>,
    pub max_wall_ms: Option<u64>,
    pub max_verify_attempts: u32,
}

impl Default for Budget {
    fn default() -> Self {
        Self {
            max_turns: 50,
            max_tool_calls: 200,
            max_cost_usd: None,
            max_wall_ms: None,
            max_verify_attempts: 3,
        }
    }
}

/// Open enum: unknown values deserialize as `Other` (spec §7).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolStatus {
    Ok,
    Error,
    SoftError,
    Denied,
    Interrupted,
    Timeout,
    BudgetExceeded,
    #[serde(other)]
    Other,
}

/// Task terminal status (`task.ended.status`). Open enum.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    Completed,
    Unverified,
    Blocked,
    Failed,
    Cancelled,
    #[serde(other)]
    Other,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorkspaceInfo {
    pub workspace_id: String,
    /// Stable across sessions (additive; absent in older logs).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_key: Option<String>,
    pub root: String,
    pub cwd: String,
    pub additional_dirs: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git_worktree: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Criterion {
    pub id: String,
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub check: Option<String>,
}

/// Payload of `session.completed|failed|cancelled`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionEnd {
    pub reason: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Payload of `process.spawned|ready|exited`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProcessInfo {
    pub process_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pid: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signal: Option<String>,
}

/// Payload of `subagent.started|stopped`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SubagentInfo {
    pub agent_id: String,
    pub agent_type: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// Instance name (unique within the session); absent in older logs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Started with `run_in_background`.
    #[serde(default, skip_serializing_if = "is_false")]
    pub background: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Todo {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    pub content: String,
    pub status: String,
    pub active_form: String,
}

fn is_false(b: &bool) -> bool {
    !*b
}

fn is_zero(n: &u64) -> bool {
    *n == 0
}

/// Event taxonomy, adjacently tagged: `{"type": "...", "payload": {...}}`.
/// Fields documented as enums are `String` unless they are `ToolStatus`/`TaskStatus`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", content = "payload")]
pub enum Event {
    #[serde(rename = "session.started")]
    SessionStarted {
        workspace: WorkspaceInfo,
        model: String,
        /// Provider id the model is served by (additive; absent in older logs).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        provider: Option<String>,
        permission_mode: String,
        source: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        resumed_from_seq: Option<u64>,
    },
    #[serde(rename = "session.status_changed")]
    SessionStatusChanged {
        from: String,
        to: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
    },
    #[serde(rename = "session.completed")]
    SessionCompleted(SessionEnd),
    #[serde(rename = "session.failed")]
    SessionFailed(SessionEnd),
    #[serde(rename = "session.cancelled")]
    SessionCancelled(SessionEnd),
    #[serde(rename = "task.started")]
    TaskStarted {
        request: String,
        acceptance_criteria: Vec<Criterion>,
        budget: Budget,
    },
    #[serde(rename = "task.ended")]
    TaskEnded {
        status: TaskStatus,
        reason: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        verification_report_id: Option<String>,
        usage: Usage,
        changed_files: Vec<String>,
        artifacts_root: String,
        /// Permission rules (`Bash(cargo test:*)`) the task needed but could not get because
        /// nobody could approve; the host may offer them (`--allow`).
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        needed_permissions: Vec<String>,
    },
    #[serde(rename = "user.message")]
    UserMessage {
        text: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        display_text: Option<String>,
        images: Vec<ArtifactRef>,
    },
    #[serde(rename = "turn.started")]
    TurnStarted {
        model: String,
        provider: String,
        iteration: i64,
    },
    #[serde(rename = "model.delta")]
    ModelDelta {
        block_index: i64,
        kind: String,
        text: String,
    },
    #[serde(rename = "assistant.message")]
    AssistantMessage {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        message_id: Option<String>,
        content: Vec<Value>,
        stop_reason: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        usage: Option<Usage>,
    },
    #[serde(rename = "turn.completed")]
    TurnCompleted {
        stop_reason: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        usage: Option<Usage>,
        duration_ms: u64,
    },
    #[serde(rename = "model.retry")]
    ModelRetry {
        attempt: i64,
        max: i64,
        error: String,
        delay_ms: u64,
    },
    #[serde(rename = "model.error")]
    ModelError {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        status: Option<i64>,
        message: String,
        retryable: bool,
        context_overflow: bool,
    },
    #[serde(rename = "context.compacted")]
    ContextCompacted {
        /// Legacy: `"auto"` (also for overflow) or `"manual"`. Prefer `reason`.
        trigger: String,
        summary_ref: ArtifactRef,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tokens_before: Option<u64>,
        /// `"auto"` | `"overflow"` | `"manual"` (open; empty in logs written before it existed).
        #[serde(default)]
        reason: String,
        #[serde(default)]
        before_tokens: u64,
        /// Estimate of the compacted request (not provider-reported).
        #[serde(default)]
        after_tokens: u64,
        #[serde(default)]
        window_tokens: u64,
        #[serde(default)]
        messages_before: u64,
        #[serde(default)]
        messages_after: u64,
        #[serde(default)]
        summary_chars: u64,
        #[serde(default)]
        duration_ms: u64,
    },
    /// Context size after each model response and right after each compaction.
    #[serde(rename = "context.usage")]
    ContextUsage {
        used_tokens: u64,
        window_tokens: u64,
        /// 0..=100 (may exceed 100 when the estimate is over the window).
        percent: f64,
        /// `"provider"` (reported by the model API) | `"estimate"`.
        source: String,
        /// True when the number predates the latest history change (no usage in the response).
        stale: bool,
    },
    #[serde(rename = "context.compacting")]
    ContextCompacting {
        /// `"auto"` | `"overflow"` | `"manual"`.
        reason: String,
        used_tokens: u64,
        window_tokens: u64,
    },
    #[serde(rename = "context.compaction_failed")]
    ContextCompactionFailed {
        reason: String,
        /// Redacted by the sink like every payload string.
        error: String,
    },
    #[serde(rename = "tool.requested")]
    ToolRequested {
        tool_call_id: String,
        name: String,
        input: Value,
        input_truncated: bool,
        /// `"user"` for a command the person typed (`!cmd`); absent for the model's calls.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        origin: Option<String>,
    },
    #[serde(rename = "tool.approval_required")]
    ToolApprovalRequired {
        approval_id: String,
        tool_call_id: String,
        dialog: String,
        prompt: String,
        options: Vec<String>,
        suggestions: Vec<Value>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        deadline: Option<String>,
    },
    #[serde(rename = "tool.approval_resolved")]
    ToolApprovalResolved {
        approval_id: String,
        decision: String,
        reviewer: String,
        input_changed: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        message: Option<String>,
    },
    #[serde(rename = "tool.started")]
    ToolStarted {
        tool_call_id: String,
        name: String,
        side_effect: String,
        cwd: String,
    },
    #[serde(rename = "tool.completed")]
    ToolCompleted {
        tool_call_id: String,
        name: String,
        status: ToolStatus,
        is_error: bool,
        not_run: bool,
        duration_ms: u64,
        preview: String,
        bytes: u64,
        truncated: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        result_ref: Option<ArtifactRef>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        origin: Option<String>,
        /// The model-facing result text, whole up to 64 KiB (redacted like every payload).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        output: Option<String>,
        /// `output` is a cut of a longer result; `result_ref` names the whole.
        #[serde(default, skip_serializing_if = "is_false")]
        output_truncated: bool,
        /// Size of the whole result in bytes (0 = not reported).
        #[serde(default, skip_serializing_if = "is_zero")]
        output_bytes: u64,
        /// Structured line diff of a file change (Edit / Write / NotebookEdit), from the tool's
        /// `meta.diff`: `{path, kind, added, removed, hunks[{old_start, old_lines, new_start,
        /// new_lines, lines[{tag, old_no?, new_no?, text}]}], truncated, hidden?}`. At most about
        /// 64 KiB of line text; redacted like every payload. Absent for other tools and old logs.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        diff: Option<serde_json::Value>,
    },
    /// Live output of a running command (Bash), coalesced; the whole log is an artifact.
    #[serde(rename = "tool.output")]
    ToolOutput {
        tool_call_id: String,
        /// `"stdout"` | `"stderr"` | `"merged"`.
        stream: String,
        text: String,
        /// 1, 2, ... per tool call.
        seq: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        origin: Option<String>,
    },
    #[serde(rename = "hook.ran")]
    HookRan {
        event: String,
        hook_name: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        exit_code: Option<i64>,
        outcome: String,
        duration_ms: u64,
    },
    #[serde(rename = "process.spawned")]
    ProcessSpawned(ProcessInfo),
    #[serde(rename = "process.ready")]
    ProcessReady(ProcessInfo),
    #[serde(rename = "process.exited")]
    ProcessExited(ProcessInfo),
    #[serde(rename = "artifact.created")]
    ArtifactCreated {
        #[serde(flatten)]
        artifact: ArtifactRef,
        kind: String,
        retention: String,
    },
    #[serde(rename = "verification.completed")]
    VerificationCompleted {
        report_id: String,
        status: String,
        attempt: i64,
        summary: String,
    },
    #[serde(rename = "todo.updated")]
    TodoUpdated { todos: Vec<Todo> },
    #[serde(rename = "subagent.started")]
    SubagentStarted(SubagentInfo),
    #[serde(rename = "subagent.stopped")]
    SubagentStopped(SubagentInfo),
    /// `running | waiting_approval | idle | cancelled | evicted`.
    #[serde(rename = "subagent.status_changed")]
    SubagentStatusChanged {
        agent_id: String,
        name: String,
        from: String,
        to: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
    },
    /// A message was accepted into an inbox. `from` / `to` are instance names (`main`, `user`).
    #[serde(rename = "message.sent")]
    MessageSent {
        message_id: String,
        from: String,
        to: String,
        origin: String,
        kind: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        summary: Option<String>,
        text: String,
        bytes: u64,
        #[serde(default, skip_serializing_if = "is_false")]
        notify_when_idle: bool,
    },
    /// `at` = `tool_round | turn_end | wake`.
    #[serde(rename = "message.delivered")]
    MessageDelivered { message_id: String, at: String },
    #[serde(rename = "message.dropped")]
    MessageDropped { message_id: String, reason: String },
    /// The session's peer endpoint is up / down. Never carries the path or pipe name.
    #[serde(rename = "peer.listening")]
    PeerListening { endpoint_kind: String },
    #[serde(rename = "peer.stopped")]
    PeerStopped { endpoint_kind: String },
    /// This session sent a request to a peer session.
    #[serde(rename = "peer.request_sent")]
    PeerRequestSent {
        request_id: String,
        from: PeerAddr,
        to: PeerAddr,
        summary: String,
        bytes: u64,
        chain_len: u64,
    },
    /// A peer asked this session; `decision` = `accepted | rejected`, `reason` the rejection.
    #[serde(rename = "peer.request_received")]
    PeerRequestReceived {
        request_id: String,
        from: PeerAddr,
        to: PeerAddr,
        summary: String,
        bytes: u64,
        decision: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
    },
    #[serde(rename = "peer.reply_partial")]
    PeerReplyPartial {
        request_id: String,
        seq: u64,
        text: String,
    },
    /// `status` = `answered | declined | failed | timeout | cancelled | offline`.
    #[serde(rename = "peer.reply")]
    PeerReply {
        request_id: String,
        status: String,
        text: String,
        bytes: u64,
        duration_ms: u64,
    },
    /// A thread of peer rounds ended: `reason` = `cap | ttl | cancel | size | budget`.
    #[serde(rename = "peer.thread_closed")]
    PeerThreadClosed { thread_id: String, reason: String },
    /// This session accepted a delegated request (messaging.md J). `from` is self-asserted;
    /// `mode` is the mode the worker runs in, `cap` the sender's mode it was capped by.
    #[serde(rename = "peer.delegation.accepted")]
    PeerDelegationAccepted {
        delegation_id: String,
        request_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        thread_id: Option<String>,
        from: PeerAddr,
        mode: String,
        cap: String,
    },
    #[serde(rename = "peer.delegation.started")]
    PeerDelegationStarted {
        delegation_id: String,
        agent_id: String,
    },
    /// One tool call made for a peer. `origin` is always `peer`; `from` is the canonical sender
    /// address (unverified).
    #[serde(rename = "peer.delegation.tool")]
    PeerDelegationTool {
        delegation_id: String,
        tool_call_id: String,
        name: String,
        origin: String,
        from: String,
    },
    /// `status` = `answered | declined | failed | cancelled | timeout`.
    #[serde(rename = "peer.delegation.completed")]
    PeerDelegationCompleted {
        delegation_id: String,
        status: String,
        tool_calls: u64,
        duration_ms: u64,
    },
    /// A delegated request was refused before any work (`reason` as `peer.request_received`).
    #[serde(rename = "peer.delegation.denied")]
    PeerDelegationDenied {
        request_id: String,
        from: PeerAddr,
        reason: String,
    },
    /// The permission mode changed (front-end switch). Never emitted for the mode a session starts in.
    #[serde(rename = "session.mode_changed")]
    SessionModeChanged {
        from: String,
        to: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
    },
    /// Auto mode: what the judge model decided about one pending call.
    #[serde(rename = "permission.auto_verdict")]
    AutoVerdict {
        tool_call_id: String,
        name: String,
        /// `"allow"` | `"deny"` | `"ask"`.
        verdict: String,
        reason: String,
        /// `"model"` | `"cache"` | `"fail_closed"` (judge unavailable, unparseable, over the cap).
        source: String,
    },
    #[serde(rename = "model.changed")]
    ModelChanged {
        model: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        effort: Option<String>,
    },
    #[serde(rename = "budget.exceeded")]
    BudgetExceeded {
        budget: String,
        limit: f64,
        used: f64,
    },
    /// Long-term memory (memory.md §11): the notes injected at task start. No text, ids only.
    #[serde(rename = "memory.retrieved")]
    MemoryRetrieved {
        ids: Vec<String>,
        tokens: u64,
        candidates: u64,
    },
    /// A record was written; `op` = `added | deduped | superseded | proposed | quarantined |
    /// updated | archived`.
    #[serde(rename = "memory.written")]
    MemoryWritten {
        id: String,
        op: String,
        kind: String,
        scope: String,
        trust: String,
        status: String,
    },
    #[serde(rename = "memory.quarantined")]
    MemoryQuarantined { id: String, rule: String },
    #[serde(rename = "memory.reflection.started")]
    MemoryReflectionStarted { job_id: String },
    /// `ops` = records the reflection saved or queued for review. `estimated` is true when the
    /// provider sent no usage and `tokens` (and `cost_usd`) come from the token estimator.
    #[serde(rename = "memory.reflection.completed")]
    MemoryReflectionCompleted {
        job_id: String,
        ops: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tokens: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cost_usd: Option<f64>,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        estimated: bool,
    },
    #[serde(rename = "memory.reflection.failed")]
    MemoryReflectionFailed { job_id: String, error: String },
    #[serde(rename = "memory.proposal.created")]
    MemoryProposalCreated { id: String },
    /// `browser_login` opened a visible browser on `origin` and waits for the person to log in and
    /// confirm. Names only: no cookie, token or page content ever appears in an event.
    #[serde(rename = "browser.login_required")]
    BrowserLoginRequired { origin: String, profile: String },
    /// `desktop_launch` started an app (docs/DESKTOP-CONTROL.md). Names and ids only.
    #[serde(rename = "desktop.launch")]
    DesktopLaunch {
        app_id: String,
        pid: u64,
        name: String,
        /// `"project"` (launched from the workspace) or `"external"` (approved by the person).
        origin: String,
        /// `"none"|"weak"|"strong"`: GUI guarantee of the sandbox the app was spawned under.
        #[serde(default)]
        gui: String,
    },
    /// A `desktop_act` call. The value that was typed or set is never part of the event.
    #[serde(rename = "desktop.act")]
    DesktopAct {
        app_id: String,
        action: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        r#ref: Option<String>,
        /// agent-desktop's `disposition.delivery`, or `"failed"`.
        delivery: String,
    },
    /// A window screenshot was saved (path of the artifact, never the pixels).
    #[serde(rename = "desktop.screenshot")]
    DesktopScreenshot {
        app_id: String,
        window_id: String,
        path: String,
    },
    /// A desktop call was refused by policy (hard-deny list, mode, peer or sub-agent, scope).
    #[serde(rename = "desktop.denied")]
    DesktopDenied { tool: String, reason: String },
    #[serde(rename = "notice")]
    Notice { level: String, text: String },
}
