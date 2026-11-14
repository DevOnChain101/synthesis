//! Durable session store: SQLite (WAL) plus Node transcript compatibility
//! (docs/specs/session-store.md). One writer per session, any number of readers.
//!
//! Everything is rooted at an injected `synthesis_home`; no ambient env or process cwd is read.

mod clean;
mod error;
pub mod node;
pub mod outcome;
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
/// The schema a default open produces (outcomes are opt-in: this stays 4 until they are enabled).
pub use schema::DEFAULT_TARGET as SCHEMA_VERSION_LATEST;
/// Newest schema this build reads and writes; a database above it is refused (`SchemaTooNew`).
pub use schema::LATEST as SCHEMA_VERSION_SUPPORTED;
/// `schema_migrations.version` marking the outcome DDL revision this build writes.
pub use schema::OUTCOME_DDL_MARKER;
/// First schema with the outcome tables; opened only through the explicit enable step.
pub use schema::OUTCOMES as SCHEMA_VERSION_OUTCOMES;
pub use store::*;
