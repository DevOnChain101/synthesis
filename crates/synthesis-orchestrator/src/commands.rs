//! The owner's side of the commands a surface queued (ADR-016 item 11, ADR-019 section 5).
//!
//! A surface only inserts a command row; the one writer of the outcome applies it here, exactly
//! once. Before the commands, the decision driver settles expiry and invalidation, so a resolve
//! that races either one finds the decision already `expired` or `invalidated` and is `stale`:
//! an expired decision is never approved. Kinds this build does not apply (`outcome.start`,
//! `pause`, `resume`, `cancel`, `ack`) stay `queued`; the orchestrator process (plan PR-12)
//! applies them.

use serde_json::{Value, json};
use synthesis_core::outcome::DecisionState;
use synthesis_session::Error as StoreError;
use synthesis_session::outcome::{CommandRecord, CommandState, OutcomeWriter, Resolution};
use time::OffsetDateTime;

use crate::sched::{Decisions, SchedError};
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
        Ok((
            CommandState::Applied,
            json!({"decision_id": r.decision_id, "state": to, "revision": r.decision_revision + 1}),
        ))
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
