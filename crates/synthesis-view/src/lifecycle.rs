//! How long a finished sub-agent and a finished task list stay on screen. Pure functions of the
//! stored times and a `now` the caller passes in (epoch ms), so the transitions are testable
//! without a clock. Owner decision 2026-11-10.

use crate::agent::SubAgent;

/// Done -> idle after one minute, removed (with its transcript) after five.
pub const IDLE_AFTER_MS: i64 = 60_000;
pub const REMOVE_AFTER_MS: i64 = 5 * 60_000;

/// One row of the roster under the prompt.
#[derive(Debug, Clone, PartialEq)]
pub struct RosterRow {
    pub id: String,
    pub name: String,
    /// `running` (or the hub's status while it works), `done`, then `idle`.
    pub state: String,
    /// Time since it started; frozen when it finished.
    pub online_ms: i64,
    pub tokens: u64,
}

/// The rows to show at `now`: still-working agents, finished ones for [`REMOVE_AFTER_MS`].
pub fn roster(subs: &[SubAgent], now: i64) -> Vec<RosterRow> {
    subs.iter()
        .filter_map(|s| {
            // a cancelled or evicted agent without a stop event counts as finished when it started
            let end = if s.live() {
                None
            } else {
                s.ended.or(s.started)
            };
            let age = end.map_or(0, |e| now - e);
            if age >= REMOVE_AFTER_MS {
                return None;
            }
            let state = match end {
                None => s.status.clone(),
                Some(_) if age >= IDLE_AFTER_MS => "idle".into(),
                Some(_) => "done".into(),
            };
            Some(RosterRow {
                id: s.id.clone(),
                name: s.name.clone(),
                state,
                online_ms: (end.unwrap_or(now) - s.started.unwrap_or(now)).max(0),
                tokens: s.tokens,
            })
        })
        .collect()
}

/// `42s`, `5m`, `1h05m`.
pub fn online(ms: i64) -> String {
    let s = ms.max(0) / 1000;
    match s {
        0..=59 => format!("{s}s"),
        60..=3599 => format!("{}m", s / 60),
        _ => format!("{}h{:02}m", s / 3600, s % 3600 / 60),
    }
}

/// The task list (Mission panel) is shown while it has tasks and has not been all done for
/// [`REMOVE_AFTER_MS`]. A new list replaces the old one at once (the reducer); a resumed
/// session starts without one (`Convo::clear_plan`).
pub fn mission_visible(tasks: usize, all_done_at: Option<i64>, now: i64) -> bool {
    tasks > 0 && all_done_at.is_none_or(|t| now - t < REMOVE_AFTER_MS)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sub(status: &str, started: i64, ended: Option<i64>) -> SubAgent {
        SubAgent {
            id: "a1".into(),
            name: "developer".into(),
            kind: "developer".into(),
            tool_call_id: None,
            background: false,
            status: status.into(),
            model: String::new(),
            started: Some(started),
            ended,
            reason: String::new(),
            tool_calls: 0,
            tokens: 900,
            entries: vec![],
        }
    }

    #[test]
    fn running_then_done_then_idle_at_1m_then_removed_at_5m() {
        let t0 = 1_000_000;
        let running = [sub("running", t0, None)];
        let r = &roster(&running, t0 + 90_000)[0];
        assert_eq!(
            (r.state.as_str(), r.online_ms, r.tokens),
            ("running", 90_000, 900)
        );

        let end = t0 + 120_000;
        let done = [sub("stopped", t0, Some(end))];
        let at = |d: i64| roster(&done, end + d);
        assert_eq!(at(0)[0].state, "done");
        assert_eq!(at(IDLE_AFTER_MS - 1)[0].state, "done");
        assert_eq!(at(IDLE_AFTER_MS)[0].state, "idle");
        assert_eq!(at(REMOVE_AFTER_MS - 1)[0].state, "idle");
        assert!(at(REMOVE_AFTER_MS).is_empty());
        // online time stops at the end
        assert_eq!(at(200_000)[0].online_ms, 120_000);
    }

    #[test]
    fn online_formats() {
        assert_eq!(online(42_000), "42s");
        assert_eq!(online(300_000), "5m");
        assert_eq!(online(3_900_000), "1h05m");
    }

    #[test]
    fn mission_hides_five_minutes_after_all_done() {
        assert!(!mission_visible(0, None, 0));
        assert!(mission_visible(3, None, 10_000_000));
        assert!(mission_visible(3, Some(0), REMOVE_AFTER_MS - 1));
        assert!(!mission_visible(3, Some(0), REMOVE_AFTER_MS));
    }
}
