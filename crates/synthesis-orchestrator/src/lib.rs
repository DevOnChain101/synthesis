//! Outcome orchestrator (ADR-017). PR-04 content: allocating an attempt's execution environment.
//!
//! One attempt, one worktree on branch `synthesis/<session_id>`, one session, one explicit cwd
//! built here from the outcome's recorded repository and base commit (never from a surface, never
//! from the process cwd). The allocation is an outbox effect (`worktree.create`) with the
//! probe-before-rerun rule, so a crash at any point is reconciled without a second worktree.
//!
//! Everything is off unless `synt.orchestrator.enabled` and the store was migrated to v5; this
//! crate holds no switch of its own, the store's `OutcomePolicy` gate does.

mod alloc;
mod decision;
mod envelope;
mod host;
mod planner;
mod proc;
mod profile;
mod recover;
mod retention;
mod runner;
mod sched;

pub use alloc::{AllocError, AllocSpec, Allocator, AttemptEnv, Platform};
pub use decision::record_blocked_decision;
pub use envelope::{CompileError, CompiledEnvelope, Network, compile_envelope};
pub use host::ProcessHost;
pub use planner::{Planner, PlannerError, Replan, StubPlanner, make_plan, replan};
pub use proc::{ChildState, StateFn, kill_verified};
pub use profile::{DEFAULT_WALL_SECS, Profile, ProfileError};
pub use recover::{RecoveryReport, recover_attempts};
pub use retention::{Cleanup, cleanup_attempt, finish_attempt, retention_due};
pub use runner::{
    CancelFlag, RunError, Runner, Running, Settled, Spawn, SyntSpawn, WorkerEnd, map_exit,
    settle_item,
};
pub use sched::{
    Control, HandOff, SchedConfig, SchedError, Scheduler, Stop, TickReport, Verdict, Verifier,
    WorkerHost, pick_in_order,
};
