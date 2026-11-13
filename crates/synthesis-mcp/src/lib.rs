//! MCP client manager on the official `rmcp` crate (stdio child process + streamable HTTP).

mod config;
mod guard;
mod manager;
mod naming;
mod result;
mod sse;
pub use reqwest::Url;
pub use sse::check_remote_url;
mod ws;

pub use config::*;
pub use manager::*;
pub use naming::{MAX_TOOL_NAME_LEN, parse_tool_name_fallback, tool_name_for, tool_name_hashed};
pub use result::{MAX_TEXT_LEN, McpContent, McpToolResult, fit, normalize_result, normalize_with};
