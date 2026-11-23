//! The chat's pure state: why it is unavailable, and the guard that keeps a card from being confirmed by a key
//! typed ahead or a click that lands the moment the card appears (approvals design principle, T12).

use std::time::{Duration, Instant};

use synthesis_config::Env;
use synthesis_view::conductor::ProposalCard;

use crate::outcomes_view::link::gate as orchestrator_gate;
use crate::outcomes_view::model::Screen;

/// A card ignores Confirm for this long after it first showed these bytes.
pub const ARM: Duration = Duration::from_millis(400);

/// Why the chat cannot start, or `Ready`.
#[derive(Debug, Clone, PartialEq)]
pub enum Gate {
    Ready,
    /// `synt.orchestrator.enabled` is false: the chat explains how to turn it on and does nothing else.
    Off,
    Unavailable(String),
}

pub fn gate(env: &Env) -> Gate {
    match orchestrator_gate(env) {
        Err(Screen::Off) => return Gate::Off,
        Err(Screen::Windows) => {
            return Gate::Unavailable(
                "The Super-Agent chat is not available on native Windows. Use WSL2.".into(),
            );
        }
        Err(Screen::Unreachable(e)) => return Gate::Unavailable(e),
        Err(_) => return Gate::Unavailable("the orchestrator is not ready".into()),
        Ok(home) => {
            if crate::wsl::enabled(&home).unwrap_or(true) {
                return Gate::Unavailable(
                    "The Super-Agent chat does not run in WSL mode yet.".into(),
                );
            }
        }
    }
    Gate::Ready
}

/// What the person reads when the engine did not start: the CLI's own wording ("pass --model", "set
/// SYNT_MODEL") names flags this window does not have, so the two config errors say what to do here.
pub fn explain(e: &str) -> String {
    if e.starts_with("no model configured") || e.starts_with("no model for provider") {
        "No model for Super-Agent: set synt.conductor.model (provider/model) or a default model, add its API key, then Send again.".into()
    } else if e.starts_with("no API key") {
        format!("{e}. Add the key in Settings, then Send again.")
    } else {
        e.to_string()
    }
}

/// One card as the person was shown it: the snapshot, the full hash a click is bound to, and when those bytes
/// first appeared. A new hash is a new card, so the clock starts again.
pub struct Shown {
    pub card: ProposalCard,
    pub sha256: String,
    pub since: Instant,
    /// Index into the transcript after which the card is drawn (the items there when it first appeared).
    pub anchor: usize,
    /// Why the last click did not go on (a limit, an edit that did not validate, stale).
    pub note: Option<String>,
}

impl Shown {
    /// True when a Confirm activation (mouse or key) may count. Not before `ARM` has passed.
    pub fn armed(&self, now: Instant) -> bool {
        now.saturating_duration_since(self.since) >= ARM
    }
}

/// Reads the host's proposals into cards. A proposal the card model refuses is not drawn at all.
pub fn snapshot(
    proposals: &[serde_json::Value],
    old: &[Shown],
    items: usize,
    now: Instant,
) -> Vec<Shown> {
    proposals
        .iter()
        .filter_map(|p| {
            let card = ProposalCard::from_json(p).ok()?;
            let sha256 = p.get("payload_sha256")?.as_str()?.to_string();
            let before = old.iter().find(|o| o.card.id == card.id);
            let same = before.filter(|o| o.sha256 == sha256);
            Some(Shown {
                since: same.map_or(now, |o| o.since),
                anchor: before.map_or(items, |o| o.anchor),
                note: same.and_then(|o| o.note.clone()),
                card,
                sha256,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn p(sha: &str) -> serde_json::Value {
        json!({"id": "p1", "state": "shown", "payload_sha256": sha,
               "payload": {"kind": "close", "session": "s1", "sid": "x"}})
    }

    #[test]
    fn a_card_is_armed_only_after_the_guard_time_and_a_new_hash_restarts_it() {
        let t0 = Instant::now();
        let a = snapshot(&[p("aa")], &[], 3, t0);
        assert_eq!(a[0].anchor, 3);
        assert!(!a[0].armed(t0 + Duration::from_millis(399)));
        assert!(a[0].armed(t0 + ARM));
        let t1 = t0 + Duration::from_secs(5);
        let same = snapshot(&[p("aa")], &a, 9, t1);
        assert_eq!((same[0].since, same[0].anchor), (t0, 3));
        let changed = snapshot(&[p("bb")], &a, 9, t1);
        assert_eq!(changed[0].since, t1);
        assert!(!changed[0].armed(t1));
    }

    #[test]
    fn start_errors_say_what_to_do_in_this_window() {
        assert!(explain("no model configured: pass --model x").contains("synt.conductor.model"));
        assert!(explain("no API key for provider \"a\": set K and restart").contains("Settings"));
        assert_eq!(explain("boom"), "boom");
    }

    #[test]
    fn a_proposal_the_card_model_refuses_is_not_drawn() {
        let bad = json!({"id": "p2", "state": "shown", "payload_sha256": "aa", "payload": {"kind": "approve_all"}});
        assert!(snapshot(&[bad], &[], 0, Instant::now()).is_empty());
    }
}
