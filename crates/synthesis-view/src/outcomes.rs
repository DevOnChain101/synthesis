//! Outcome view models (ADR-021 section 2), built from protocol v4 replies. A surface renders
//! them and never computes a verdict, a state, a binding or a brief: everything here is the
//! server's data, made printable (the engine is not trusted) and laid out for any front end.
//!
//! A decision brief is shown whole: every non-empty field the server sent appears, alternatives
//! keep the server's order, and nothing is preselected. A brief that names a recommended or
//! selected option is refused, so a hostile or buggy engine cannot steer a person's choice.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use synthesis_harness::sanitize_for_display as clean;
use synthesis_session::outcome::CommandState;

/// The inspector tabs, defined once for every surface.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InspectorTab {
    Overview,
    Workers,
    Activity,
    Diff,
    Evidence,
}

impl InspectorTab {
    pub const ALL: [InspectorTab; 5] = [
        Self::Overview,
        Self::Workers,
        Self::Activity,
        Self::Diff,
        Self::Evidence,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::Overview => "Overview",
            Self::Workers => "Workers",
            Self::Activity => "Activity",
            Self::Diff => "Diff",
            Self::Evidence => "Evidence",
        }
    }

    /// `synt outcome inspect --tab overview|workers|activity|diff|evidence`.
    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|t| t.label().eq_ignore_ascii_case(s))
    }
}

/// What waits for a person, as the server derived it from rows.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct NeedsYou {
    pub decisions_pending: u32,
    pub blocked: bool,
    pub verified_awaiting_ack: bool,
}

impl NeedsYou {
    pub fn any(&self) -> bool {
        self.decisions_pending > 0 || self.blocked || self.verified_awaiting_ack
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct OutcomeRow {
    pub id: String,
    pub goal: String,
    pub state: String,
    pub revision: i64,
    pub needs_you: NeedsYou,
}

fn text(v: &Value, k: &str) -> Option<String> {
    v.get(k)?.as_str().map(clean)
}

impl OutcomeRow {
    pub fn from_json(v: &Value) -> Option<Self> {
        Some(Self {
            id: text(v, "id")?,
            goal: text(v, "goal")?,
            state: text(v, "state")?,
            revision: v.get("revision")?.as_i64()?,
            needs_you: serde_json::from_value(v.get("needs_you")?.clone()).ok()?,
        })
    }
}

/// Brief fields that would pre-select an option (the store refuses them too, ADR-019).
const PRESELECT: [&str; 5] = [
    "selected",
    "recommended",
    "default",
    "default_choice",
    "preselected",
];

/// What a person may answer besides the alternatives of the brief.
const FIXED: [(&str, &str); 3] = [
    ("reject", "Reject"),
    ("request_evidence", "Request evidence"),
    ("edit_scope", "Edit scope"),
];

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Alternative {
    pub id: String,
    pub label: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BriefRow {
    pub label: String,
    pub text: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct DecisionCard {
    pub id: String,
    pub kind: String,
    /// As the server states it: `pending`, `approved`, `expired`, `invalidated`, ... Only a
    /// `pending` decision can be answered.
    pub state: String,
    pub revision: i64,
    /// Echoed back with the answer: it proves which brief the person saw.
    pub brief_sha256: String,
    pub expires_at: String,
    pub rows: Vec<BriefRow>,
    pub alternatives: Vec<Alternative>,
}

fn empty(v: &Value) -> bool {
    match v {
        Value::Null => true,
        Value::String(s) => s.trim().is_empty(),
        Value::Array(a) => a.iter().all(empty),
        Value::Object(o) => o.values().all(empty),
        _ => false,
    }
}

fn flat(v: &Value, out: &mut Vec<String>) {
    match v {
        Value::String(s) => out.push(clean(s)),
        Value::Array(a) => {
            for x in a.iter().filter(|x| !empty(x)) {
                let mut inner = vec![];
                flat(x, &mut inner);
                out.push(format!("- {}", inner.join("; ")));
            }
        }
        Value::Object(o) => {
            // `id` first (an alternative reads "id: label"), the rest in the server's key order
            let keys = o
                .keys()
                .filter(|k| *k == "id")
                .chain(o.keys().filter(|k| *k != "id"));
            for k in keys.filter(|k| !empty(&o[*k])) {
                let mut inner = vec![];
                flat(&o[k], &mut inner);
                out.push(format!("{}: {}", clean(k), inner.join("; ")));
            }
        }
        Value::Null => {}
        other => out.push(other.to_string()),
    }
}

fn heading(key: &str) -> String {
    match key {
        "why" | "why_human" => "Why you",
        "delta_from_contract" | "delta" => "Change from the contract",
        "alternatives" => "Alternatives",
        "evidence" | "evidence_refs" => "Evidence",
        "unknowns" => "Unknowns",
        "defer_consequence" => "If you defer",
        "safe_default" | "default_action" => "If you do nothing",
        other => return clean(other),
    }
    .into()
}

/// One row per non-empty field of the brief, in the server's order, unknown fields included.
pub fn brief_rows(brief: &Value) -> Vec<BriefRow> {
    let Some(o) = brief.as_object() else {
        return vec![];
    };
    o.iter()
        .filter(|(_, v)| !empty(v))
        .map(|(k, v)| {
            let mut lines = vec![];
            flat(v, &mut lines);
            BriefRow {
                label: heading(k),
                text: lines.join("\n"),
            }
        })
        .collect()
}

/// Every non-empty leaf value of a brief (as shown), for the renderer field-coverage test.
fn leaves(v: &Value, out: &mut Vec<String>) {
    match v {
        Value::String(s) if !s.trim().is_empty() => out.push(clean(s)),
        Value::Array(a) => a.iter().for_each(|x| leaves(x, out)),
        Value::Object(o) => o.values().for_each(|x| leaves(x, out)),
        Value::Number(_) | Value::Bool(_) => out.push(v.to_string()),
        _ => {}
    }
}

/// The values of the brief that `rendered` does not contain: a surface's own layout of a brief
/// is correct only when this is empty (`surface-brief-all-fields-shown`).
pub fn uncovered(brief: &Value, rendered: &str) -> Vec<String> {
    let mut all = vec![];
    leaves(brief, &mut all);
    all.retain(|l| !rendered.contains(l.as_str()));
    all
}

impl DecisionCard {
    /// `Err` for a decision this build must not offer a choice on: a malformed reply, or a brief
    /// that names a preferred option.
    pub fn from_json(v: &Value) -> Result<Self, String> {
        let bad = |what: &str| format!("unreadable decision: {what}");
        let brief = v
            .get("brief")
            .filter(|b| b.is_object())
            .ok_or_else(|| bad("no brief"))?;
        let alts = brief.get("alternatives").and_then(Value::as_array);
        let preselected = |o: &Value| PRESELECT.iter().find(|k| o.get(**k).is_some());
        if let Some(k) = preselected(brief).or_else(|| alts?.iter().find_map(preselected)) {
            return Err(format!(
                "the decision brief carries a {k:?} field: refused, the choice is the person's"
            ));
        }
        let alternatives: Vec<Alternative> = alts
            .map(|a| {
                a.iter()
                    .filter_map(|x| {
                        let id = x.get("id")?.as_str()?;
                        let label = x.get("label").or_else(|| x.get("name"));
                        Some(Alternative {
                            id: id.to_string(),
                            label: clean(label.and_then(Value::as_str).unwrap_or(id)),
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();
        // an alternative named like a fixed answer would let the brief stand in for it
        if let Some(a) = alternatives
            .iter()
            .find(|a| FIXED.iter().any(|(id, _)| *id == a.id))
        {
            return Err(format!(
                "the brief offers {:?}, which is a fixed answer",
                a.id
            ));
        }
        Ok(Self {
            id: text(v, "id").ok_or_else(|| bad("no id"))?,
            kind: text(v, "kind").ok_or_else(|| bad("no kind"))?,
            state: text(v, "state").ok_or_else(|| bad("no state"))?,
            revision: v
                .get("revision")
                .and_then(Value::as_i64)
                .ok_or_else(|| bad("no revision"))?,
            brief_sha256: text(v, "brief_sha256").ok_or_else(|| bad("no brief hash"))?,
            expires_at: text(v, "expires_at").unwrap_or_default(),
            rows: brief_rows(brief),
            alternatives,
        })
    }

    pub fn resolvable(&self) -> bool {
        self.state == "pending"
    }

    /// The card as plain lines: the server's state and expiry first, then every brief field.
    pub fn render_text(&self) -> String {
        let mut out = format!(
            "Decision {} ({}), {}, expires {}\n",
            self.id, self.kind, self.state, self.expires_at
        );
        for r in &self.rows {
            out.push_str(&format!("{}:\n{}\n", r.label, r.text));
        }
        out
    }
}

/// A choice list with no default: the cursor starts nowhere and only the person moves it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Picker {
    pub options: Vec<Alternative>,
    cursor: Option<usize>,
}

impl Picker {
    /// The alternatives in the brief's order, then reject, request evidence and edit scope.
    /// `None` when the decision cannot be answered (not `pending`).
    pub fn new(card: &DecisionCard) -> Option<Self> {
        card.resolvable().then(|| Self {
            options: card
                .alternatives
                .iter()
                .cloned()
                .chain(FIXED.iter().map(|(id, label)| Alternative {
                    id: (*id).into(),
                    label: (*label).into(),
                }))
                .collect(),
            cursor: None,
        })
    }

    pub fn move_to(&mut self, i: usize) {
        self.cursor = (i < self.options.len()).then_some(i);
    }

    pub fn cursor(&self) -> Option<usize> {
        self.cursor
    }

    /// The id to send; `None` until the person chose.
    pub fn choice(&self) -> Option<&str> {
        self.cursor.map(|i| self.options[i].id.as_str())
    }
}

/// The server's answer to a mutating call.
#[derive(Clone, Debug, PartialEq)]
pub struct CommandReply {
    pub idempotency_key: String,
    pub kind: String,
    pub state: CommandState,
    pub result: Value,
}

impl CommandReply {
    pub fn from_json(v: &Value) -> Option<Self> {
        Some(Self {
            idempotency_key: v.get("idempotency_key")?.as_str()?.to_string(),
            kind: v.get("kind")?.as_str()?.to_string(),
            state: serde_json::from_value(v.get("state")?.clone()).ok()?,
            result: v.get("result").cloned().unwrap_or(Value::Null),
        })
    }

    /// What to tell the person; a lost race says to refetch, never to retry with a guessed revision.
    pub fn describe(&self) -> &'static str {
        match self.state {
            CommandState::Queued => "queued: it is applied when the orchestrator runs",
            CommandState::Applied => "done",
            CommandState::Stale => "stale: it changed since it was shown; reload and look again",
            CommandState::AlreadyResolved => "already answered; reload to see who answered and how",
            CommandState::Denied => "refused",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn decision(brief: Value) -> Value {
        json!({"id": "d1", "kind": "ambiguity", "state": "pending", "revision": 2,
               "brief_sha256": "a".repeat(64), "expires_at": "2030-01-01T00:00:00.000Z", "brief": brief})
    }

    fn full_brief() -> Value {
        json!({"why": "the worker needs an answer", "delta_from_contract": "Which API shape?",
               "alternatives": [{"id": "REST", "label": "REST", "tradeoffs": "simple, chatty"},
                                {"id": "RPC", "label": "RPC", "tradeoffs": "fast, coupled"}],
               "evidence": {"work_item": "one", "attempt_ordinal": 3, "refused_calls": ["Bash: rm x"]},
               "unknowns": ["is the client typed?"], "defer_consequence": "the item stays on hold",
               "safe_default": "HOLD", "future_field": "shown even though this build does not know it"})
    }

    // surface-brief-all-fields-shown (the shared renderer; each surface runs `uncovered` on its own layout)
    #[test]
    fn every_non_empty_field_of_a_brief_is_shown() {
        let card = DecisionCard::from_json(&decision(full_brief())).unwrap();
        let shown = card.render_text();
        assert_eq!(
            uncovered(&full_brief(), &shown),
            Vec::<String>::new(),
            "{shown}"
        );
        assert!(shown.contains("2030-01-01") && shown.contains("pending"));
        // the helper does catch a layout that drops a field
        assert_eq!(
            uncovered(&full_brief(), &shown.replace("fast, coupled", "")),
            vec!["fast, coupled".to_string()]
        );
        // empty fields are not shown, and control text is made visible
        let rows = brief_rows(&json!({"why": "", "unknowns": [], "x": "a\u{1b}[31mb"}));
        assert_eq!(rows.len(), 1);
        assert!(!rows[0].text.contains('\u{1b}'));
        // alternatives keep the server's order
        let ids: Vec<_> = card.alternatives.iter().map(|a| a.id.as_str()).collect();
        assert_eq!(ids, ["REST", "RPC"]);
    }

    // surface-no-preselection
    #[test]
    fn nothing_is_preselected_and_a_preferred_option_is_refused() {
        let card = DecisionCard::from_json(&decision(full_brief())).unwrap();
        let mut p = Picker::new(&card).unwrap();
        assert_eq!((p.cursor(), p.choice()), (None, None));
        let ids: Vec<_> = p.options.iter().map(|a| a.id.as_str()).collect();
        assert_eq!(
            ids,
            ["REST", "RPC", "reject", "request_evidence", "edit_scope"]
        );
        p.move_to(99);
        assert_eq!(p.choice(), None);
        p.move_to(1);
        assert_eq!(p.choice(), Some("RPC"));
        for k in PRESELECT {
            let mut b = full_brief();
            b[k] = json!("RPC");
            let err = DecisionCard::from_json(&decision(b)).unwrap_err();
            assert!(err.contains(k), "{err}");
        }
    }

    #[test]
    fn a_preselect_key_inside_an_alternative_or_a_fixed_id_is_refused() {
        let mut b = full_brief();
        b["alternatives"][1]["recommended"] = json!(true);
        assert!(DecisionCard::from_json(&decision(b)).is_err());
        let mut b = full_brief();
        b["alternatives"][0]["id"] = json!("reject");
        assert!(DecisionCard::from_json(&decision(b)).is_err());
    }

    #[test]
    fn only_a_pending_decision_can_be_answered_and_the_state_is_the_servers() {
        for state in [
            "approved",
            "expired",
            "invalidated",
            "unverified",
            "preparing",
        ] {
            let mut d = decision(full_brief());
            d["state"] = json!(state);
            let card = DecisionCard::from_json(&d).unwrap();
            assert!(
                !card.resolvable() && Picker::new(&card).is_none(),
                "{state}"
            );
            assert!(card.render_text().contains(state));
        }
        for bad in [
            json!({}),
            json!({"id": "x", "kind": "k", "state": "pending", "revision": 1}),
        ] {
            assert!(DecisionCard::from_json(&bad).is_err());
        }
    }

    #[test]
    fn rows_replies_and_tabs() {
        let row = json!({"id": "o1", "goal": "g", "state": "running", "revision": 4,
            "needs_you": {"decisions_pending": 1, "blocked": false, "verified_awaiting_ack": false}});
        let r = OutcomeRow::from_json(&row).unwrap();
        assert!(r.needs_you.any() && !NeedsYou::default().any());
        assert!(OutcomeRow::from_json(&json!({"id": "o"})).is_none());
        let reply = json!({"idempotency_key": "k", "kind": "decision.resolve", "state": "already_resolved"});
        let c = CommandReply::from_json(&reply).unwrap();
        assert_eq!(c.state, CommandState::AlreadyResolved);
        assert!(c.describe().contains("reload"));
        assert!(
            CommandReply::from_json(
                &json!({"idempotency_key": "k", "kind": "x", "state": "maybe"})
            )
            .is_none()
        );
        let tabs: Vec<_> = InspectorTab::ALL.iter().map(|t| t.label()).collect();
        assert_eq!(
            tabs,
            ["Overview", "Workers", "Activity", "Diff", "Evidence"]
        );
        assert_eq!(InspectorTab::parse("diff"), Some(InspectorTab::Diff));
        assert_eq!(InspectorTab::parse("x"), None);
    }
}
