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

pub mod views;
pub use views::*;

/// What every surface shows on native Windows (the orchestrator says the same; a test in the CLI
/// keeps the two equal). Writable attempts need an OS sandbox.
pub const WSL2_REFUSAL: &str = "outcome attempts write files and need an OS sandbox: they run on macOS, Linux and WSL2, not on native Windows; open the folder in WSL2";

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
            id: text(v, "id").filter(|i| valid_alt_id(i))?,
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

/// What an answer id may be: plain ASCII, so it prints as it is sent.
pub fn valid_alt_id(id: &str) -> bool {
    (1..=64).contains(&id.len())
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'-'))
}

/// Exactly `n` lowercase hex digits: a hash that is printed in a command to paste.
pub fn is_hex(s: &str, n: usize) -> bool {
    s.len() == n && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
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
        // a read-only goal's `report_review` (S1 section 2.3)
        "report" => "Report (the worker's own words, untrusted)",
        "rubric" => "Rubric",
        "citations" => "Citations",
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
        // the id is printed and sent back as typed, so it is never cleaned: a bad one refuses the card
        let mut alternatives: Vec<Alternative> = vec![];
        for x in alts.into_iter().flatten() {
            let Some(id) = x.get("id").and_then(Value::as_str) else {
                continue;
            };
            if !valid_alt_id(id) {
                return Err(format!(
                    "the brief has an alternative id that is not 1 to 64 of A-Z a-z 0-9 _ . -: {}",
                    clean(id)
                ));
            }
            if alternatives.iter().any(|a| a.id == id) {
                return Err(format!("the brief offers the answer id {id:?} twice"));
            }
            let label = x.get("label").or_else(|| x.get("name"));
            alternatives.push(Alternative {
                id: id.to_string(),
                label: clean(label.and_then(Value::as_str).unwrap_or(id)),
            });
        }
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
            id: text(v, "id")
                .filter(|i| valid_alt_id(i))
                .ok_or_else(|| bad("no id, or not 1 to 64 of A-Z a-z 0-9 _ . -"))?,
            kind: text(v, "kind").ok_or_else(|| bad("no kind"))?,
            state: text(v, "state").ok_or_else(|| bad("no state"))?,
            revision: v
                .get("revision")
                .and_then(Value::as_i64)
                .ok_or_else(|| bad("no revision"))?,
            brief_sha256: text(v, "brief_sha256")
                .filter(|h| is_hex(h, 64))
                .ok_or_else(|| bad("no brief hash, or not 64 hex digits"))?,
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

/// How a message to a worker stands (S1 4.6).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MessageState {
    /// Sent to a running attempt; its worker has not acked yet.
    Live,
    /// The worker took it.
    Delivered,
    /// Waits for the next attempt of the work item.
    Queued,
    /// A live message its worker never took, queued again.
    NotDelivered,
    /// The next attempt carries it in its first prompt.
    InPrompt,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MessageRow {
    pub seq: u64,
    pub state: MessageState,
    /// The attempt a live message went to (short, printable).
    pub attempt: Option<String>,
    pub text: String,
}

impl MessageRow {
    /// The message rows of an outcome's control-session `events` (`events.subscribe` pages, oldest
    /// first) and the `seq`s its workers acked (`messages_delivered` of the attempts). A live message
    /// that was queued again is shown once, as the queued one. Printable: the server's text is not trusted.
    pub fn from_events(events: &[Value], delivered: &[u64]) -> Vec<Self> {
        let ev = |e: &Value| {
            (
                e.get("type")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                e.get("payload").cloned().unwrap_or(Value::Null),
            )
        };
        let mut requeued = vec![];
        let mut taken = vec![];
        for e in events {
            match ev(e) {
                (k, p) if k == "attempt.message.queued" => {
                    requeued.extend(p.get("requeued_from").and_then(Value::as_u64));
                }
                (k, p) if k == "attempt.message.consumed" => {
                    taken.extend(
                        p.get("seqs")
                            .and_then(Value::as_array)
                            .into_iter()
                            .flatten()
                            .filter_map(Value::as_u64),
                    );
                }
                _ => {}
            }
        }
        events
            .iter()
            .filter_map(|e| {
                let (kind, p) = ev(e);
                let seq = p.get("seq")?.as_u64()?;
                let body = clean(p.get("text")?.as_str()?);
                let (state, attempt) = match kind.as_str() {
                    "attempt.message" if requeued.contains(&seq) => return None,
                    "attempt.message" if delivered.contains(&seq) => {
                        (MessageState::Delivered, text(&p, "attempt"))
                    }
                    "attempt.message" => (MessageState::Live, text(&p, "attempt")),
                    "attempt.message.queued" if taken.contains(&seq) => {
                        (MessageState::InPrompt, None)
                    }
                    "attempt.message.queued" if p.get("requeued_from").is_some() => {
                        (MessageState::NotDelivered, None)
                    }
                    "attempt.message.queued" => (MessageState::Queued, None),
                    _ => return None,
                };
                Some(Self {
                    seq,
                    state,
                    attempt: attempt.map(|a| a.chars().take(8).collect()),
                    text: body,
                })
            })
            .collect()
    }

    /// What the row says about delivery.
    pub fn label(&self) -> String {
        let to = self.attempt.as_deref().unwrap_or("the worker");
        match self.state {
            MessageState::Live => format!("sent to {to} (live)"),
            MessageState::Delivered => format!("delivered to {to}"),
            MessageState::Queued => "queued for the next attempt".into(),
            MessageState::NotDelivered => "not delivered, queued for the next attempt".into(),
            MessageState::InPrompt => "taken by the next attempt".into(),
        }
    }

    pub fn render_text(&self) -> String {
        format!("{}: {}\n", self.label(), self.text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn message_rows_show_live_delivered_queued_and_not_delivered_once() {
        let msg = |seq: u64, kind: &str, p: Value| {
            let mut p = p;
            p["seq"] = json!(seq);
            p["text"] = json!(format!("m{seq}\u{1b}[31m"));
            json!({"seq": seq, "type": kind, "payload": p})
        };
        let events = [
            msg(5, "attempt.message", json!({"attempt": "a7a7a7a7-1111"})),
            msg(6, "attempt.message", json!({"attempt": "a7a7a7a7-1111"})),
            msg(
                7,
                "attempt.message.queued",
                json!({"work_item": "w", "requeued_from": 6}),
            ),
            msg(8, "attempt.message.queued", json!({"work_item": "w"})),
            msg(9, "attempt.message.queued", json!({"work_item": "w"})),
            json!({"seq": 10, "type": "attempt.message.consumed", "payload": {"seqs": [9]}}),
        ];
        let rows = MessageRow::from_events(&events, &[5]);
        let got: Vec<_> = rows.iter().map(|r| (r.seq, r.label())).collect();
        assert_eq!(
            got,
            [
                (5, "delivered to a7a7a7a7".to_string()),
                (7, "not delivered, queued for the next attempt".to_string()),
                (8, "queued for the next attempt".to_string()),
                (9, "taken by the next attempt".to_string()),
            ]
        );
        assert!(rows.iter().all(|r| !r.text.contains('\u{1b}')));
        let live = MessageRow::from_events(&events[..1], &[]);
        assert_eq!(live[0].label(), "sent to a7a7a7a7 (live)");
    }

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

    #[test]
    fn a_report_review_brief_is_shown_whole_with_the_report_made_printable() {
        let brief = json!({"why": "a worker finished", "alternatives": [{"id": "accept_report", "label": "accept"}],
            "report": "all good\u{1b}[31m", "rubric": ["r1: names the risks"],
            "citations": [{"path": "a.txt", "lines": "1-2", "result": "verified"}]});
        let card = DecisionCard::from_json(&decision(brief.clone())).unwrap();
        let shown = card.render_text();
        assert!(uncovered(&brief, &shown).is_empty());
        assert!(
            shown.contains("Report (the worker's own words, untrusted)")
                && shown.contains("Citations")
        );
        assert!(!shown.contains('\u{1b}'));
        let ids: Vec<_> = Picker::new(&card)
            .unwrap()
            .options
            .iter()
            .map(|o| o.id.clone())
            .collect();
        assert_eq!(
            ids,
            ["accept_report", "reject", "request_evidence", "edit_scope"]
        );
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
    fn an_alternative_id_that_is_not_plain_is_refused_not_cleaned() {
        let long = "a".repeat(65);
        for bad in [
            "a\u{1b}[31m",
            "\u{1b}]52;c;AAAA\u{7}",
            "a\nb",
            "a\u{202e}b",
            "a b",
            "",
            long.as_str(),
        ] {
            let mut b = full_brief();
            b["alternatives"][0]["id"] = json!(bad);
            assert!(DecisionCard::from_json(&decision(b)).is_err(), "{bad:?}");
        }
        let mut b = full_brief();
        b["alternatives"][0]["id"] = json!("opt-1.a_B");
        assert!(DecisionCard::from_json(&decision(b)).is_ok());
    }

    #[test]
    fn a_repeated_answer_id_a_bad_decision_id_and_a_bad_hash_are_refused() {
        let mut b = full_brief();
        b["alternatives"][1]["id"] = json!("REST");
        let e = DecisionCard::from_json(&decision(b)).unwrap_err();
        assert!(e.contains("twice"), "{e}");
        // the id and hash are printed in commands to paste: only their real shapes pass
        for (k, bad) in [
            ("id", "d1; rm -rf ~"),
            ("id", ""),
            ("brief_sha256", "abc"),
            ("brief_sha256", "A".repeat(64).as_str()),
            ("brief_sha256", format!("{}$(x)", "a".repeat(58)).as_str()),
        ] {
            let mut d = decision(full_brief());
            d[k] = json!(bad);
            assert!(DecisionCard::from_json(&d).is_err(), "{k} {bad:?}");
        }
        let row = json!({"id": "o1; x", "goal": "g", "state": "running", "revision": 1,
            "needs_you": {"decisions_pending": 0, "blocked": false, "verified_awaiting_ack": false}});
        assert!(OutcomeRow::from_json(&row).is_none());
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
