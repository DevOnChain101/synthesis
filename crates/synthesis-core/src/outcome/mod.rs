//! Outcome orchestration domain (ADR-016..020, docs/architecture/outcome-orchestration-plan.md PR-01).
//!
//! Pure logic only: identifiers, state machines with guards, the contract and its authority
//! envelope, budgets, and plan validation. No I/O, no clock, no randomness; storage lives in
//! `synthesis-session`, behaviour in the (later) orchestrator crate. "Work item" is the DAG node;
//! a "task" stays one prompt-run inside a session (`TaskId`, unchanged).
//!
//! Everything here is inert unless `synt.orchestrator.enabled` is on (ADR-017, plan D1).

mod contract;
mod ids;
mod plan;
mod state;

pub use contract::*;
pub use ids::*;
pub use plan::*;
pub use state::*;
