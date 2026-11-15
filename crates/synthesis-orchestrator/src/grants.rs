//! The decision lifecycle after a person answers (ADR-019 sections 4 and 5), and one-shot grants.
//!
//! [`DecisionDriver`] runs on every scheduler tick (through the [`crate::Decisions`] seam):
//! - a `pending` decision past its expiry becomes `expired` (HOLD: nothing is approved, the work
//!   item stays `waiting_decision`);
//! - a `pending` decision whose binding no longer matches the outcome (a new contract version or
//!   plan, another commit, another policy hash) becomes `invalidated`, and an unused grant behind
//!   such a decision is revoked;
//! - a decision a person approved with `grant_once` becomes a one-shot grant, derived by the
//!   store from the action stored with the decision (never from text a caller supplies), after the
//!   contract is checked again; the work item is then `ready` for a new attempt;
//! - an approved alternative of an `ambiguity` or `blocked` decision makes the item `ready`.
//!
//! A new attempt of that item uses the grant when it starts ([`crate::ProcessHost`]): the grant is
//! consumed (once), its rules become `--allow` flags of that attempt only, and every step is a
//! chained `policy.decision` event.

use serde_json::Value;
use synthesis_core::outcome::{DecisionState, WorkItemId, WorkItemState};
use synthesis_session::Error as StoreError;
use synthesis_session::outcome::{DecisionRecord, Effects, OutcomeWriter, WorkItemRecord};
use time::OffsetDateTime;

use crate::compile_envelope;
use crate::decision::{clean, rule_denied};
use crate::sched::{Decisions, SchedError};

/// How long an unused grant lives (the settings' `decision_expiry_hours` caps it).
pub const GRANT_TTL_HOURS: u32 = 24;

/// The binding of a decision, built in one place so a decision and its later checks agree on
/// every byte: contract version, plan version, subject commit, policy hash, action hash, the
/// attempt ordinal and the work item.
pub fn decision_binding(
    contract_version: u32,
    plan_version: u32,
    subject_commit: Option<&str>,
    policy_hash: &str,
    action_hash: &str,
    attempt_ordinal: u32,
    item: &WorkItemId,
) -> Value {
    serde_json::json!({
        "contract_version": contract_version,
        "plan_version": plan_version,
        "subject_commit": subject_commit,
        "policy_hash": policy_hash,
        "action_hash": action_hash,
        "attempt_ordinal": attempt_ordinal,
        "work_item_id": item,
    })
}

/// The binding the decision would get NOW: the outcome's current contract and plan, the policy hash
/// of the current contract, the item's newest finished attempt (before `before_ordinal` when an
/// attempt is starting: it is not the one the decision is about). The action hash is the stored one
/// (the action is immutable).
pub fn current_binding_for(
    w: &OutcomeWriter,
    d: &DecisionRecord,
    before_ordinal: Option<u32>,
) -> Result<Option<(WorkItemId, Value)>, StoreError> {
    let Some(item) = d
        .binding
        .get("work_item_id")
        .and_then(Value::as_str)
        .and_then(|s| WorkItemId::parse(s).ok())
    else {
        return Ok(None);
    };
    let outcome = w.get_outcome(w.id())?;
    let (contract, ..) = w.get_contract(w.id(), outcome.contract_version)?;
    let policy = compile_envelope(&contract.envelope, &[])
        .map_or_else(|_| "uncompiled".to_owned(), |c| c.policy_hash());
    let newest = w
        .list_attempts(&item)?
        .into_iter()
        .filter(|a| a.subject_commit.is_some() || a.ended_at.is_some())
        .filter(|a| before_ordinal.is_none_or(|o| a.ordinal < o))
        .max_by_key(|a| a.ordinal);
    let action_hash = d
        .binding
        .get("action_hash")
        .and_then(Value::as_str)
        .unwrap_or_default();
    Ok(Some((
        item.clone(),
        decision_binding(
            outcome.contract_version,
            outcome.plan_version.unwrap_or(0),
            newest.as_ref().and_then(|a| a.subject_commit.as_deref()),
            &policy,
            action_hash,
            newest.as_ref().map_or(0, |a| a.ordinal),
            &item,
        ),
    )))
}

/// The choices a person made for `item`'s earlier decisions (ambiguities), as quoted DATA for the
/// prompt of a new attempt: what was asked and which alternative was approved.
pub fn decided_choices(
    w: &OutcomeWriter,
    item: &WorkItemId,
    before_ordinal: u32,
) -> Result<Vec<String>, StoreError> {
    let mut out = vec![];
    for d in w.list_decisions(w.id())? {
        let for_item = d.binding.get("work_item_id").and_then(Value::as_str) == Some(item.as_str());
        let earlier = d
            .binding
            .get("attempt_ordinal")
            .and_then(Value::as_u64)
            .is_some_and(|o| o < u64::from(before_ordinal));
        if !for_item || !earlier || d.kind != "ambiguity" {
            continue;
        }
        let Some(r) = w.verified_resolution(&d.id)? else {
            continue;
        };
        if r.state != DecisionState::Approved {
            continue;
        }
        let label = d
            .brief
            .get("alternatives")
            .and_then(Value::as_array)
            .and_then(|a| {
                a.iter()
                    .find(|x| x.get("id").and_then(Value::as_str) == Some(r.choice.as_str()))
            })
            .and_then(|x| x.get("label").and_then(Value::as_str))
            .unwrap_or(&r.choice);
        let question = d
            .brief
            .get("delta_from_contract")
            .and_then(Value::as_str)
            .unwrap_or("");
        out.push(format!("{}: {}", clean(question), clean(label)));
    }
    Ok(out)
}

/// Runs the decision lifecycle on every tick; see the module comment.
/// The refusals already audited in this process (a decision that stays refused is reported once,
/// not on every tick).
#[derive(Default)]
pub struct DecisionDriver {
    refused: std::collections::BTreeSet<String>,
}

fn go(w: &mut OutcomeWriter, id: &WorkItemId, to: WorkItemState) -> Result<(), StoreError> {
    let rev = w.get_work_item(id)?.revision;
    w.transition_work_item(id, rev, to, &Effects::default())?;
    Ok(())
}

impl DecisionDriver {
    fn item(w: &OutcomeWriter, id: &WorkItemId) -> Result<WorkItemRecord, StoreError> {
        w.get_work_item(id)
    }
}

impl Decisions for DecisionDriver {
    fn drive(
        &mut self,
        w: &mut OutcomeWriter,
        _now: OffsetDateTime,
    ) -> Result<Vec<String>, SchedError> {
        let mut notes = vec![];
        let none = Effects::default();
        for d in w.list_decisions(w.id())? {
            let Some((item_id, current)) = current_binding_for(w, &d, None)? else {
                continue;
            };
            let state = w.decision_state(&d.id)?;
            match state {
                DecisionState::Preparing | DecisionState::Pending => {
                    // expiry first: HOLD, never an approval
                    if w.transition_decision(
                        &d.id,
                        d.revision,
                        DecisionState::Expired,
                        &current,
                        &none,
                    )
                    .is_ok()
                    {
                        notes.push(format!(
                            "decision {} expired: the work item stays on hold",
                            d.id
                        ));
                        w.audit_policy(
                            "decision.expire",
                            "hold",
                            serde_json::json!({"decision_id": d.id, "work_item_id": item_id}),
                        )?;
                    } else if w
                        .transition_decision(
                            &d.id,
                            d.revision,
                            DecisionState::Invalidated,
                            &current,
                            &none,
                        )
                        .is_ok()
                    {
                        notes.push(format!(
                            "decision {} invalidated: the contract, plan, commit or policy changed",
                            d.id
                        ));
                        w.audit_policy(
                            "decision.invalidate",
                            "deny",
                            serde_json::json!({"decision_id": d.id, "work_item_id": item_id}),
                        )?;
                    }
                }
                DecisionState::Approved => {
                    let grants: Vec<_> = w
                        .list_grants(w.id())?
                        .into_iter()
                        .filter(|g| g.decision_id == d.id)
                        .collect();
                    if let Some(g) = grants.first() {
                        // an unused grant behind a decision whose binding moved is withdrawn
                        let stale = !binding_matches(w, &d, &current)?;
                        if stale && g.consumed_at.is_none() && g.revoked_at.is_none() {
                            w.revoke_grant(&g.id, "the binding changed before the grant was used")?;
                            notes.push(format!("grant {} revoked: the binding changed", g.id));
                        }
                        continue;
                    }
                    let item = Self::item(w, &item_id)?;
                    if item.state != WorkItemState::WaitingDecision {
                        continue;
                    }
                    let Some(res) = w.verified_resolution(&d.id)? else {
                        continue;
                    };
                    if d.kind == "approval_needed" && res.choice == "grant_once" {
                        // the contract is checked AGAIN: a grant never exceeds what it denies now
                        let outcome = w.get_outcome(w.id())?;
                        let (contract, ..) = w.get_contract(w.id(), outcome.contract_version)?;
                        let rules: Vec<String> = w
                            .decision_action(&d.id)?
                            .and_then(|a| a.get("grant_rules").cloned())
                            .and_then(|r| serde_json::from_value(r).ok())
                            .unwrap_or_default();
                        let denied = match compile_envelope(&contract.envelope, &[]) {
                            Ok(c) => rules.iter().any(|r| rule_denied(&c.deny, r)),
                            Err(_) => true,
                        };
                        if denied || rules.is_empty() {
                            if !self.refused.insert(d.id.to_string()) {
                                continue;
                            }
                            w.audit_policy(
                                "grant.create",
                                "deny",
                                serde_json::json!({"decision_id": d.id, "reason": "the contract denies it now"}),
                            )?;
                            notes.push(format!(
                                "decision {}: no grant, the contract denies it",
                                d.id
                            ));
                            continue;
                        }
                        match w.create_grant(&d.id, &current, GRANT_TTL_HOURS) {
                            Ok(g) => {
                                go(w, &item_id, WorkItemState::Ready)?;
                                notes.push(format!("grant {} created for {}", g.id, item.key));
                            }
                            Err(e) => {
                                if !self.refused.insert(d.id.to_string()) {
                                    continue;
                                }
                                w.audit_policy(
                                    "grant.create",
                                    "deny",
                                    serde_json::json!({"decision_id": d.id, "reason": e.to_string()}),
                                )?;
                                notes.push(format!("decision {}: no grant ({e})", d.id));
                            }
                        }
                    } else if d.kind != "approval_needed" && binding_matches(w, &d, &current)? {
                        // a chosen alternative (or a retry): the item runs again, the choice goes
                        // into the prompt of that attempt as data
                        go(w, &item_id, WorkItemState::Ready)?;
                        notes.push(format!(
                            "decision {} answered: {} runs again",
                            d.id, item.key
                        ));
                    } else if d.kind == "approval_needed" && res.choice != "grant_once" {
                        // `retry` on an approval: run again without a grant
                        if binding_matches(w, &d, &current)? {
                            go(w, &item_id, WorkItemState::Ready)?;
                            notes.push(format!("decision {}: {} runs again", d.id, item.key));
                        }
                    }
                }
                _ => {}
            }
        }
        Ok(notes)
    }
}

/// Does the stored binding hash equal the hash of `current` (as the store compares it)?
fn binding_matches(
    w: &mut OutcomeWriter,
    d: &DecisionRecord,
    current: &Value,
) -> Result<bool, StoreError> {
    w.binding_is_current(d, current)
}
