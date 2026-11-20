//! The orchestrator chat's proposal card as a pure view model (super-agent-s1.md 4.3, 4.8 T3, T12). It is
//! built from the host's `Proposal` JSON and nothing else, so model text can never forge a card: every payload
//! field is shown, nothing is preselected, and the taint reason and the forced-card reasons are part of the card.
//! Rendering and the click handler are P4.

use serde_json::Value;
use synthesis_harness::sanitize_for_display;

/// A button on an open card. The person picks one; there is no default.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CardButton {
    /// Runs the proposal (label by kind: Send, Start session, Create draft, Interrupt, Close, Pause).
    Confirm,
    /// Only on cards with free text (a message, a first message, a contract).
    Edit,
    Cancel,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ProposalCard {
    pub id: String,
    pub title: String,
    pub confirm_label: &'static str,
    /// Every payload field, in order, as shown text.
    pub fields: Vec<(String, String)>,
    /// Why the person must look closely: set when the conductor read session content before proposing.
    pub taint: Option<String>,
    pub forced: Vec<String>,
    pub state: String,
    /// First 8 hex digits of the payload's sha256, shown so a swapped card is visible.
    pub sha8: String,
    pub buttons: Vec<CardButton>,
    /// Always `None`: nothing is preselected (T12).
    pub preselected: Option<CardButton>,
}

impl ProposalCard {
    pub fn from_json(p: &Value) -> Result<Self, String> {
        let s = |v: &Value, k: &str| v.get(k).and_then(Value::as_str).map(str::to_string);
        let id = s(p, "id").ok_or("proposal: no id")?;
        let payload = p
            .get("payload")
            .and_then(Value::as_object)
            .ok_or("proposal: no payload")?;
        let kind = payload
            .get("kind")
            .and_then(Value::as_str)
            .ok_or("proposal: no kind")?;
        let (title, confirm_label, editable) = match kind {
            "create_session" => ("New session".to_string(), "Start session", true),
            "create_goal" => (
                format!(
                    "Goal draft ({})",
                    payload
                        .get("goal_kind")
                        .and_then(Value::as_str)
                        .unwrap_or("?")
                ),
                "Create draft",
                true,
            ),
            "send_message" => (
                format!(
                    "Message to {}",
                    payload
                        .get("session")
                        .and_then(Value::as_str)
                        .unwrap_or("?")
                ),
                "Send",
                true,
            ),
            "interrupt" => ("Interrupt a session".into(), "Interrupt", false),
            "close" => ("Close a session".into(), "Close", false),
            "pause_outcome" => ("Pause an outcome".into(), "Pause", false),
            other => {
                return Err(format!(
                    "proposal: unknown kind {}",
                    sanitize_for_display(other)
                ));
            }
        };
        let mut fields = vec![];
        for (k, v) in payload.iter().filter(|(k, _)| k.as_str() != "kind") {
            let text = match v {
                Value::Null => continue,
                Value::String(t) => t.clone(),
                Value::Array(a) => a
                    .iter()
                    .map(|x| match (x.get("handle"), x.get("title")) {
                        (Some(h), Some(t)) => format!("{} \"{}\"", plain(h), plain(t)),
                        _ => plain(x),
                    })
                    .collect::<Vec<_>>()
                    .join(", "),
                other => other.to_string(),
            };
            fields.push((k.clone(), sanitize_for_display(&text)));
        }
        let sources: Vec<&str> = p
            .get("taint_sources")
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(Value::as_str).collect())
            .unwrap_or_default();
        let taint = (p.get("tainted").and_then(Value::as_bool) == Some(true)).then(|| {
            format!(
                "The conductor had read text from other sessions ({}). Check the target and the wording.",
                sanitize_for_display(&sources.join(", "))
            )
        });
        let state = s(p, "state").unwrap_or_default();
        let mut buttons = vec![];
        if state == "shown" {
            buttons.push(CardButton::Confirm);
            if editable {
                buttons.push(CardButton::Edit);
            }
            buttons.push(CardButton::Cancel);
        }
        Ok(Self {
            id,
            title,
            confirm_label,
            fields,
            taint,
            forced: p
                .get("card_forced")
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(Value::as_str)
                        .map(sanitize_for_display)
                        .collect()
                })
                .unwrap_or_default(),
            state,
            sha8: s(p, "payload_sha256")
                .unwrap_or_default()
                .chars()
                .take(8)
                .collect(),
            buttons,
            preselected: None,
        })
    }

    pub fn render_text(&self) -> String {
        let mut out = format!("{} [{}] #{}\n", self.title, self.state, self.sha8);
        if let Some(t) = &self.taint {
            out += &format!("! {t}\n");
        }
        for r in &self.forced {
            out += &format!("! {r}\n");
        }
        for (k, v) in &self.fields {
            out += &format!("{k}: {v}\n");
        }
        out
    }
}

fn plain(v: &Value) -> String {
    v.as_str().map_or_else(|| v.to_string(), str::to_string)
}

/// Payload fields the rendered card does not show (should be empty: nothing is hidden from the person).
pub fn uncovered(proposal: &Value, rendered: &str) -> Vec<String> {
    let mut out = vec![];
    if let Some(o) = proposal.get("payload").and_then(Value::as_object) {
        for (k, v) in o.iter().filter(|(k, _)| k.as_str() != "kind") {
            let text = match v {
                Value::String(s) => sanitize_for_display(s),
                Value::Array(a) => a
                    .iter()
                    .filter_map(|x| x.get("handle").and_then(Value::as_str))
                    .collect::<Vec<_>>()
                    .join(", "),
                Value::Null => continue,
                other => other.to_string(),
            };
            if !rendered.contains(&text) || !rendered.contains(k.as_str()) {
                out.push(k.clone());
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn send(state: &str, tainted: bool) -> Value {
        json!({"id": "p1", "state": state, "tainted": tainted, "taint_sources": ["session:s1"],
               "card_forced": ["the target was not named by you"], "payload_sha256": "abcdef0123456789",
               "payload": {"kind": "send_message", "session": "s2", "sid": "0123abcd-x", "text": "run the tests\u{202e}evil",
                           "basis": "described", "alternatives": [{"handle": "s3", "title": "auth refactor"}]}})
    }

    #[test]
    fn every_field_is_shown_nothing_is_preselected_and_hidden_text_is_visible() {
        let p = send("shown", true);
        let c = ProposalCard::from_json(&p).unwrap();
        let t = c.render_text();
        assert!(uncovered(&p, &t).is_empty(), "{:?}", uncovered(&p, &t));
        assert_eq!(c.preselected, None);
        assert_eq!(
            c.buttons,
            [CardButton::Confirm, CardButton::Edit, CardButton::Cancel]
        );
        assert!(t.contains("\\u{202e}") && !t.contains('\u{202e}'));
        assert!(c.taint.as_deref().unwrap().contains("session:s1"));
        assert!(t.contains("s3 \"auth refactor\"") && t.contains("#abcdef01"));
    }

    #[test]
    fn a_decided_card_has_no_buttons_and_an_unknown_kind_is_refused() {
        assert!(
            ProposalCard::from_json(&send("accepted", false))
                .unwrap()
                .buttons
                .is_empty()
        );
        let mut bad = send("shown", false);
        bad["payload"]["kind"] = json!("approve_everything");
        assert!(ProposalCard::from_json(&bad).is_err());
        let close = json!({"id": "p2", "state": "shown", "payload": {"kind": "close", "session": "s1", "sid": "x"}});
        assert_eq!(
            ProposalCard::from_json(&close).unwrap().buttons,
            [CardButton::Confirm, CardButton::Cancel]
        );
    }
}
