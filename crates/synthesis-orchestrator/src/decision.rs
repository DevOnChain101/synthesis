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

/// Control characters and the invisible or direction-changing ones (bidi, zero-width, line and
/// paragraph separators, the tag block): text from the model must not rewrite what a person reads.
fn hidden(c: char) -> bool {
    c.is_control()
        || matches!(
            c,
            '\u{00AD}'
                | '\u{200B}'..='\u{200F}'
                | '\u{2028}'..='\u{202E}'
                | '\u{2060}'..='\u{206F}'
                | '\u{FEFF}'
                | '\u{FFF9}'..='\u{FFFB}'
                | '\u{E0000}'..='\u{E007F}'
        )
}

fn clean(s: &str) -> String {
    let t: String = s.chars().filter(|c| !hidden(*c)).collect();
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
    /// Untruncated, as the worker reported them: the action hash is over these.
    needed: Vec<String>,
    question: Option<(String, Vec<String>)>,
    /// The worker asked to leave plan mode (`ExitPlanMode`).
    plan: bool,
    /// Calls that needed an approval nobody could give (`tool: input`, untruncated).
    asked: Vec<String>,
    /// Calls the contract's own rules refused (`tool: input`, untruncated).
    denied: Vec<String>,
}

fn blocked_of(store: &Store, session_id: &str) -> Result<Option<Blocked>, StoreError> {
    let records = store.reader()?.read_events(session_id, 0, 20_000)?;
    let (mut ended, mut question) = (None, None);
    let mut requested = std::collections::BTreeMap::new();
    let (mut asked, mut denied) = (vec![], vec![]);
    let mut plan = false;
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
                    preview,
                    ..
                } => {
                    if let Some((name, input)) = requested.get(&tool_call_id) {
                        let call = format!("{name}: {input}");
                        if preview.trim_start().starts_with("Permission denied") {
                            denied.push(call);
                        } else {
                            asked.push(call);
                        }
                    }
                }
                Event::ToolApprovalRequired {
                    dialog,
                    prompt,
                    options,
                    ..
                } if dialog == "question" => question = Some((prompt, options)),
                Event::ToolApprovalRequired { dialog, .. } if dialog == "plan" => plan = true,
                _ => {}
            }
        }
    }
    Ok(ended.map(|(reason, needed)| Blocked {
        reason,
        needed,
        question,
        plan,
        asked,
        denied,
    }))
}

/// Is the rule the worker needed one that must not be offered as a grant: covered by a deny rule
/// of the compiled contract (identical, or a bare deny of the tool; the edit tools count as one
/// family) or a tool that does not exist in an attempt.
fn rule_denied(deny: &[String], rule: &str) -> bool {
    let family = |t: &str| match t {
        "Write" | "MultiEdit" | "NotebookEdit" => "Edit".to_owned(),
        t => t.to_owned(),
    };
    let (tool, _) = rule.split_once('(').unwrap_or((rule, ""));
    // an attempt has no browser and no MCP tools: a grant could never make them exist
    if tool.starts_with("browser_") || tool.starts_with("mcp__") {
        return true;
    }
    deny.iter().any(|d| {
        d == rule || {
            let (dt, spec) = d.split_once('(').unwrap_or((d, ""));
            spec.is_empty() && family(dt) == family(tool)
        }
    })
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
    // the caller's record may predate the start (the policy binding is written then)
    let fresh = w
        .list_attempts(&item.id)?
        .into_iter()
        .find(|a| a.id == attempt.id)
        .unwrap_or_else(|| attempt.clone());
    let attempt = &fresh;
    let outcome = w.get_outcome(w.id())?;
    // the contract and policy the attempt STARTED under (an outcome that moved on since is stale)
    let contract_version = attempt
        .bound_contract_version
        .unwrap_or(outcome.contract_version);
    let (contract, ..) = w.get_contract(w.id(), contract_version)?;
    let compiled = compile_envelope(&contract.envelope, &[]).ok();
    let policy_hash = attempt.bound_policy_hash.clone().unwrap_or_else(|| {
        compiled
            .as_ref()
            .map_or_else(|| "uncompiled".to_owned(), |c| c.policy_hash())
    });
    // a rule the contract itself denies is never offered as a grant
    let denied_by_contract =
        |rule: &str| compiled.as_ref().is_none_or(|c| rule_denied(&c.deny, rule));
    let rules: Vec<String> = b.needed.iter().take(MAX_RULES).cloned().collect();
    let grantable = !rules.is_empty() && !rules.iter().any(|r| denied_by_contract(r));
    let shown_rules: Vec<String> = rules.iter().map(|r| clean(r)).collect();
    let refused: Vec<String> = b
        .asked
        .iter()
        .chain(&b.denied)
        .take(MAX_RULES)
        .map(|c| clean(c))
        .collect();
    // the hash covers everything the worker reported, in full; the brief shows a cleaned, shortened
    // view. Grants (PR-10) must be made from the stored full action, never from the brief.
    let full_action = |extra: &str| {
        format!(
            "{extra}\n{}\n{}\n{}",
            b.needed.join("\n"),
            b.asked.join("\n"),
            b.denied.join("\n")
        )
    };
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
            full_action(&format!("{prompt}\n{}", options.join("\n"))),
        ),
        _ if b.plan && b.reason == "needs_user_input" => (
            "blocked",
            "the worker asked to leave plan mode, which an attempt cannot do".to_owned(),
            String::new(),
            vec![serde_json::json!({"id": "retry", "label": "run the work item again"})],
            full_action("ExitPlanMode"),
        ),
        _ if b.reason == "approval_unavailable" || !rules.is_empty() => {
            let alternatives = if grantable {
                vec![serde_json::json!({
                    "id": "grant_once",
                    "label": format!("allow exactly {} in one new attempt", shown_rules.join(", "))
                })]
            } else {
                vec![serde_json::json!({"id": "retry", "label": "run the work item again"})]
            };
            (
                "approval_needed",
                if grantable {
                    "the worker needed a permission the contract does not give".to_owned()
                } else {
                    "the worker needed something the contract denies: a grant is not offered; change the contract (a new version) instead".to_owned()
                },
                format!(
                    "not allowed by the contract: {} (refused calls: {})",
                    shown_rules.join(", "),
                    refused.join("; ")
                ),
                alternatives,
                full_action(""),
            )
        }
        _ => (
            "blocked",
            format!("the worker ended blocked: {}", clean(&b.reason)),
            String::new(),
            vec![serde_json::json!({"id": "retry", "label": "run the work item again"})],
            full_action(&b.reason),
        ),
    };
    let truncated = b.needed.len() > MAX_RULES
        || b.needed
            .iter()
            .chain(&b.asked)
            .chain(&b.denied)
            .any(|t| clean(t).chars().count() != t.chars().count());
    let brief = serde_json::json!({
        "why": why,
        "delta_from_contract": delta,
        "alternatives": alternatives,
        "evidence": {"work_item": item.key, "attempt_ordinal": attempt.ordinal,
                     "attempt_session": attempt.session_id, "reason": clean(&b.reason),
                     "refused_calls": refused,
                     "action_sha256": sha(&action), "text_shortened_or_cleaned": truncated},
        "unknowns": [],
        "safe_default": "HOLD",
    });
    let binding = serde_json::json!({
        "contract_version": contract_version,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn invisible_and_direction_changing_characters_are_removed() {
        let evil = "a\u{202E}b\u{2066}c\u{200B}d\u{FEFF}e\u{2028}f\u{E0041}g\u{1b}[31mh\u{00AD}i";
        assert_eq!(clean(evil), "abcdefg[31mhi");
        assert!(clean(&"x".repeat(1000)).chars().count() < 310);
    }

    #[test]
    fn a_rule_the_contract_denies_is_not_grantable() {
        let deny: Vec<String> = ["Bash(git push:*)", "Edit", "Read(/s/**)"]
            .map(String::from)
            .to_vec();
        assert!(rule_denied(&deny, "Bash(git push:*)"));
        assert!(rule_denied(&deny, "Write"), "the edit tools are one family");
        assert!(rule_denied(&deny, "Edit(/src/x)"));
        assert!(rule_denied(&deny, "NotebookEdit(/n)"));
        assert!(rule_denied(&deny, "Read(/s/**)"));
        assert!(!rule_denied(&deny, "Read(/other)"));
        assert!(!rule_denied(&deny, "Bash(cargo test:*)"));
        assert!(rule_denied(&deny, "browser_navigate(domain:x)"));
        assert!(rule_denied(&deny, "mcp__s__t"));
    }
}
