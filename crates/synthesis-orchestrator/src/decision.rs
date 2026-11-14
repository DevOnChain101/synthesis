//! A blocked attempt becomes a durable Decision (ADR-019 sections 2 and 4), never a live ask.
//!
//! The worker has no one to ask: a tool call that needs a permission the contract does not give,
//! or a question to the person, ends the task `blocked` (exit 4) with the reason and the rules it
//! needed in its `task.ended` event. When the runner settles such an attempt it reads that event
//! from the attempt's session and stores a Decision any surface can list and answer, also after a
//! restart. The text in the brief comes from the model and tool output: it is data, truncated and
//! stripped of control characters, and it never carries a pre-selected choice.

use sha2::{Digest, Sha256};
use synthesis_events::{Event, Record};
use synthesis_session::outcome::{
    AttemptRecord, DecisionRecord, NewDecision, OutcomeWriter, WorkItemRecord,
};
use synthesis_session::{Error as StoreError, Store};
use time::OffsetDateTime;

use crate::compile_envelope;

const MAX_TEXT: usize = 300;
const MAX_RULES: usize = 10;

fn clean(s: &str) -> String {
    let t: String = s.chars().filter(|c| !c.is_control()).collect();
    if t.chars().count() > MAX_TEXT {
        format!("{}...", t.chars().take(MAX_TEXT).collect::<String>())
    } else {
        t
    }
}

fn sha(s: &str) -> String {
    Sha256::digest(s.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn ts(t: OffsetDateTime) -> String {
    let fmt = time::macros::format_description!(
        "[year]-[month]-[day]T[hour]:[minute]:[second].[subsecond digits:3]Z"
    );
    t.format(&fmt).expect("format utc timestamp")
}

/// What the blocked worker said, from its session's events.
struct Blocked {
    reason: String,
    needed: Vec<String>,
    question: Option<(String, Vec<String>)>,
    /// The tool calls that were refused (`tool name: input`), at most [`MAX_RULES`].
    denied: Vec<String>,
}

fn blocked_of(store: &Store, session_id: &str) -> Result<Option<Blocked>, StoreError> {
    let records = store.reader()?.read_events(session_id, 0, 20_000)?;
    let (mut ended, mut question) = (None, None);
    let mut requested = std::collections::BTreeMap::new();
    let mut denied = vec![];
    for r in records {
        if let Record::Known(e) = r {
            match e.event {
                Event::TaskEnded {
                    reason,
                    needed_permissions,
                    ..
                } => ended = Some((reason, needed_permissions)),
                Event::ToolRequested {
                    tool_call_id,
                    name,
                    input,
                    ..
                } => {
                    requested.insert(tool_call_id, (name, input));
                }
                Event::ToolCompleted {
                    tool_call_id,
                    not_run: true,
                    ..
                } => {
                    if let Some((name, input)) = requested.get(&tool_call_id)
                        && denied.len() < MAX_RULES
                    {
                        denied.push(clean(&format!("{name}: {input}")));
                    }
                }
                Event::ToolApprovalRequired {
                    dialog,
                    prompt,
                    options,
                    ..
                } if dialog == "question" => question = Some((prompt, options)),
                _ => {}
            }
        }
    }
    Ok(ended.map(|(reason, needed)| Blocked {
        reason,
        needed,
        question,
        denied,
    }))
}

/// Stores (or finds) the Decision for a `blocked` attempt and makes it `pending`. `Ok(None)` when
/// the session says nothing about why it blocked.
pub fn record_blocked_decision(
    store: &Store,
    w: &mut OutcomeWriter,
    item: &WorkItemRecord,
    attempt: &AttemptRecord,
    subject_commit: Option<&str>,
    now: OffsetDateTime,
) -> Result<Option<DecisionRecord>, StoreError> {
    let Some(b) = blocked_of(store, &attempt.session_id)? else {
        return Ok(None);
    };
    let outcome = w.get_outcome(w.id())?;
    let (contract, ..) = w.get_contract(w.id(), outcome.contract_version)?;
    let policy_hash = compile_envelope(&contract.envelope, &[])
        .map_or_else(|e| format!("uncompiled:{e}"), |c| c.policy_hash());
    let rules: Vec<String> = b.needed.iter().take(MAX_RULES).map(|r| clean(r)).collect();
    let (kind, why, delta, alternatives, action) = match (&b.question, b.reason.as_str()) {
        (Some((prompt, options)), _) if b.reason == "needs_user_input" => (
            "ambiguity",
            "the worker needs an answer to continue".to_owned(),
            clean(prompt),
            if options.is_empty() {
                vec![serde_json::json!({"id": "answer", "label": "answer the question"})]
            } else {
                options
                    .iter()
                    .take(8)
                    .map(|o| serde_json::json!({"id": clean(o), "label": clean(o)}))
                    .collect()
            },
            prompt.clone(),
        ),
        _ if b.reason == "approval_unavailable" || !rules.is_empty() => (
            "approval_needed",
            "the worker needed a permission the contract does not give".to_owned(),
            format!(
                "not allowed by the contract: {} (refused calls: {})",
                rules.join(", "),
                b.denied.join("; ")
            ),
            vec![serde_json::json!({
                "id": "grant_once",
                "label": format!("allow exactly {} in one new attempt", rules.join(", "))
            })],
            format!("{}\n{}", rules.join("\n"), b.denied.join("\n")),
        ),
        _ => (
            "blocked",
            format!("the worker ended blocked: {}", clean(&b.reason)),
            String::new(),
            vec![serde_json::json!({"id": "retry", "label": "run the work item again"})],
            b.reason.clone(),
        ),
    };
    let brief = serde_json::json!({
        "why": why,
        "delta_from_contract": delta,
        "alternatives": alternatives,
        "evidence": {"work_item": item.key, "attempt_ordinal": attempt.ordinal,
                     "attempt_session": attempt.session_id, "reason": clean(&b.reason),
                     "refused_calls": b.denied},
        "unknowns": [],
        "safe_default": "HOLD",
    });
    let binding = serde_json::json!({
        "contract_version": outcome.contract_version,
        "plan_version": item.plan_version,
        "subject_commit": subject_commit,
        "policy_hash": policy_hash,
        "action_hash": sha(&action),
        "attempt_ordinal": attempt.ordinal,
    });
    let hours = i64::from(w.decision_expiry_hours().min(24));
    let mut key = format!("{}:{}:{kind}", item.key, attempt.ordinal);
    key.truncate(128);
    let (d, fresh) = w.new_decision(&NewDecision {
        kind: kind.into(),
        dedupe_key: key,
        brief,
        binding: binding.clone(),
        expires_at: ts(now + time::Duration::hours(hours)),
    })?;
    if !fresh {
        return Ok(Some(d));
    }
    w.transition_decision(
        &d.id,
        d.revision,
        synthesis_core::outcome::DecisionState::Pending,
        &binding,
        &synthesis_session::outcome::Effects::default(),
    )?;
    Ok(w.list_decisions(w.id())?.into_iter().find(|x| x.id == d.id))
}
