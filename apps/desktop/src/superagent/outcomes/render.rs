//! What the Super Agent shows of an outcome as plain strings: the decision card layout and the budget line.

use synthesis_view::outcomes::{BriefRow, BudgetView, DecisionCard};

/// What a decision card shows, as plain strings: the view draws exactly these, and a test proves that every
/// non-empty field of the brief is in them (`synthesis_view::outcomes::uncovered`).
pub struct CardLayout {
    pub title: String,
    pub state: String,
    pub meta: String,
    pub rows: Vec<BriefRow>,
    pub proof: String,
}

pub fn card_layout(c: &DecisionCard) -> CardLayout {
    CardLayout {
        title: c.kind.clone(),
        state: c.state.clone(),
        meta: format!(
            "decision {} \u{b7} revision {} \u{b7} expires {}",
            c.id,
            c.revision,
            if c.expires_at.is_empty() {
                "unknown"
            } else {
                &c.expires_at
            }
        ),
        rows: c.rows.clone(),
        proof: format!(
            "An answer is sent with revision {} and brief hash {}.",
            c.revision, c.brief_sha256
        ),
    }
}

impl CardLayout {
    /// Everything the card draws, one string (the coverage test reads this).
    #[cfg(test)]
    pub fn text(&self) -> String {
        let mut s = format!("{}\n{}\n{}\n", self.title, self.state, self.meta);
        for r in &self.rows {
            s.push_str(&format!("{}\n{}\n", r.label, r.text));
        }
        s.push_str(&self.proof);
        s
    }
}

fn money(a: Option<f64>) -> String {
    a.map_or_else(|| "unknown".into(), |v| format!("${v:.2}"))
}

/// "$1.20 of $5.00", or "budget unknown" when the server gave neither (the list's line, here and in the TUI view).
pub(crate) fn budget_line(b: &BudgetView) -> String {
    match (b.cost_usd.spent, b.cost_usd.limit) {
        (None, None) => "budget unknown".to_string(),
        (s, l) => format!("{} of {}", money(s), money(l)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};
    use synthesis_view::outcomes::uncovered;

    fn briefs() -> Vec<Value> {
        vec![
            json!({"why": "the worker needs an answer", "delta_from_contract": "Which API shape?",
                   "alternatives": [{"id": "REST", "label": "REST", "tradeoffs": "simple, chatty"},
                                    {"id": "RPC", "label": "RPC", "tradeoffs": "fast, coupled"}],
                   "evidence": {"work_item": "one", "attempt_ordinal": 3, "refused_calls": ["Bash: rm x"]},
                   "unknowns": ["is the client typed?"], "defer_consequence": "the item stays on hold",
                   "safe_default": "HOLD", "future_field": "shown even though this build does not know it"}),
            // a plan decision: nested fields of every item, a diff, a trigger
            json!({"trigger": "item_blocked", "plan": {"source": "model", "plan_hash": "abc123", "items": [
                    {"key": "w1", "kind": "code", "requires": [], "title": "first", "instructions": "do the first thing"},
                    {"key": "w2", "kind": "test", "requires": ["w1"], "title": "second", "instructions": "x\ny"}],
                   "max_attempts_per_work_item": 3, "outcome_cost_cap_usd": 5.5,
                   "diff_vs_current_plan": {"current_plan_version": 2, "diff": {"added": ["w2"], "removed": [], "changed": ["w1"]}}},
                   "alternatives": [{"id": "approve_model_plan", "label": "Approve"}, {"id": "keep_current_plan"}]}),
            json!({"unknown_a": {"nested": {"deep": "kept"}}, "alternatives": [], "why_human": "e\u{1b}[31mscaped"}),
        ]
    }

    #[test]
    fn the_card_layout_shows_every_non_empty_field_of_the_brief_and_the_proof_of_what_was_seen() {
        for brief in briefs() {
            let c = DecisionCard::from_json(&json!({"id": "d1", "kind": "ambiguity", "state": "pending", "revision": 7,
                "brief_sha256": "c".repeat(64), "expires_at": "2030-01-01T00:00:00.000Z", "brief": brief.clone()}))
            .unwrap();
            let lay = card_layout(&c);
            let shown = lay.text();
            assert_eq!(uncovered(&brief, &shown), Vec::<String>::new(), "{shown}");
            // the server's state, expiry, the revision and the whole brief hash are on the card
            for want in [
                "pending",
                "2030-01-01T00:00:00.000Z",
                "revision 7",
                &"c".repeat(64),
            ] {
                assert!(shown.contains(want), "{want} in {shown}");
            }
            assert!(!shown.contains('\u{1b}'), "control text made visible");
            assert!(!lay.rows.is_empty());
        }
        // the check does catch a layout that drops a field
        let c = DecisionCard::from_json(
            &json!({"id": "d1", "kind": "k", "state": "pending", "revision": 1,
            "brief_sha256": "d".repeat(64), "brief": briefs().remove(0)}),
        )
        .unwrap();
        assert!(
            !uncovered(
                &briefs()[0],
                &card_layout(&c).text().replace("fast, coupled", "")
            )
            .is_empty()
        );
    }
}
