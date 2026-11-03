//! Tool trait, registry and the filesystem tools (docs/specs/tools.md).
//!
//! The 20 wire schemas live in `schemas.json`, copied verbatim from `tests/golden/schemas`
//! (the golden test keeps them equal). All are implemented except Task (run by the harness),
//! a schema-only entry whose `execute` returns a `not_implemented` error. `Registry::with_new_tools` adds the terminal_*, browser_*, verify_run and artifact_read tools.

use std::future::Future;
use std::pin::Pin;

use serde::Serialize;
use serde_json::Value;
use synthesis_core::ToolContext;

mod artifact;
mod browser;
mod desktop;
mod diff;
/// Line diff of a file change, for the approval dialog preview.
pub use diff::for_file as file_diff;
mod edit;
mod glob;
mod grep;
mod native;
mod notebook;
mod pdf;
mod read;
mod registry;
mod skill;
mod tasks;
mod terminal;
mod util;
mod verify;
mod web;
mod write;

pub use browser::{BrowserConfig, BrowserHost, LoginRequest};
pub use desktop::{DesktopAudit, DesktopConfig, DesktopHost};
/// Tool input safe for events, hooks, approval prompts and history: the secrets of
/// `browser_fill sensitive` and the `value` of `desktop_act` removed.
pub fn redact_input(tool: &str, input: &Value) -> Value {
    desktop::redact_input(tool, &browser::redact_input(tool, input))
}
pub use registry::{Registry, standard_policy, standard_policy_git, standard_policy_with};
pub use skill::{
    LearnedProvider, LearnedSkill, SkillInfo, SkillSource, discover_skills, set_learned_skills,
};
pub use synthesis_terminal::OutputSink;
pub use tasks::{NeedsUser, Question, QuestionOption};
pub use terminal::{LIVE_OUTPUT, UNCONFINED};
pub use web::WebConfig;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SideEffectClass {
    Read,
    Write,
    Execute,
    External,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolRuntime {
    Builtin,
    /// Run by the agent loop itself (Task, AskUserQuestion, ExitPlanMode).
    Harness,
    Mcp {
        server: String,
    },
}

#[derive(Debug, Clone)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    /// JSON Schema object, exactly as sent to providers.
    pub input_schema: Value,
    pub side_effect: SideEffectClass,
    pub runtime: ToolRuntime,
}

/// Wire shape of an Anthropic tool_result block list.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Block {
    Text { text: String },
    Image { source: ImageSource },
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ImageSource {
    Base64 { media_type: String, data: String },
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(untagged)]
pub enum Content {
    Text(String),
    Blocks(Vec<Block>),
}

/// Expected failures are `is_error: true`, never a Rust `Err` (tools.md §1 rule 1).
#[derive(Debug, Clone, PartialEq)]
pub struct ToolOutput {
    pub content: Content,
    pub is_error: bool,
    /// Refusal / not applicable: drawn neutral, the model just retries differently.
    pub soft: bool,
    /// e.g. `{"replacements":2}`; `Null` when absent.
    pub meta: Value,
}

impl ToolOutput {
    pub fn text(s: impl Into<String>) -> Self {
        Self {
            content: Content::Text(s.into()),
            is_error: false,
            soft: false,
            meta: Value::Null,
        }
    }
    pub fn error(s: impl Into<String>) -> Self {
        Self {
            is_error: true,
            ..Self::text(s)
        }
    }
    pub fn soft_error(s: impl Into<String>) -> Self {
        Self {
            soft: true,
            ..Self::error(s)
        }
    }
    pub fn with_meta(mut self, meta: Value) -> Self {
        self.meta = meta;
        self
    }
}

pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

pub trait Tool: Send + Sync {
    fn spec(&self) -> &ToolSpec;
    fn execute<'a>(&'a self, input: Value, ctx: &'a ToolContext) -> BoxFuture<'a, ToolOutput>;
}
