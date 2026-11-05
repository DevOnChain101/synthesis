//! Data of the dashboard's Usage panel: per-day activity of every stored session (all folders),
//! read-only from `synt.db`, off the UI thread. There is no provider quota source, so the
//! 5-hour / 7-day rows stay absent.

use std::path::Path;

use synthesis_session::{Reader, UsageDay};
use synthesis_tui_render::dash::HistoryDay;

const MS_DAY: i64 = 86_400_000;
/// 53 weeks: the year grid plus the partial week.
const WINDOW_DAYS: i64 = 371;

/// Contiguous days from the first active one (at most a year back) to today (the render indexes
/// days back from the last entry); the last year at least, so the grid is drawn with no history.
fn fill(days: &[UsageDay], today: i64, tz: i32) -> Vec<HistoryDay> {
    let first = days.first().map_or(today, |d| d.day).min(today - 364);
    (first.min(today)..=today)
        .map(|day| {
            let d = days.iter().find(|d| d.day == day);
            let f = |g: fn(&UsageDay) -> u64| d.map_or(0.0, |d| g(d) as f64);
            HistoryDay {
                date_ms: day * MS_DAY - i64::from(tz) * 60_000,
                tokens: f(|d| d.tokens),
                output: f(|d| d.output),
                messages: f(|d| d.messages),
                tools: f(|d| d.tools),
                agents: f(|d| d.agents),
                sessions: f(|d| d.sessions),
            }
        })
        .collect()
}

/// An unreadable or missing store is a history of zeros.
pub(crate) fn load(home: &Path, now_ms: i64, tz: i32) -> Vec<HistoryDay> {
    let today = (now_ms + i64::from(tz) * 60_000).div_euclid(MS_DAY);
    let since = now_ms - WINDOW_DAYS * MS_DAY;
    Reader::open(home)
        .and_then(|r| r.usage_by_day(since, tz))
        .map(|d| fill(&d, today, tz))
        .unwrap_or_else(|_| fill(&[], today, tz))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn day(day: i64, tokens: u64) -> UsageDay {
        UsageDay {
            day,
            tokens,
            output: 1,
            messages: 2,
            tools: 3,
            agents: 4,
            sessions: 5,
        }
    }

    #[test]
    fn fills_gaps_and_ends_today_at_local_midnight() {
        let h = fill(&[day(100, 7), day(102, 9)], 103, 120);
        assert_eq!(
            h.iter().rev().take(4).map(|d| d.tokens).collect::<Vec<_>>(),
            [0.0, 9.0, 0.0, 7.0]
        );
        assert_eq!(
            (h.len(), h[364].date_ms),
            (365, 103 * MS_DAY - 120 * 60_000)
        );
        assert_eq!((h[363].sessions, h[362].sessions), (5.0, 0.0));
        let z = fill(&[], 103, 0);
        assert_eq!(
            (z.len(), z[0].tokens, z[364].date_ms),
            (365, 0.0, 103 * MS_DAY)
        );
    }

    #[test]
    fn a_missing_store_is_a_year_of_zeros() {
        let t = std::env::temp_dir().join("synt-usage-no-store-here");
        let h = load(&t, 1_800_000_000_000, 0);
        assert_eq!((h.len(), h[0].tokens), (365, 0.0));
    }
}
