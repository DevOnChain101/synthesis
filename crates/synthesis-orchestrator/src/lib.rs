//! Outcome orchestrator (ADR-017). PR-04 content: allocating an attempt's execution environment.
//!
//! One attempt, one worktree on branch `synthesis/<session_id>`, one session, one explicit cwd
//! built here from the outcome's recorded repository and base commit (never from a surface, never
//! from the process cwd). The allocation is an outbox effect (`worktree.create`) with the
//! probe-before-rerun rule, so a crash at any point is reconciled without a second worktree.
//!
//! Everything is off unless `synt.orchestrator.enabled` and the store was migrated to v5; this
//! crate holds no switch of its own, the store's `OutcomePolicy` gate does.

/// Why `orchestrator.enable` and `synt outcome enable` refuse in a build whose gate is closed.
pub const GATE_REFUSAL: &str = "the release gate is not passed: outcomes cannot be turned on in this build. Use a preview build (cargo build --release --features orchestrator-preview) or a debug build";

/// The build-time enable gate (spec orchestrator-remaining-backend section 4): open in debug builds
/// and in release builds with `orchestrator-preview`, closed in an official release until B8 flips it.
/// It blocks only NEW enabling; an already enabled home keeps running.
pub const fn enable_gate_open() -> bool {
    cfg!(any(debug_assertions, feature = "orchestrator-preview"))
}

mod alloc;
mod commands;
mod decision;
mod envelope;
pub mod gateway;
mod gitsafe;
mod grants;
mod host;
mod model_plan;
mod planner;
mod planrun;
mod proc;
mod profile;
mod recover;
mod retention;
mod runner;
mod sched;
mod verify;

pub use alloc::{AllocError, AllocSpec, Allocator, AttemptEnv, Platform};
pub use commands::{
    apply_lifecycle, apply_queued, apply_queued_with, awaiting_replan, conclude,
    replan_after_revise, restore_pause,
};
pub use decision::record_blocked_decision;
pub use envelope::{CompileError, CompiledEnvelope, Network, compile_envelope};
pub use gateway::{Action, Principal};
pub use gitsafe::{
    ChangeReport, DIFF_MAX_FILE_PATCH, DIFF_MAX_FILES, DIFF_MAX_PATCH, DiffFile, DiffReport, Entry,
    Scratch, change_flags, changed_files, commit_diff, file_edits, fresh_checkout, inspect_change,
    temp_scratch,
};
pub use grants::{
    DecisionDriver, GRANT_TTL_HOURS, answered_plans, answered_waiting, awaiting_rebrief,
    carried_commit, current_binding_for, decided_choices, decision_binding, plan_binding_for,
};
pub use host::{
    PLAN_TEXT_BEGIN, PLAN_TEXT_END, PLAN_TEXT_LABEL, ProcessHost, plan_text_block, start_commit,
};
pub use model_plan::{
    DEFAULT_MAX_ITEMS, MAX_PLAN_DEPTH, ModelPlanLimits, ReasonCode, ValidatedPlan,
    install_approved_plan, plan_hash, record_plan_approval, record_stub_fallback,
    validate_model_plan, validate_replan,
};
pub use planner::{ModelPlanner, Planner, PlannerError, Replan, StubPlanner, make_plan, replan};
pub use planrun::{
    PLAN_MAX_TURNS, PLAN_WALL_SECS, PlanCfg, PlanRun, PlanSpawn, PlanWait, PlannerProfile,
    needs_model_plan, plan_wait, plan_wait_of, record_planner_failed, recover_plan_runs,
    replan_open, replan_trigger, start_plan_run,
};
pub use proc::{ChildState, StateFn, child_state, kill_verified};
pub use profile::{DEFAULT_WALL_SECS, Profile, ProfileError, wall_secs};
pub use recover::{RecoveryReport, recover_attempts, recover_item};
pub use retention::{Cleanup, cleanup_attempt, finish_attempt, retention_due};
pub use runner::{
    CancelFlag, RunError, Runner, Running, Settled, Spawn, SyntSpawn, WorkerEnd, map_exit,
    settle_item,
};
pub use sched::{
    Control, Decisions, HandOff, SchedConfig, SchedError, Scheduler, Stop, TickReport, Verdict,
    Verifier, WorkerHost, pick_in_order,
};
pub use verify::{
    CheckRun, CheckRunner, IndependentVerifier, SCOPE_CHECK, SandboxRunner, VerifyError,
};

#[cfg(test)]
mod gate_tests {
    /// Run in release: `cargo test --release -p synthesis-orchestrator --lib closed_without_preview`.
    #[cfg(not(any(debug_assertions, feature = "orchestrator-preview")))]
    #[test]
    fn closed_without_preview() {
        assert!(!super::enable_gate_open());
    }

    #[cfg(any(debug_assertions, feature = "orchestrator-preview"))]
    #[test]
    fn open_in_debug_and_preview() {
        assert!(super::enable_gate_open());
    }
}
