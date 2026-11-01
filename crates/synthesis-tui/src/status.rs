//! The honest status line under the frame: sandbox mode and what it really guarantees, budgets
//! against their limits, and the verification verdict with its evidence. Nothing here is
//! decoration: a missing fact is shown as missing ("unknown", "unverified"), never as OK.

use synthesis_events::Budget;
use synthesis_tui_render::theme::{GREEN, RED, Rgb, T_INACTIVE, YELLOW, fit_items, paint};
use synthesis_tui_render::util::{fmt_n, short, to_fixed};

use crate::chat::{Totals, Verification};

/// What the host built for command confinement. Strings, so this crate does not depend on the
/// sandbox backends: the CLI fills it from `SandboxMode` and `SandboxBackend::capabilities()`.
#[derive(Clone, Debug, Default)]
pub struct SandboxStatus {
    /// `enforce`, `best-effort`, `off`; empty = the host did not say.
    pub mode: String,
    pub backend: String,
    /// Guarantee levels as words (`strong`, `weak`, `none`).
    pub filesystem: String,
    pub network: String,
    /// Commands run (or may run) with the agent's full privileges because the user asked.
    pub unsandboxed_by_request: bool,
}

impl SandboxStatus {
    pub(crate) fn text(&self) -> (String, bool) {
        if self.mode.is_empty() {
            return ("sandbox: unknown".into(), true);
        }
        let mut s = format!("sandbox: {}", self.mode);
        let caps: Vec<String> = [
            (!self.backend.is_empty()).then(|| self.backend.clone()),
            (!self.filesystem.is_empty()).then(|| format!("fs:{}", self.filesystem)),
            (!self.network.is_empty()).then(|| format!("net:{}", self.network)),
        ]
        .into_iter()
        .flatten()
        .collect();
        if !caps.is_empty() {
            s.push_str(&format!(" [{}]", caps.join(" ")));
        }
        if self.unsandboxed_by_request {
            s.push_str(" UNSANDBOXED_BY_REQUEST");
        }
        let bad = self.unsandboxed_by_request || self.mode == "off";
        (s, bad)
    }
}

/// How long the "↓ compacted A→B" note stays after a compaction.
const COMPACTED_NOTE_MS: i64 = 8000;

/// Context-window usage for the status line. Fed by `context.usage` / `context.compacting` /
/// `context.compacted` / `context.compaction_failed` (see `Chat::apply`); knows nothing of the
/// event types, so it is testable without them.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ContextStatus {
    used: u64,
    window: u64,
    percent: Option<f64>,
    known: bool,
    stale: bool,
    compacting: bool,
    note: Option<(String, i64)>,
}

/// `124000` -> `124k`, `1_000_000` -> `1M`.
fn tok(n: u64) -> String {
    if n >= 1_000_000 {
        let m = format!("{:.1}", n as f64 / 1e6);
        format!("{}M", m.trim_end_matches(".0"))
    } else if n >= 1000 {
        format!("{}k", (n + 500) / 1000)
    } else {
        n.to_string()
    }
}

impl ContextStatus {
    /// `percent` 0..=100 (may exceed 100); `None` computes it from the two sizes.
    pub fn on_usage(&mut self, used: u64, window: u64, percent: Option<f64>, stale: bool) {
        self.used = used;
        self.window = window;
        self.percent = percent;
        self.known = true;
        self.stale = stale;
        self.compacting = false;
    }

    pub fn on_compacting(&mut self) {
        self.compacting = true;
    }

    /// The numbers after compaction replace the old ones; `after == 0` means unknown.
    pub fn on_compacted(&mut self, before: u64, after: u64, window: u64, now: i64) {
        self.compacting = false;
        if window > 0 {
            self.window = window;
        }
        if after > 0 {
            self.used = after;
            self.percent = None;
            self.known = true;
            self.stale = false;
        } else {
            self.known = false;
        }
        let to = if after > 0 { tok(after) } else { "?".into() };
        self.note = Some((
            format!("↓ compacted {}→{to}", tok(before)),
            now + COMPACTED_NOTE_MS,
        ));
    }

    pub fn on_compaction_failed(&mut self) {
        self.compacting = false;
    }

    /// `(used, window)` tokens; `(0, 0)` while nothing is known (new or cleared conversation).
    pub fn numbers(&self) -> (u64, u64) {
        if self.known {
            (self.used, self.window)
        } else {
            (0, 0)
        }
    }

    /// `ctx 62% 124k/200k`, `⟳ compacting…`, plus the brief compaction note; `None` before any
    /// number arrived.
    pub fn text(&self, now: i64) -> Option<(String, Rgb)> {
        let note = self
            .note
            .as_ref()
            .filter(|n| now < n.1)
            .map(|n| n.0.as_str());
        if self.compacting {
            return Some(("⟳ compacting…".into(), YELLOW));
        }
        if !self.known {
            return note.map(|n| (n.to_string(), T_INACTIVE));
        }
        let pct = self.percent.unwrap_or_else(|| {
            if self.window > 0 {
                self.used as f64 * 100.0 / self.window as f64
            } else {
                0.0
            }
        });
        let mut s = if self.window > 0 {
            format!(
                "ctx {}% {}/{}",
                pct.round() as i64,
                tok(self.used),
                tok(self.window)
            )
        } else {
            format!("ctx {}", tok(self.used))
        };
        if self.stale {
            s.push('~'); // estimate older than the last history change
        }
        if let Some(n) = note {
            s = format!("{s} {n}");
        }
        let colour = match pct {
            p if p >= 90.0 => RED,
            p if p >= 75.0 => YELLOW,
            _ => T_INACTIVE,
        };
        Some((s, colour))
    }
}

/// What the status line shows beyond sandbox, budgets and verification.
#[derive(Clone, Copy, Default)]
pub struct StatusExtra<'a> {
    pub context: Option<&'a ContextStatus>,
    pub now: i64,
    /// ctrl+o state: tasks / bash / code blocks shown whole.
    pub expanded: bool,
}

pub fn status_line(
    sb: &SandboxStatus,
    budget: &Budget,
    t: &Totals,
    verification: Option<&Verification>,
    ran_a_task: bool,
    wall_ms: u64,
    width: usize,
) -> String {
    status_line_with(
        sb,
        budget,
        t,
        verification,
        ran_a_task,
        wall_ms,
        width,
        &StatusExtra::default(),
    )
}

#[allow(clippy::too_many_arguments)]
pub fn status_line_with(
    sb: &SandboxStatus,
    budget: &Budget,
    t: &Totals,
    verification: Option<&Verification>,
    ran_a_task: bool,
    wall_ms: u64,
    width: usize,
    extra: &StatusExtra,
) -> String {
    let (sandbox, bad) = sb.text();
    let sandbox = paint(if bad { RED } else { GREEN }, &sandbox);

    let mut budgets = vec![
        format!("turns {}/{}", t.turns, budget.max_turns),
        format!("tools {}/{}", t.tool_calls, budget.max_tool_calls),
        format!("tok {}↑ {}↓", fmt_n(t.input_tokens), fmt_n(t.output_tokens)),
    ];
    match (t.cost_usd, budget.max_cost_usd) {
        (Some(c), Some(m)) => budgets.push(format!("cost ${}/${}", to_fixed(c, 4), to_fixed(m, 2))),
        (Some(c), None) => budgets.push(format!("cost ${}", to_fixed(c, 4))),
        (None, Some(m)) => budgets.push(format!(
            "cost unknown (limit ${} not enforced)",
            to_fixed(m, 2)
        )),
        (None, None) => {}
    }
    if let Some(ms) = budget.max_wall_ms {
        budgets.push(format!("time {}s/{}s", wall_ms / 1000, ms / 1000));
    }

    let verify = match verification {
        Some(v) => {
            let colour = match v.status.as_str() {
                "passed" => GREEN,
                "failed" => RED,
                _ => YELLOW,
            };
            let ev = if v.summary.is_empty() {
                String::new()
            } else {
                format!(": {}", short(&v.summary.replace('\n', " "), 60))
            };
            paint(colour, &format!("verify {}{ev}", v.status))
        }
        None if ran_a_task => paint(YELLOW, "verify unverified: no verification result"),
        None => paint(T_INACTIVE, "verify n/a"),
    };
    let dot = paint(T_INACTIVE, "·");
    let ctx = extra
        .context
        .and_then(|c| c.text(extra.now))
        .map(|(s, c)| paint(c, &s))
        .unwrap_or_default();
    let view = paint(
        T_INACTIVE,
        if extra.expanded {
            "ctrl+o: collapse"
        } else {
            "ctrl+o: expand"
        },
    );
    // Dropped whole, last priority first: key hint, budgets, context, then verify; sandbox stays.
    let items = [
        (1, format!(" {sandbox}")),
        (4, paint(T_INACTIVE, &budgets.join(" · "))),
        (3, ctx),
        (2, verify),
        (5, view),
    ];
    let items: Vec<(u8, String)> = items.into_iter().filter(|i| !i.1.is_empty()).collect();
    fit_items(&items, &format!(" {dot} "), width)
}

#[cfg(test)]
mod tests {
    use super::*;
    use synthesis_tui_base::text::strip_ansi;

    fn line(sb: &SandboxStatus, v: Option<&Verification>, task: bool, t: &Totals) -> String {
        strip_ansi(&status_line(sb, &Budget::default(), t, v, task, 0, 200))
    }

    #[test]
    fn states_are_never_flattering() {
        let t = Totals::default();
        let l = line(&SandboxStatus::default(), None, true, &t);
        assert!(l.contains("sandbox: unknown"), "{l}");
        assert!(
            l.contains("verify unverified: no verification result"),
            "{l}"
        );
        let sb = SandboxStatus {
            mode: "best-effort".into(),
            backend: "seatbelt".into(),
            filesystem: "strong".into(),
            network: "none".into(),
            unsandboxed_by_request: true,
        };
        let v = Verification {
            status: "failed".into(),
            summary: "cargo test: 2 failed".into(),
        };
        let l = line(&sb, Some(&v), true, &t);
        assert!(
            l.contains("[seatbelt fs:strong net:none] UNSANDBOXED_BY_REQUEST"),
            "{l}"
        );
        assert!(l.contains("verify failed: cargo test: 2 failed"), "{l}");
        assert!(l.contains("turns 0/50 · tools 0/200"), "{l}");
        assert!(line(&SandboxStatus::default(), None, false, &t).contains("verify n/a"));
    }

    fn ctx_text(c: &ContextStatus, now: i64) -> String {
        c.text(now).map(|(s, _)| strip_ansi(&s)).unwrap_or_default()
    }

    #[test]
    fn context_status_usage_compacting_compacted() {
        let mut c = ContextStatus::default();
        assert_eq!(c.text(0), None, "nothing known, nothing shown");
        c.on_usage(124_000, 200_000, Some(62.0), false);
        assert_eq!(ctx_text(&c, 0), "ctx 62% 124k/200k");
        c.on_usage(150_000, 200_000, None, true); // percent computed; stale is marked
        assert_eq!(ctx_text(&c, 0), "ctx 75% 150k/200k~");
        c.on_usage(900, 0, None, false);
        assert_eq!(ctx_text(&c, 0), "ctx 900", "unknown window");
        c.on_usage(1_000_000, 1_000_000, Some(100.4), false);
        assert_eq!(ctx_text(&c, 0), "ctx 100% 1M/1M");

        c.on_usage(168_000, 200_000, Some(84.0), false);
        c.on_compacting();
        assert_eq!(ctx_text(&c, 0), "⟳ compacting…");
        c.on_compacted(168_000, 41_000, 200_000, 1000);
        // the NEW numbers, plus a brief note that goes away
        assert_eq!(ctx_text(&c, 1001), "ctx 21% 41k/200k ↓ compacted 168k→41k");
        assert_eq!(ctx_text(&c, 1000 + COMPACTED_NOTE_MS), "ctx 21% 41k/200k");
        // a failed compaction stops showing "compacting"
        c.on_compacting();
        c.on_compaction_failed();
        assert_eq!(ctx_text(&c, 1000 + COMPACTED_NOTE_MS), "ctx 21% 41k/200k");
        // unknown after-size: the old number is not kept as if it were current
        c.on_compacted(168_000, 0, 200_000, 5000);
        assert_eq!(ctx_text(&c, 5001), "↓ compacted 168k→?");
    }

    #[test]
    fn context_colours_warn_before_the_window_is_full() {
        let mut c = ContextStatus::default();
        let col = |c: &ContextStatus| c.text(0).unwrap().1;
        c.on_usage(50, 100, Some(50.0), false);
        assert_eq!(col(&c), T_INACTIVE);
        c.on_usage(80, 100, Some(80.0), false);
        assert_eq!(col(&c), YELLOW);
        c.on_usage(95, 100, Some(95.0), false);
        assert_eq!(col(&c), RED);
    }

    #[test]
    fn cost_limit_without_pricing_says_so() {
        let b = Budget {
            max_cost_usd: Some(1.0),
            ..Budget::default()
        };
        let l = strip_ansi(&status_line(
            &SandboxStatus::default(),
            &b,
            &Totals::default(),
            None,
            false,
            0,
            300,
        ));
        assert!(l.contains("cost unknown (limit $1.00 not enforced)"), "{l}");
    }
}
