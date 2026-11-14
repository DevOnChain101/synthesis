//! Typed event stream (docs/specs/events.md): envelope, taxonomy, JSONL writer/reader.
//!
//! Versioning (spec §7): additive changes keep `SCHEMA_VERSION`; consumers ignore unknown
//! fields and types. Removing/renaming/retyping a field bumps it.

mod io;
pub mod outcome;
mod types;

pub use io::{EventReader, EventWriter, Record, Redactor};
pub use types::*;

/// Schema major version written to `v`.
pub const SCHEMA_VERSION: u16 = 1;
