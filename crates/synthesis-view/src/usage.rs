//! Data of the dashboard's Usage panel: per-day activity of every stored session (all folders),
//! read-only from `synt.db`, off the UI thread. There is no provider quota source, so the
//! 5-hour / 7-day rows stay absent.

use std::path::Path;

use serde::{Deserialize, Serialize};
use synthesis_session::{Reader, UsageDay};

/// One calendar day of activity (the render indexes days back from the last entry).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
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

// Calendar helpers of the Usage panel (days since 1970 <-> civil dates, local to a UTC offset).

pub struct Civil {
    pub year: i64,
    pub month: usize,
    pub day: i64,
    pub dow: usize,
}

pub fn civil_from_days(z: i64) -> (i64, usize, i64) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, (m - 1) as usize, d)
}
pub fn days_from_civil(y: i64, m0: usize, d: i64) -> i64 {
    let m = m0 as i64 + 1;
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}
pub fn civil(ms: i64, tz: i32) -> Civil {
    let days = (ms + i64::from(tz) * 60_000).div_euclid(86_400_000);
    let (year, month, day) = civil_from_days(days);
    Civil {
        year,
        month,
        day,
        dow: (days + 4).rem_euclid(7) as usize,
    }
}
pub fn days_in_month(y: i64, m0: usize) -> i64 {
    days_from_civil(if m0 == 11 { y + 1 } else { y }, (m0 + 1) % 12, 1) - days_from_civil(y, m0, 1)
}

pub const MONTHS: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

pub fn heat_level(v: Option<f64>, max: Option<f64>) -> usize {
    let (Some(v), Some(max)) = (v, max) else {
        return 0;
    };
    if v.is_nan() || max.is_nan() || v <= 0.0 || max <= 0.0 || !v.is_finite() || !max.is_finite() {
        return 0;
    }
    4.min(1 + 3.min(((v / max) * (4.0 - 1e-9)).floor() as usize))
}

#[derive(Clone)]
pub struct DayCell<'a> {
    pub day: i64,
    pub month: usize,
    pub future: bool,
    pub entry: Option<&'a HistoryDay>,
}

pub struct MonthGrid<'a> {
    pub weeks: Vec<[Option<DayCell<'a>>; 7]>,
    pub year: i64,
    pub month: usize,
    pub today_ms: i64,
}

pub fn month_grid<'a>(history: &'a [HistoryDay], tz: i32, now: i64) -> MonthGrid<'a> {
    let today_ms = history.last().map_or(now, |l| l.date_ms);
    let t = civil(today_ms, tz);
    let dim = days_in_month(t.year, t.month);
    let mut weeks = vec![];
    let mut week: [Option<DayCell>; 7] = Default::default();
    for day in 1..=dim {
        let days = days_from_civil(t.year, t.month, day);
        let start_ms = days * 86_400_000 - i64::from(tz) * 60_000;
        let dow = (days + 4).rem_euclid(7) as usize;
        let entry = history.iter().find(|d| {
            let c = civil(d.date_ms, tz);
            c.year == t.year && c.month == t.month && c.day == day
        });
        week[dow] = Some(DayCell {
            day,
            month: t.month,
            future: start_ms > today_ms,
            entry,
        });
        if dow == 6 || day == dim {
            weeks.push(std::mem::take(&mut week));
        }
    }
    MonthGrid {
        weeks,
        year: t.year,
        month: t.month,
        today_ms,
    }
}

/// The year heatmap: `cols` weeks (oldest first, the last one holds today), each Sunday to Saturday; `None` is a
/// day before the history or after today. `max` is the busiest day (at least 1) for [`heat_level`].
pub struct YearGrid<'a> {
    pub cols: Vec<[Option<&'a HistoryDay>; 7]>,
    pub max: f64,
}

pub fn year_grid<'a>(
    history: &'a [HistoryDay],
    tz: i32,
    weeks: i64,
    val: &dyn Fn(&HistoryDay) -> f64,
) -> YearGrid<'a> {
    let Some(today) = history.last() else {
        return YearGrid {
            cols: vec![],
            max: 1.0,
        };
    };
    let today_dow = civil(today.date_ms, tz).dow as i64;
    let mut max = 1.0f64;
    let cols = (0..weeks)
        .map(|c| {
            std::array::from_fn(|r| {
                let ago = (weeks - 1 - c) * 7 + (today_dow - r as i64);
                let idx = history.len() as i64 - 1 - ago;
                let d = if ago < 0 || idx < 0 {
                    None
                } else {
                    history.get(idx as usize)
                };
                if let Some(d) = d {
                    max = max.max(val(d));
                }
                d
            })
        })
        .collect();
    YearGrid { cols, max }
}

/// Per week column, the month (0-11) whose 1st falls in it: where the month labels go.
pub fn year_month_starts(grid: &YearGrid, tz: i32) -> Vec<Option<usize>> {
    grid.cols
        .iter()
        .map(|col| {
            col.iter()
                .flatten()
                .map(|d| civil(d.date_ms, tz))
                .filter(|c| c.day == 1)
                .map(|c| c.month)
                .next_back()
        })
        .collect()
}

/// The numbers of the Usage panel's summary rows, for the metric `val` (the CLI and the desktop app draw them).
pub struct MonthSummary {
    pub today: f64,
    pub week: f64,
    pub month: f64,
    pub best: f64,
    /// Day of month and month index (0-11) of the best day.
    pub best_day: Option<(i64, usize)>,
    pub active: usize,
    /// Days of the month up to today.
    pub days: usize,
    pub streak: usize,
}

pub fn month_summary(
    history: &[HistoryDay],
    grid: &MonthGrid,
    val: &dyn Fn(&HistoryDay) -> f64,
) -> MonthSummary {
    let month_days: Vec<&DayCell> = grid
        .weeks
        .iter()
        .flatten()
        .flatten()
        .filter(|d| !d.future)
        .collect();
    let active = month_days
        .iter()
        .filter(|d| d.entry.is_some_and(|e| val(e) > 0.0))
        .count();
    let mut peak_val = -1.0;
    let mut peak_day: Option<&DayCell> = None;
    for d in &month_days {
        let v = d.entry.map_or(0.0, val);
        if v > peak_val {
            peak_val = v;
            peak_day = Some(d);
        }
    }
    let today_entry = history.last();
    let mut streak = 0;
    let mut i = history.len() as i64 - 1 - i64::from(!today_entry.is_some_and(|t| val(t) > 0.0));
    while i >= 0 && val(&history[i as usize]) > 0.0 {
        streak += 1;
        i -= 1;
    }
    let sum = |n: usize| {
        history[history.len().saturating_sub(n)..]
            .iter()
            .map(val)
            .sum::<f64>()
    };
    MonthSummary {
        today: today_entry.map_or(0.0, val),
        week: sum(7),
        month: month_days.iter().map(|d| d.entry.map_or(0.0, val)).sum(),
        best: peak_val.max(0.0),
        best_day: peak_day.map(|d| (d.day, d.month)),
        active,
        days: month_days.len(),
        streak,
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
    fn the_year_grid_ends_with_today_and_marks_month_starts() {
        // 2026-11-10 is a Tuesday (day 20767)
        let h: Vec<HistoryDay> = (20_600..=20_767)
            .map(|n| HistoryDay {
                date_ms: n * MS_DAY,
                tokens: if n == 20_767 { 5.0 } else { 0.0 },
                ..Default::default()
            })
            .collect();
        let g = year_grid(&h, 0, 53, &|d| d.tokens);
        assert_eq!(g.cols.len(), 53);
        let last = g.cols.last().unwrap();
        assert_eq!(
            (last[2].map(|d| d.tokens), last[3].is_none()),
            (Some(5.0), true)
        );
        assert!(g.cols[0].iter().all(Option::is_none), "before the history");
        assert_eq!(g.max, 5.0);
        let starts = year_month_starts(&g, 0);
        assert_eq!(starts.last().unwrap(), &None);
        assert_eq!(
            starts[52 - 1],
            Some(10),
            "Nov 1 is a Sunday, the week before the last"
        );
        assert!(year_grid(&[], 0, 53, &|d| d.tokens).cols.is_empty());
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
