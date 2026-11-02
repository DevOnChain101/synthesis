//! Long-term memory (docs/specs/memory.md): a SQLite store with FTS5 retrieval, trust computed from
//! evidence, a poisoning scanner and secret masking. Memory is DATA: nothing here grants a
//! permission or reaches the permission engine. The crate reads no environment variables; the
//! caller passes the home directory and the exact secret values to mask.

mod consolidate;
mod jobs;
mod reflect;
mod retrieve;
mod scan;
mod skills;
mod store;

pub use consolidate::*;
pub use jobs::*;
pub use reflect::*;
pub use retrieve::*;
pub use scan::{masked_fraction, scan};
pub use skills::*;
pub use store::*;
pub use time::OffsetDateTime;

use serde::{Deserialize, Serialize};

#[derive(Debug)]
pub struct Error(pub String);

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for Error {}
impl From<rusqlite::Error> for Error {
    fn from(e: rusqlite::Error) -> Self {
        if e.sqlite_error_code() == Some(rusqlite::ErrorCode::ReadOnly) {
            return Self("memory is read-only: its schema is newer than this build; upgrade Synthesis to change it".into());
        }
        Self(format!("memory store: {e}"))
    }
}
impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Self(format!("memory store: {e}"))
    }
}
impl From<serde_json::Error> for Error {
    fn from(e: serde_json::Error) -> Self {
        Self(format!("memory store: {e}"))
    }
}

pub type Result<T> = std::result::Result<T, Error>;

/// Computed by the harness from the evidence, never chosen by the model (spec §5).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Trust {
    Untrusted,
    Agent,
    Verified,
    Owner,
}

impl Trust {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Untrusted => "untrusted",
            Self::Agent => "agent",
            Self::Verified => "verified",
            Self::Owner => "owner",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        [Self::Untrusted, Self::Agent, Self::Verified, Self::Owner]
            .into_iter()
            .find(|t| t.as_str() == s)
    }
}

pub const KINDS: [&str; 5] = ["episode", "fact", "preference", "lesson", "skill"];
pub const SCOPES: [&str; 3] = ["user", "workspace", "agent"];
pub const STATUSES: [&str; 6] = [
    "active",
    "proposed",
    "quarantined",
    "stale",
    "superseded",
    "archived",
];
