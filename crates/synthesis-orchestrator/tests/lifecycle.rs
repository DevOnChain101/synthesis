//! The lifecycle commands a surface queues (`outcome.start|pause|resume|cancel|ack`) and the
//! owner applies (plan PR-12): each is applied once, by compare-and-swap, and what cannot be
//! finished yet stays queued.

#[macro_use]
mod common;
use common::*;

use std::process::Command;

use serde_json::json;
use synthesis_core::WorkspaceContext;
use synthesis_core::outcome::*;
use synthesis_orchestrator::*;
use synthesis_session::outcome::*;

fn queue(w: &OutcomeWriter, r: &Rig, key: &str, kind: &str, rev: Option<i64>) {
    r.store
        .enqueue_command(NewCommand {
            outcome_id: w.id().clone(),
            idempotency_key: key.into(),
            kind: kind.into(),
            params: json!({}),
            expected_revision: rev,
            from_surface: Surface::Cli,
        })
        .unwrap();
}

fn state_of(w: &OutcomeWriter, key: &str) -> CommandState {
    w.get_command(w.id(), key).unwrap().unwrap().state
}

/// A created outcome whose contract waits for approval (no plan yet).
fn pending(r: &Rig) -> OutcomeWriter {
    let ws = WorkspaceContext::open("w", &r.repo, &r.repo, &[], r.base.join("cart"), "pg").unwrap();
    let mut w = r
        .store
        .create_outcome(NewOutcome {
            workspace: &ws,
            base_commit: r.head.clone(),
            contract: &contract(),
            policy: &policy(),
        })
        .unwrap();
    let rev = w.get_outcome(w.id()).unwrap().revision;
    w.transition_outcome(
        rev,
        OutcomeState::ContractPending,
        None,
        &Effects::default(),
    )
    .unwrap();
    w
}

fn approve(w: &mut OutcomeWriter) {
    let h = w.get_contract(w.id(), 1).unwrap().1;
    w.approve_contract(1, &h, "user").unwrap();
}

#[test]
fn start_plans_and_runs_only_an_approved_contract() {
    need_git!();
    let r = rig();
    let mut w = pending(&r);
    let c = Control::default();
    queue(&w, &r, "s1", "outcome.start", None);
    apply_lifecycle(&mut w, &c, &StubPlanner).unwrap();
    assert_eq!(state_of(&w, "s1"), CommandState::Denied);
    assert_eq!(
        w.get_outcome(w.id()).unwrap().state,
        OutcomeState::ContractPending
    );
    approve(&mut w);
    queue(&w, &r, "s2", "outcome.start", None);
    apply_lifecycle(&mut w, &c, &StubPlanner).unwrap();
    assert_eq!(state_of(&w, "s2"), CommandState::Applied);
    let o = w.get_outcome(w.id()).unwrap();
    assert_eq!((o.state, o.plan_version), (OutcomeState::Running, Some(1)));
    // a second start is a no-op, not a second plan
    queue(&w, &r, "s3", "outcome.start", None);
    apply_lifecycle(&mut w, &c, &StubPlanner).unwrap();
    assert_eq!(state_of(&w, "s3"), CommandState::Applied);
    assert_eq!(w.get_outcome(w.id()).unwrap().plan_version, Some(1));
}

#[test]
fn a_command_for_an_older_revision_is_stale_and_changes_nothing() {
    need_git!();
    let r = rig();
    let mut w = pending(&r);
    approve(&mut w);
    let rev = w.get_outcome(w.id()).unwrap().revision;
    queue(&w, &r, "old", "outcome.start", Some(rev - 1));
    apply_lifecycle(&mut w, &Control::default(), &StubPlanner).unwrap();
    assert_eq!(state_of(&w, "old"), CommandState::Stale);
    assert_eq!(
        w.get_outcome(w.id()).unwrap().state,
        OutcomeState::ContractPending
    );
}

#[test]
fn a_pause_is_durable_and_a_new_owner_keeps_it() {
    need_git!();
    let r = rig();
    let (mut w, _) = r.fresh();
    let c = Control::default();
    queue(&w, &r, "p", "outcome.pause", None);
    apply_lifecycle(&mut w, &c, &StubPlanner).unwrap();
    assert!(c.is_paused());
    assert_eq!(state_of(&w, "p"), CommandState::Applied);
    // another owner, later: it reads the pause from the control session's events
    let later = Control::default();
    restore_pause(&w, &later).unwrap();
    assert!(later.is_paused());
    queue(&w, &r, "r", "outcome.resume", None);
    apply_lifecycle(&mut w, &c, &StubPlanner).unwrap();
    let after = Control::default();
    restore_pause(&w, &after).unwrap();
    assert!(!c.is_paused() && !after.is_paused());
}

#[test]
fn cancelling_a_running_outcome_waits_for_the_scheduler() {
    need_git!();
    let r = rig();
    let (mut w, _) = r.fresh();
    let c = Control::default();
    queue(&w, &r, "x", "outcome.cancel", None);
    let done = apply_lifecycle(&mut w, &c, &StubPlanner).unwrap();
    // the flag is set; the command is NOT finished (a crash now must not lose the cancel)
    assert!(c.cancel.is_set() && done.is_empty());
    assert_eq!(state_of(&w, "x"), CommandState::Queued);
    // the scheduler ends the outcome; the next pass finishes the command
    let rev = w.get_outcome(w.id()).unwrap().revision;
    w.transition_outcome(rev, OutcomeState::Cancelled, None, &Effects::default())
        .unwrap();
    apply_lifecycle(&mut w, &c, &StubPlanner).unwrap();
    assert_eq!(state_of(&w, "x"), CommandState::Applied);
}

#[test]
fn cancel_before_it_runs_and_ack_only_when_verified() {
    need_git!();
    let r = rig();
    let mut w = pending(&r);
    let c = Control::default();
    queue(&w, &r, "a", "outcome.ack", None);
    queue(&w, &r, "c", "outcome.cancel", None);
    apply_lifecycle(&mut w, &c, &StubPlanner).unwrap();
    assert_eq!(state_of(&w, "a"), CommandState::Denied);
    assert_eq!(state_of(&w, "c"), CommandState::Applied);
    assert_eq!(
        w.get_outcome(w.id()).unwrap().state,
        OutcomeState::Cancelled
    );
}

#[test]
fn an_outcome_without_independent_evidence_is_not_verified() {
    need_git!();
    let r = rig();
    let (mut w, _) = r.fresh();
    // no evidence was recorded: the store's own verdict refuses `verified`
    assert_eq!(conclude(&mut w).unwrap(), OutcomeState::ReworkRequired);
    // and doing it again changes nothing
    assert_eq!(conclude(&mut w).unwrap(), OutcomeState::ReworkRequired);
}
