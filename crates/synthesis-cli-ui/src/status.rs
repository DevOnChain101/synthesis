//! The honest status line under the frame: sandbox mode and what it really guarantees, budgets
//! against their limits, and the verification verdict with its evidence. Nothing here is
//! decoration: a missing fact is shown as missing ("unknown", "unverified"), never as OK.

use synthesis_cli_ui_render::theme::{GREEN, RED, Rgb, T_INACTIVE, YELLOW, fit_items, paint};
use synthesis_cli_ui_render::util::{fmt_n, short, to_fixed};
use synthesis_events::Budget;

use crate::chat::{Totals, Verification};

#[cfg(test)]
use synthesis_view::status::COMPACTED_NOTE_MS;
pub use synthesis_view::status::ContextStatus;
use synthesis_view::status::Level;
pub use synthesis_view::status::SandboxStatus;

/// The context label with the colour of its level.
pub(crate) trait ContextText {
    fn text(&self, now: i64) -> Option<(String, Rgb)>;
}

impl ContextText for ContextStatus {
    fn text(&self, now: i64) -> Option<(String, Rgb)> {
        self.label(now).map(|(s, l)| {
            let c = match l {
                Level::Normal => T_INACTIVE,
                Level::Warn => YELLOW,
                Level::Critical => RED,
            };
            (s, c)
        })
    }
}

/// What the status line shows beyond sandbox, budgets and verification.
#[derive(Clone, Copy, Default)]
pub struct StatusExtra<'a> {
    pub context: Option<&'a ContextStatus>,
    pub now: i64,
    /// ctrl+o state: tasks / bash / code blocks shown whole.
    pub expanded: bool,
    /// Cross-session messaging is on: the requests in flight (`peers on · N in flight`).
    pub peers: Option<usize>,
    /// The note files (`#` notes, `/memory`) are available.
    pub memory: Option<bool>,
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
    let peers = extra.peers.map_or(String::new(), |n| {
        paint(
            T_INACTIVE,
            &if n > 0 {
                format!("peers on · {n} in flight")
            } else {
                "peers on".to_string()
            },
        )
    });
    let memory = match extra.memory {
        Some(true) => paint(T_INACTIVE, "memory on"),
        _ => String::new(),
    };
    let items = [
        (1, format!(" {sandbox}")),
        (4, paint(T_INACTIVE, &budgets.join(" · "))),
        (3, ctx),
        (2, verify),
        (5, view),
        (3, peers),
        (6, memory),
    ];
    let items: Vec<(u8, String)> = items.into_iter().filter(|i| !i.1.is_empty()).collect();
    fit_items(&items, &format!(" {dot} "), width)
}

#[cfg(test)]
mod tests {
    use super::*;
    use synthesis_cli_ui_base::text::strip_ansi;

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
        assert_eq!(ctx_text(&c, 0), "ctx 900/?", "unknown window");
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
    fn a_new_window_replaces_the_old_one_at_once() {
        // /model restarts the app on another model: the first number of the new run sets its window
        let mut c = ContextStatus::default();
        c.on_usage(150_000, 200_000, Some(75.0), false);
        assert_eq!(ctx_text(&c, 0), "ctx 75% 150k/200k");
        let mut d = ContextStatus::default();
        assert_eq!(d.numbers(), (0, 0), "nothing carried over");
        d.on_usage(150_000, 1_000_000, Some(15.0), false);
        assert_eq!(ctx_text(&d, 0), "ctx 15% 150k/1M");
        d.on_usage(150_000, 0, Some(0.0), false); // a model whose window is unknown
        assert_eq!(ctx_text(&d, 0), "ctx 150k/?");
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
