//! Data of the dashboard's Usage panel: per-day activity of every stored session (all folders),
//! read-only from `synt.db`, off the UI thread. There is no provider quota source, so the
//! 5-hour / 7-day rows stay absent.

use std::path::Path;

use serde::Serialize;
use synthesis_session::{Reader, UsageDay};

/// One calendar day of activity (the render indexes days back from the last entry).
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct HistoryDay {
    /// Instant (ms) inside the day; the calendar day is taken in the state's UTC offset.
    pub date_ms: i64,
    pub tokens: f64,
    pub output: f64,
    pub messages: f64,
    pub tools: f64,
    pub agents: f64,
    pub sessions: f64,
    pub commands: f64,
    pub files: f64,
    pub browser: f64,
    pub mcp: f64,
}

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
                commands: f(|d| d.commands),
                files: f(|d| d.files),
                browser: f(|d| d.browser),
                mcp: f(|d| d.mcp),
            }
        })
        .collect()
}

/// An unreadable or missing store is a history of zeros.
pub fn load(home: &Path, now_ms: i64, tz: i32) -> Vec<HistoryDay> {
    match Reader::open(home) {
        Ok(r) => load_from(&r, now_ms, tz),
        Err(_) => fill(&[], today_of(now_ms, tz), tz),
    }
}

fn today_of(now_ms: i64, tz: i32) -> i64 {
    (now_ms + i64::from(tz) * 60_000).div_euclid(MS_DAY)
}

/// [`load`] over a reader the caller already holds.
pub fn load_from(r: &Reader, now_ms: i64, tz: i32) -> Vec<HistoryDay> {
    let today = today_of(now_ms, tz);
    let since = now_ms - WINDOW_DAYS * MS_DAY;
    r.usage_by_day(since, tz)
        .map(|d| fill(&d, today, tz))
        .unwrap_or_else(|_| fill(&[], today, tz))
}

/// Sums of [`HistoryDay`] fields over a range of days.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Metrics {
    pub tokens: f64,
    pub tool_calls: f64,
    pub files: f64,
    pub commands: f64,
    pub browser: f64,
    pub mcp: f64,
}

/// The Usage panel's data: [`HistoryDay`]s, oldest first, ending today.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct UsageHistory {
    pub days: Vec<HistoryDay>,
}

impl UsageHistory {
    pub fn load(home: &Path, now_ms: i64, tz: i32) -> Self {
        Self {
            days: load(home, now_ms, tz),
        }
    }

    /// The Usage & Metrics rows over the last `n` days (today included).
    pub fn metrics(&self, n: usize) -> Metrics {
        let mut m = Metrics::default();
        for d in self.days.iter().rev().take(n) {
            m.tokens += d.tokens;
            m.tool_calls += d.tools;
            m.files += d.files;
            m.commands += d.commands;
            m.browser += d.browser;
            m.mcp += d.mcp;
        }
        m
    }

    /// Tokens of the last `n` days (today included).
    pub fn tokens_last(&self, n: usize) -> f64 {
        self.days.iter().rev().take(n).map(|d| d.tokens).sum()
    }
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
            commands: 6,
            files: 7,
            browser: 8,
            mcp: 9,
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
    fn history_sums_the_last_days() {
        let h = UsageHistory {
            days: fill(&[day(101, 7), day(102, 9)], 103, 0),
        };
        assert_eq!(
            (h.tokens_last(1), h.tokens_last(2), h.tokens_last(3)),
            (0.0, 9.0, 16.0)
        );
    }

    #[test]
    fn metrics_sum_the_last_days() {
        let mut days = fill(&[], 103, 0);
        let n = days.len();
        days[n - 1].commands = 2.0;
        days[n - 1].mcp = 1.0;
        days[n - 8].commands = 5.0; // 8 days back: outside 7, inside 30
        let h = UsageHistory { days };
        assert_eq!((h.metrics(7).commands, h.metrics(30).commands), (2.0, 7.0));
        assert_eq!(h.metrics(7).mcp, 1.0);
    }

    #[test]
    fn a_missing_store_is_a_year_of_zeros() {
        let t = std::env::temp_dir().join("synt-usage-no-store-here");
        let h = load(&t, 1_800_000_000_000, 0);
        assert_eq!((h.len(), h[0].tokens), (365, 0.0));
    }
}
