//! Settings model for the desktop Settings view (and later the TUI `/config`): a typed catalogue
//! ([`SettingsSchema`]) and a store ([`SettingsStore`]) that shows the effective value and origin of
//! every key and writes the USER settings file atomically. No UI dependencies.
//!
//! Own crate, not a module of `synthesis-settings`: it reuses the validators of `synthesis-harness`,
//! which depends on `synthesis-settings`. MCP servers are not edited here: call
//! `synthesis_app::mcp::{add_server, remove_server}` (credential guards, atomic write).

mod schema;
mod store;

pub use schema::{
    Edit, Field, Kind, ListItem, Scope, Section, SettingsSchema, group_of, validate_kind,
};
pub use store::{Entry, Origin, SetError, SettingsStore};
