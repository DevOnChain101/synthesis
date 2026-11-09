//! Agent harness (R4e): the model/tool loop, stop conditions, session/task state machine,
//! sub-agents and the completion gate hand-off. A library: interfaces plug in through the
//! [`Approver`], [`InputProvider`] and [`ContextProvider`] traits (headless defaults included).
//!
//! # Context compaction
//!
//! Before every model request the harness estimates the context size (provider-reported input
//! tokens of the previous response plus a calibrated estimate for what was added) and, from
//! `HarnessConfig::auto_compact` ([`AutoCompact`]: threshold x window), runs
//! `ContextProvider::compact` and continues the same turn. A provider context overflow still
//! compacts reactively (once per task). Every path emits `context.compacting`,
//! `context.compacted` + `context.usage` (post-compaction estimate) or `context.compaction_failed`;
//! every response emits `context.usage`.
//!
//! Manual compaction (`/compact [instructions]`):
//! - task running: [`Harness::request_compact`]`(Option<String>)`, callable from any thread; it
//!   runs at the next safe point (before the next model request of the main agent, so no
//!   `tool_use` is left without its result). A request made when no task runs waits for the
//!   next task's first safe point.
//! - idle: `Harness::compact_now(&session, history, instructions, cancel).await` returns the
//!   compacted history to pass as the next `TaskRequest.history`.
//!
//! The model's final message never completes a task: it is a proposal that goes through
//! `CompletionGate::evaluate` (design doc §6.2).

mod agent;
mod agents;
mod classifier;
mod comm;
mod context;
mod exec;
mod harness;
mod hub;
mod live;
mod mcp_guard;
mod peer;
mod sink;
pub mod testing;
mod toolbox;
mod toolsearch;
mod traits;
mod user_cmd;
mod util;

pub use agents::{
    AgentDef, AgentSource, AgentTable, GENERAL_PURPOSE, ModelResolver, from_mode, stricter, to_mode,
};
pub use classifier::AutoJudge;
pub use comm::{AgentInfo, NO_TASK_RUNNING, TaskAgent};
pub use context::{RULES, StaticContext, wrap_tool_output};
pub use harness::{
    AutoCompact, GateFactory, Harness, HarnessConfig, HarnessError, ImageAttachment, ModeError,
    Pricing, ShellSettings, TaskOutcome, TaskRequest, ToolSearchMode, VerifiedCommand,
    browser_config, budget_from_settings,
};
pub use mcp_guard::{Check, GuardConfig, McpCatalog, McpGuard, McpToolInfo, ServerRules};
pub use peer::PeerLink;
pub use sink::{EventSink, Scope};
pub use toolbox::{ToolBox, ToolSource, validate};
pub use traits::*;
pub use user_cmd::{ToolOutcome, user_command_note};
pub use util::{is_invisible_or_blank, sanitize_for_display};
