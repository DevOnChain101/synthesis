//! Durable session store: SQLite (WAL) plus Node transcript compatibility
//! (docs/specs/session-store.md). One writer per session, any number of readers.
//!
//! Everything is rooted at an injected `synthesis_home`; no ambient env or process cwd is read.

mod clean;
mod error;
pub mod node;
mod recover;
mod schema;
mod store;
pub mod trust;

pub use clean::{CleanCounts, CleanPlan, CleanResult, CleanScope, INACTIVE_DAYS};
pub use error::{Error, Result};
pub use recover::{
    Orphan, OrphanAction, OrphanState, OsProbe, ProcessProbe, RecoveryReport, process_start_time,
    recover, recover_with,
};
pub use schema::LATEST as SCHEMA_VERSION_LATEST;
pub use store::*;
