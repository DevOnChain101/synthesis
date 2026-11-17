//! The owner's side of the commands a surface queued (ADR-016 item 11, ADR-019 section 5).
//!
//! A surface only inserts a command row; the one writer of the outcome applies it here, exactly
//! once. Before the commands, the decision driver settles expiry and invalidation, so a resolve
//! that races either one finds the decision already `expired` or `invalidated` and is `stale`:
//! an expired decision is never approved. [`apply_lifecycle`] applies `outcome.start|pause|resume|
//! cancel|ack` the same way; a kind nobody here knows stays `queued`.
//!
//! `outcome.contract.revise` (an `edit_scope` answer, B4) stores a revised contract as the next
//! version, unapproved. Approving it (`outcome.contract.approve`, bound to its hash) is what moves
//! the outcome: the store rebinds and the decision driver, run right after, invalidates every
//! decision and revokes every unused grant of the old version; [`replan_after_revise`] then stores
//! the new plan and the outcome runs again.

use serde_json::{Value, json};
use synthesis_core::outcome::{Contract, DecisionState, OutcomeState};
use synthesis_events::Record;
use synthesis_session::Error as StoreError;
use synthesis_session::outcome::{CommandRecord, CommandState, Effects, OutcomeWriter, Resolution};
use time::OffsetDateTime;

use crate::planner::{Planner, make_plan, replan};
use crate::sched::{Control, Decisions, SchedError};
use crate::{DecisionDriver, current_binding_for};

/// Applies every queued command this build knows; returns the ones finished now, oldest first.
pub fn apply_queued(
    w: &mut OutcomeWriter,
    now: OffsetDateTime,
) -> Result<Vec<CommandRecord>, SchedError> {
    DecisionDriver::default().drive(w, now)?;
    let mut done = vec![];
    for c in w.queued_commands(w.id())? {
        let run = match c.kind.as_str() {
            "decision.resolve" => resolve(w, &c),
            "outcome.contract.approve" => approve(w, &c),
            "outcome.contract.revise" => revise(w, &c),
            _ => continue,
        };
        done.push(match run {
            Ok(rec) => rec,
            // the database or the disk failed: stop, the commands stay queued
            Err(e @ (StoreError::Sqlite(_) | StoreError::Io(_) | StoreError::Locked(_))) => {
                return Err(e.into());
            }
            // anything else is about this command: refuse it with a generic reason (no row text
            // or path reaches a surface) and go on with the others
            Err(_) => deny(w, &c, "the command could not be applied")?,
        });
    }
    // a contract that was just rebound: decisions and grants of the old version die now, not on a later tick
    if done
        .iter()
        .any(|c| c.kind == "outcome.contract.approve" && c.state == CommandState::Applied)
    {
        DecisionDriver::default().drive(w, now)?;
    }
    Ok(done)
}

/// A refusal that changed nothing: any validation error of the store, mapped to `denied`.
fn refused(e: &StoreError) -> bool {
    matches!(e, StoreError::Invalid(_) | StoreError::InvalidTransition(_))
}

fn finish(
    w: &mut OutcomeWriter,
    c: &CommandRecord,
    state: CommandState,
    body: Value,
) -> Result<CommandRecord, StoreError> {
    w.apply_command(&c.idempotency_key, |_, _| Ok((state, body.clone())))
}

fn deny(
    w: &mut OutcomeWriter,
    c: &CommandRecord,
    why: impl ToString,
) -> Result<CommandRecord, StoreError> {
    finish(
        w,
        c,
        CommandState::Denied,
        json!({"reason": why.to_string()}),
    )
}

fn resolve(w: &mut OutcomeWriter, c: &CommandRecord) -> Result<CommandRecord, StoreError> {
    let r = match Resolution::from_command(c) {
        Ok(r) => r,
        Err(e) => return deny(w, c, e),
    };
    let Some(d) = w
        .list_decisions(w.id())?
        .into_iter()
        .find(|d| d.id == r.decision_id)
    else {
        return deny(w, c, "no such decision in this outcome");
    };
    // fail closed: a row whose state cannot be verified is not resolved by anyone
    let state = match w.decision_state(&d.id) {
        Ok(s) => s,
        Err(e) => return deny(w, c, e),
    };
    let body = json!({"decision_id": d.id, "state": state, "revision": d.revision});
    match state {
        DecisionState::Pending => {}
        DecisionState::Approved | DecisionState::Rejected | DecisionState::NeedsMoreInfo => {
            return finish(w, c, CommandState::AlreadyResolved, body);
        }
        DecisionState::Expired | DecisionState::Invalidated => {
            return finish(w, c, CommandState::Stale, body);
        }
        DecisionState::Preparing => return deny(w, c, "the decision is not open yet"),
    }
    let Some((_, current)) = current_binding_for(w, &d, None)? else {
        return deny(w, c, "the decision has no work item binding");
    };
    let run = w.apply_command(&c.idempotency_key, |tx, _| {
        let to = tx.resolve_decision(&r, &current)?;
        let mut body =
            json!({"decision_id": r.decision_id, "state": to, "revision": r.decision_revision + 1});
        let note = match r.choice.as_str() {
            "request_evidence" => Some(
                "evidence requested: this decision is closed and a new one with the stored evidence replaces it; answer the new one (list the decisions)",
            ),
            "edit_scope" => Some(
                "scope edit requested: nothing changes yet and the outcome stays blocked. Submit a revised contract (synt outcome revise OUTCOME --contract-file FILE), read the diff, and approve that exact version and hash; approving it closes this decision, revokes its unused grants and replans. Until then the approved contract stays in force",
            ),
            _ => None,
        };
        if let Some(n) = note {
            body["note"] = json!(n);
        }
        Ok((CommandState::Applied, body))
    });
    match run {
        Err(e) if refused(&e) => deny(w, c, e),
        other => other,
    }
}

fn approve(w: &mut OutcomeWriter, c: &CommandRecord) -> Result<CommandRecord, StoreError> {
    let version = c
        .params
        .get("version")
        .and_then(Value::as_u64)
        .and_then(|v| u32::try_from(v).ok());
    let hash = c.params.get("hash").and_then(Value::as_str);
    let (Some(version), Some(hash)) = (version, hash) else {
        return deny(w, c, "approving a contract needs a version and its hash");
    };
    let surface = json!(c.from_surface);
    let approver = format!("user:{}", surface.as_str().unwrap_or("unknown"));
    let run = w.apply_command(&c.idempotency_key, |tx, _| {
        tx.approve_contract(version, hash, &approver)?;
        Ok((CommandState::Applied, json!({"version": version})))
    });
    match run {
        Err(e) if refused(&e) => deny(w, c, e),
        other => other,
    }
}

/// `outcome.contract.revise`: the revised contract (same shape as create's) becomes the next
/// version, unapproved. The version number is the store's; the outcome revision the person saw is
/// required, and a lost race is `stale`.
fn revise(w: &mut OutcomeWriter, c: &CommandRecord) -> Result<CommandRecord, StoreError> {
    let Some(contract) = c
        .params
        .get("contract")
        .and_then(|v| serde_json::from_value::<Contract>(v.clone()).ok())
    else {
        return deny(
            w,
            c,
            "revising needs a contract of the same shape as create's",
        );
    };
    let Some(rev) = c.expected_revision else {
        return deny(w, c, "revising needs the outcome revision you saw");
    };
    let run = w.apply_command(&c.idempotency_key, |tx, _| {
        let (version, hash) = tx.revise_contract(rev, contract.clone())?;
        Ok((
            CommandState::Applied,
            json!({"version": version, "hash": hash}),
        ))
    });
    match run {
        Err(e) if refused(&e) => deny(w, c, e),
        other => other,
    }
}

/// What one lifecycle command came to.
enum Step {
    Done(Value),
    Denied(String),
    /// Not finished: the command stays `queued` and is looked at again (a cancel of a running
    /// outcome ends with the scheduler's tick, so a crash in between cannot lose it).
    Waiting,
}

fn name(s: OutcomeState) -> String {
    serde_json::to_value(s)
        .ok()
        .and_then(|v| v.as_str().map(String::from))
        .unwrap_or_default()
}

fn go(w: &mut OutcomeWriter, to: OutcomeState) -> Result<(), StoreError> {
    let rev = w.get_outcome(w.id())?.revision;
    w.transition_outcome(rev, to, None, &Effects::default())?;
    Ok(())
}

/// Applies `outcome.start|pause|resume|cancel|ack` that surfaces queued. `control` is the pause
/// flag and cancel flag of the outcome's scheduler. A pause is also written as an event, so an
/// owner that starts later ([`restore_pause`]) does not run what a person paused.
pub fn apply_lifecycle(
    w: &mut OutcomeWriter,
    control: &Control,
    planner: &dyn Planner,
) -> Result<Vec<CommandRecord>, SchedError> {
    let mut done = vec![];
    for c in w.queued_commands(w.id())? {
        let kind = c.kind.as_str();
        if !matches!(
            kind,
            "outcome.start" | "outcome.pause" | "outcome.resume" | "outcome.cancel" | "outcome.ack"
        ) {
            continue;
        }
        let o = w.get_outcome(w.id())?;
        // a cancel that was waiting for the scheduler is finished by the state it caused
        let cancelled = kind == "outcome.cancel" && o.state == OutcomeState::Cancelled;
        let run = if cancelled {
            Ok(Step::Done(json!({"state": "cancelled"})))
        } else if c.expected_revision.is_some_and(|r| r != o.revision) {
            let body = json!({"expected": c.expected_revision, "actual": o.revision});
            done.push(finish(w, &c, CommandState::Stale, body)?);
            continue;
        } else {
            match kind {
                "outcome.start" => start(w, planner),
                "outcome.pause" => pause(w, control, &c),
                "outcome.resume" => resume(w, control, &c),
                "outcome.cancel" => cancel(w, control),
                _ => ack(w),
            }
        };
        match run {
            Ok(Step::Done(body)) => done.push(finish(w, &c, CommandState::Applied, body)?),
            Ok(Step::Denied(why)) => done.push(deny(w, &c, why)?),
            Ok(Step::Waiting) => {}
            Err(e @ (StoreError::Sqlite(_) | StoreError::Io(_) | StoreError::Locked(_))) => {
                return Err(e.into());
            }
            Err(_) => done.push(deny(w, &c, "the command could not be applied")?),
        }
    }
    Ok(done)
}

fn start(w: &mut OutcomeWriter, planner: &dyn Planner) -> Result<Step, StoreError> {
    let o = w.get_outcome(w.id())?;
    match o.state {
        OutcomeState::Running => return Ok(Step::Done(json!({"state": "running"}))),
        OutcomeState::ContractPending | OutcomeState::Ready => {}
        s => {
            return Ok(Step::Denied(format!(
                "the outcome is {}, it cannot be started",
                name(s)
            )));
        }
    }
    if !w.contract_is_approved(w.id(), o.contract_version)? {
        return Ok(Step::Denied("approve the contract first".into()));
    }
    // each step below is skipped when it already happened, so a crash between them is finished
    // by the next try (the command stays queued until the outcome runs)
    if o.plan_version.is_none() {
        let (contract, _, _) = w.get_contract(w.id(), o.contract_version)?;
        let (plan, _) = make_plan(planner, &contract, 1)
            .map_err(|e| StoreError::Invalid(format!("no plan: {e}")))?;
        w.insert_plan(&plan)?;
    }
    if w.get_outcome(w.id())?.state == OutcomeState::ContractPending {
        go(w, OutcomeState::Ready)?;
    }
    go(w, OutcomeState::Running)?;
    Ok(Step::Done(json!({"state": "running"})))
}

fn marker(
    w: &mut OutcomeWriter,
    c: &CommandRecord,
    kind: &'static str,
    body: Value,
) -> Result<Step, StoreError> {
    // the command and the marker event are one transaction
    w.apply_command(&c.idempotency_key, |tx, _| {
        tx.emit(kind, json!({}))?;
        Ok((CommandState::Applied, body.clone()))
    })?;
    // already finished here: nothing more for the caller to write
    Ok(Step::Waiting)
}

fn pause(w: &mut OutcomeWriter, control: &Control, c: &CommandRecord) -> Result<Step, StoreError> {
    let s = w.get_outcome(w.id())?.state;
    if s.is_terminal() {
        return Ok(Step::Denied(format!("the outcome is {}", name(s))));
    }
    control.pause();
    marker(w, c, "outcome.paused", json!({"paused": true}))
}

fn resume(w: &mut OutcomeWriter, control: &Control, c: &CommandRecord) -> Result<Step, StoreError> {
    let s = w.get_outcome(w.id())?.state;
    if s.is_terminal() {
        return Ok(Step::Denied(format!("the outcome is {}", name(s))));
    }
    control.resume();
    if s == OutcomeState::Blocked {
        go(w, OutcomeState::Running)?;
    }
    marker(w, c, "outcome.resumed", json!({"paused": false}))
}

fn cancel(w: &mut OutcomeWriter, control: &Control) -> Result<Step, StoreError> {
    match w.get_outcome(w.id())?.state {
        OutcomeState::Cancelled => Ok(Step::Done(json!({"state": "cancelled"}))),
        s if s.is_terminal() => Ok(Step::Denied(format!("the outcome is {}", name(s)))),
        OutcomeState::Running => {
            control.cancel.cancel();
            Ok(Step::Waiting)
        }
        _ => {
            go(w, OutcomeState::Cancelled)?;
            Ok(Step::Done(json!({"state": "cancelled"})))
        }
    }
}

fn ack(w: &mut OutcomeWriter) -> Result<Step, StoreError> {
    match w.get_outcome(w.id())?.state {
        OutcomeState::Completed => Ok(Step::Done(json!({"state": "completed"}))),
        OutcomeState::Verified => {
            go(w, OutcomeState::Completed)?;
            Ok(Step::Done(json!({"state": "completed"})))
        }
        s => Ok(Step::Denied(format!(
            "the outcome is {}, only a verified one is acknowledged",
            name(s)
        ))),
    }
}

/// Is a `blocked` outcome waiting for a plan made for its (revised, approved) contract? The owner
/// keeps it until [`replan_after_revise`] has stored one.
pub fn awaiting_replan(
    w: &synthesis_session::Reader,
    id: &synthesis_core::outcome::OutcomeId,
) -> Result<bool, StoreError> {
    let o = w.get_outcome(id)?;
    let Some(pv) = o.plan_version else {
        return Ok(false);
    };
    Ok(o.state == OutcomeState::Blocked
        && w.get_plan(id, pv)?.contract_version != o.contract_version)
}

/// After a revised contract was approved and rebound: stores the next plan for it (`replan`: its
/// diff names what runs again, and with another contract nothing of the old plan is carried over)
/// and puts the outcome back to `running`, so the scheduler walks the new plan. A plan from the stub
/// planner is a function of the approved contract, so it needs no approval of its own; a model plan
/// will go through a `plan_approval` decision (B6). Evidence of the old plan never counts for the new
/// contract (the store filters it). Returns whether it replanned; each step is skipped when it
/// already happened, so a crash in between is finished by the next try.
pub fn replan_after_revise(
    w: &mut OutcomeWriter,
    planner: &dyn Planner,
) -> Result<bool, StoreError> {
    if !awaiting_replan(w, w.id())? {
        return Ok(false);
    }
    let o = w.get_outcome(w.id())?;
    let (contract, _, approved) = w.get_contract(w.id(), o.contract_version)?;
    if !approved {
        return Ok(false);
    }
    let old = w.get_plan(w.id(), o.plan_version.unwrap_or(0))?;
    let r = replan(planner, &contract, &old)
        .map_err(|e| StoreError::Invalid(format!("no plan for the revised contract: {e}")))?;
    let d = &r.diff;
    w.atomically(|tx| {
        tx.insert_plan(&r.plan)?;
        tx.emit(
            "outcome.replanned",
            json!({"plan_version": r.plan.version, "contract_version": o.contract_version,
                   "added": d.added, "removed": d.removed, "changed": d.changed,
                   "unchanged": d.unchanged, "rerun": d.rerun}),
        )?;
        Ok(())
    })?;
    go(w, OutcomeState::Running)?;
    Ok(true)
}

/// Sets the pause flag when the last `outcome.paused` / `outcome.resumed` event of the control
/// session says paused. Called when an owner adopts an outcome.
pub fn restore_pause(w: &OutcomeWriter, control: &Control) -> Result<(), StoreError> {
    let (mut after, mut paused) = (0, false);
    loop {
        let page = w.read_events(w.control_session_id(), after, 500)?;
        if page.is_empty() {
            break;
        }
        for rec in page {
            match rec {
                Record::Known(e) => after = e.seq,
                Record::Unknown(e) => {
                    after = e.seq;
                    match e.kind.as_str() {
                        "outcome.paused" => paused = true,
                        "outcome.resumed" => paused = false,
                        _ => {}
                    }
                }
            }
        }
    }
    if paused {
        control.pause();
    }
    Ok(())
}

/// Every work item succeeded: `running` -> `verifying` -> `verified`, or `rework_required` when the
/// store's own verdict says the evidence does not support it (nothing here decides that).
/// Safe to call again after a crash between the steps.
pub fn conclude(w: &mut OutcomeWriter) -> Result<OutcomeState, StoreError> {
    if w.get_outcome(w.id())?.state == OutcomeState::Running {
        go(w, OutcomeState::Verifying)?;
    }
    if w.get_outcome(w.id())?.state == OutcomeState::Verifying {
        match go(w, OutcomeState::Verified) {
            Ok(()) => {}
            Err(e) if refused(&e) => go(w, OutcomeState::ReworkRequired)?,
            Err(e) => return Err(e),
        }
    }
    Ok(w.get_outcome(w.id())?.state)
}
