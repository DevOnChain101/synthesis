//! View models of `approvals.queue` and `approvals.history` (docs/specs/approvals.md). The server's
//! rows in the server's order, made printable and capped; a surface rates and orders nothing. Fail
//! closed: an unknown tier reads as T3, a recommendation counts only on T1, and a row that cannot
//! be read refuses the whole queue (an ask is never silently dropped).

use serde_json::Value;
use synthesis_harness::sanitize_for_display as clean;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use super::{Alternative, is_hex, valid_alt_id};

const MAX_ROWS: usize = 1000;
const MAX_TEXT: usize = 300;
/// Work item keys one row names before "+N".
const MAX_KEYS: usize = 5;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Tier {
    T0,
    T1,
    T2,
    T3,
}

impl Tier {
    fn parse(s: &str) -> Self {
        match s {
            "T0" => Self::T0,
            "T1" => Self::T1,
            "T2" => Self::T2,
            _ => Self::T3,
        }
    }

    /// The tier in words: it is never told by colour alone.
    pub const fn chip(self) -> &'static str {
        match self {
            Self::T0 => "info",
            Self::T1 => "T1 low",
            Self::T2 => "T2 review",
            Self::T3 => "T3 confirm",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    Decision,
    Contract,
    Lapsed,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QueueRow {
    pub outcome: String,
    pub goal: String,
    pub source: Source,
    /// `None` for a contract row.
    pub decision_id: Option<String>,
    pub kind: String,
    pub state: String,
    pub tier: Tier,
    /// The decision's revision (0 for a contract row).
    pub revision: i64,
    /// The contract's version (0 for a decision row).
    pub version: u64,
    /// The brief hash of a decision, the hash of a contract.
    pub hash: String,
    pub created_ms: Option<i64>,
    pub expires_ms: Option<i64>,
    pub recommended: Option<Alternative>,
    pub one_tap: bool,
    /// Keys of the work items this holds.
    pub blocks: Vec<String>,
    pub waiting_on_it: u64,
}

fn text(v: &Value, k: &str) -> String {
    v.get(k)
        .and_then(Value::as_str)
        .map(|s| clean(s).chars().take(MAX_TEXT).collect())
        .unwrap_or_default()
}

/// A server timestamp as unix milliseconds.
pub fn ts_ms(s: &str) -> Option<i64> {
    let t = OffsetDateTime::parse(s, &Rfc3339).ok()?;
    i64::try_from(t.unix_timestamp_nanos() / 1_000_000).ok()
}

/// "12s", "3m", "2h", "1d".
pub fn span(ms: i64) -> String {
    let s = (ms / 1000).max(0);
    match s {
        0..=59 => format!("{s}s"),
        60..=3599 => format!("{}m", s / 60),
        3600..=86_399 => format!("{}h", s / 3600),
        _ => format!("{}d", s / 86_400),
    }
}

impl QueueRow {
    fn from_json(v: &Value) -> Option<Self> {
        let source = match v.get("source")?.as_str()? {
            "decision" => Source::Decision,
            "contract" => Source::Contract,
            "lapsed" => Source::Lapsed,
            _ => return None,
        };
        // ids and hashes end up in commands a person pastes: any other shape is refused
        let outcome = text(v, "outcome");
        let decision_id = match source {
            Source::Contract => None,
            _ => Some(text(v, "decision_id")).filter(|i| valid_alt_id(i)),
        };
        let hash = text(
            v,
            if source == Source::Contract {
                "hash"
            } else {
                "brief_sha256"
            },
        );
        if !valid_alt_id(&outcome)
            || (source != Source::Contract && decision_id.is_none())
            || !is_hex(&hash, 64)
        {
            return None;
        }
        let tier = Tier::parse(v.get("tier").and_then(Value::as_str).unwrap_or_default());
        let recommended = v
            .get("recommended")
            .filter(|_| tier == Tier::T1 && source == Source::Decision)
            .map(|r| Alternative {
                id: text(r, "id"),
                label: text(r, "label"),
            })
            .filter(|a| valid_alt_id(&a.id));
        let one_tap = v.get("one_tap") == Some(&Value::Bool(true)) && recommended.is_some();
        Some(Self {
            outcome,
            goal: text(v, "goal"),
            source,
            decision_id,
            kind: text(v, "kind"),
            state: text(v, "state"),
            tier,
            revision: v.get("revision").and_then(Value::as_i64).unwrap_or(0),
            version: v.get("version").and_then(Value::as_u64).unwrap_or(0),
            hash,
            created_ms: ts_ms(&text(v, "created_at")),
            expires_ms: ts_ms(&text(v, "expires_at")),
            recommended,
            one_tap,
            blocks: v
                .get("blocks")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .take(MAX_ROWS)
                .map(|b| text(b, "key"))
                .filter(|k| !k.is_empty())
                .collect(),
            waiting_on_it: v.get("waiting_on_it").and_then(Value::as_u64).unwrap_or(0),
        })
    }

    /// "blocks w3 · 2 waiting on it".
    pub fn blocks_line(&self) -> Option<String> {
        if self.blocks.is_empty() {
            return None;
        }
        let mut keys = self.blocks[..self.blocks.len().min(MAX_KEYS)].join(", ");
        if self.blocks.len() > MAX_KEYS {
            keys.push_str(&format!(" +{}", self.blocks.len() - MAX_KEYS));
        }
        let mut s = format!("blocks {keys}");
        if self.waiting_on_it > 0 {
            s.push_str(&format!(" \u{b7} {} waiting on it", self.waiting_on_it));
        }
        Some(s)
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Queue {
    pub rows: Vec<QueueRow>,
}

impl Queue {
    pub fn from_json(v: &Value) -> Result<Self, String> {
        let rows = v
            .get("rows")
            .and_then(Value::as_array)
            .ok_or("the queue has no rows")?;
        rows.iter()
            .take(MAX_ROWS)
            .map(|r| QueueRow::from_json(r).ok_or_else(|| "a queue row cannot be read".to_string()))
            .collect::<Result<_, _>>()
            .map(|rows| Self { rows })
    }
}

/// One record of `approvals.history`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Record {
    pub event_id: u64,
    pub ts: String,
    pub kind: String,
    pub decision_id: String,
    pub choice: String,
    /// `user:<surface>`, or `policy:<hash>`.
    pub by: String,
    /// Empty for an answer recorded before the audit field existed.
    pub mechanism: String,
    pub tier: String,
}

impl Record {
    pub fn from_json(v: &Value) -> Option<Self> {
        let p = v.get("payload")?;
        let audit = &p["audit"];
        let by = ["approver", "by", "decided_by"]
            .iter()
            .map(|k| text(p, k))
            .find(|s| !s.is_empty())
            .unwrap_or_default();
        Some(Self {
            event_id: v.get("event_id")?.as_u64()?,
            ts: text(v, "ts"),
            kind: text(v, "type"),
            decision_id: text(p, "decision_id"),
            choice: text(p, "choice"),
            by,
            mechanism: text(audit, "mechanism"),
            tier: text(audit, "tier"),
        })
    }

    /// The record as words: what was chosen, how, at which tier, by whom.
    pub fn describe(&self) -> String {
        let what = match self.kind.as_str() {
            "decision.resolved" => "answered",
            "approval.reverted" => "undone",
            "approval.auto_decided" => "decided by your setting",
            other => other,
        };
        [
            what,
            &self.choice,
            &self.mechanism,
            &self.tier,
            &self.by,
            &self.decision_id,
        ]
        .iter()
        .filter(|s| !s.is_empty())
        .copied()
        .collect::<Vec<_>>()
        .join(" \u{b7} ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn row(tier: &str) -> Value {
        json!({"outcome": "o1", "goal": "g\u{1b}[31m", "source": "decision", "decision_id": "d1",
            "kind": "blocked", "state": "pending", "tier": tier, "revision": 2,
            "brief_sha256": "a".repeat(64), "created_at": "2026-11-26T09:00:00.000Z",
            "expires_at": "2026-11-27T09:00:00.000Z",
            "recommended": {"id": "retry", "label": "x".repeat(900)}, "one_tap": true,
            "blocks": [{"id": "i", "key": "w3"}], "waiting_on_it": 2})
    }

    #[test]
    fn rows_are_cleaned_capped_and_keep_the_server_order() {
        let q = Queue::from_json(&json!({"rows": [row("T1"), row("T3"), row("T2")]})).unwrap();
        let tiers: Vec<_> = q.rows.iter().map(|r| r.tier).collect();
        assert_eq!(tiers, [Tier::T1, Tier::T3, Tier::T2]);
        let r = &q.rows[0];
        assert!(!r.goal.contains('\u{1b}'));
        assert_eq!(r.recommended.as_ref().unwrap().label.len(), MAX_TEXT);
        assert!(r.one_tap);
        assert_eq!(r.blocks_line().unwrap(), "blocks w3 \u{b7} 2 waiting on it");
        assert_eq!(r.expires_ms.unwrap() - r.created_ms.unwrap(), 86_400_000);
    }

    #[test]
    fn a_recommendation_above_t1_gets_no_button_and_an_unknown_tier_is_t3() {
        for tier in ["T0", "T2", "T3", "T9", ""] {
            let q = Queue::from_json(&json!({"rows": [row(tier)]})).unwrap();
            assert_eq!(q.rows[0].recommended, None, "{tier}");
            assert!(!q.rows[0].one_tap, "{tier}");
        }
        let q = Queue::from_json(&json!({"rows": [row("safe")]})).unwrap();
        assert_eq!(q.rows[0].tier, Tier::T3);
        let mut bad = row("T1");
        bad["decision_id"] = json!("d1; rm -rf /");
        assert!(Queue::from_json(&json!({"rows": [row("T1"), bad]})).is_err());
    }
}
