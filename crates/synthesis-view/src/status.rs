//! Status-bar data: the sandbox words, the context-window usage and a [`StatusModel`] that folds
//! the event stream into what the status line shows (context, tokens, cost, turns, tools, model,
//! mode, cwd, uptime). Colours and layout stay with the front end; [`Level`] says how loud.

use serde::Serialize;
use synthesis_events::{Envelope, Event};

use crate::model::parse_ts;

/// How loudly a front end should show a value.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub enum Level {
    Normal,
    Warn,
    Critical,
}

/// What the host built for command confinement. Strings, so this crate does not depend on the
/// sandbox backends: the CLI fills it from `SandboxMode` and `SandboxBackend::capabilities()`.
#[derive(Clone, Debug, Default, Serialize)]
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
    /// The words and whether they deserve a warning colour.
    pub fn text(&self) -> (String, bool) {
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
pub const COMPACTED_NOTE_MS: i64 = 8000;

/// Context-window usage for the status line. Fed by `context.usage` / `context.compacting` /
/// `context.compacted` / `context.compaction_failed` (see `Chat::apply`); knows nothing of the
/// event types, so it is testable without them.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
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

    pub fn set_window(&mut self, window: u64) {
        self.window = window;
        self.percent = None;
    }

    /// A new conversation on the same model: the numbers go, the window stays.
    pub fn reset(&mut self) {
        *self = Self {
            window: self.window,
            ..Self::default()
        };
    }

    /// `(used, window)` tokens; `(0, 0)` until the first number arrives, `(0, window)` in a
    /// cleared conversation.
    pub fn numbers(&self) -> (u64, u64) {
        if self.known {
            (self.used, self.window)
        } else {
            (0, self.window)
        }
    }

    /// `ctx 62% 124k/200k`, `⟳ compacting…`, plus the brief compaction note; `None` before any
    /// number arrived. The level says how loudly a front end should show it.
    pub fn label(&self, now: i64) -> Option<(String, Level)> {
        let note = self
            .note
            .as_ref()
            .filter(|n| now < n.1)
            .map(|n| n.0.as_str());
        if self.compacting {
            return Some(("⟳ compacting…".into(), Level::Warn));
        }
        if !self.known {
            return note.map(|n| (n.to_string(), Level::Normal));
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
            format!("ctx {}/?", tok(self.used))
        };
        if self.stale {
            s.push('~'); // estimate older than the last history change
        }
        if let Some(n) = note {
            s = format!("{s} {n}");
        }
        let level = match pct {
            p if p >= 90.0 => Level::Critical,
            p if p >= 75.0 => Level::Warn,
            _ => Level::Normal,
        };
        Some((s, level))
    }
}

/// What the status line shows, folded from the events like the TUI's totals: turns and tool calls
/// count every agent, tokens/cost and the context window only the main one.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct StatusModel {
    pub context: ContextStatus,
    pub model: String,
    pub effort: Option<String>,
    pub mode: String,
    pub cwd: String,
    /// Epoch ms of `session.started` (uptime = now - this).
    pub started_ms: Option<i64>,
    pub turns: u32,
    pub tools: u32,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read: u64,
    pub cache_create: u64,
    /// `None` until a turn reported a cost (an unknown cost is not zero).
    pub cost_usd: Option<f64>,
}

impl StatusModel {
    pub fn apply(&mut self, e: &Envelope, now: i64) {
        let main = e.agent_id.is_none();
        match &e.event {
            Event::SessionStarted {
                workspace,
                model,
                permission_mode,
                ..
            } => {
                self.model.clone_from(model);
                self.mode.clone_from(permission_mode);
                self.cwd.clone_from(&workspace.cwd);
                self.started_ms = parse_ts(&e.ts).or(self.started_ms);
            }
            Event::ModelChanged { model, effort } => {
                self.model.clone_from(model);
                self.effort.clone_from(effort);
            }
            Event::SessionModeChanged { to, .. } => self.mode.clone_from(to),
            Event::TurnStarted { .. } => self.turns += 1,
            Event::ToolRequested { .. } => self.tools += 1,
            Event::TurnCompleted { usage: Some(u), .. } if main => {
                self.input_tokens += u.input_tokens;
                self.output_tokens += u.output_tokens;
                self.cache_read += u.cache_read_input_tokens.unwrap_or(0);
                self.cache_create += u.cache_creation_input_tokens.unwrap_or(0);
                if let Some(c) = u.cost_usd {
                    *self.cost_usd.get_or_insert(0.0) += c;
                }
            }
            Event::ContextUsage {
                used_tokens,
                window_tokens,
                percent,
                stale,
                ..
            } if main => {
                self.context
                    .on_usage(*used_tokens, *window_tokens, Some(*percent), *stale)
            }
            Event::ContextCompacting { .. } if main => self.context.on_compacting(),
            Event::ContextCompactionFailed { .. } => self.context.on_compaction_failed(),
            Event::ContextCompacted {
                tokens_before,
                before_tokens,
                after_tokens,
                window_tokens,
                ..
            } if main => {
                // logs from before the numbers existed carry only `tokens_before`
                let before = if *before_tokens > 0 {
                    *before_tokens
                } else {
                    tokens_before.unwrap_or(0)
                };
                self.context
                    .on_compacted(before, *after_tokens, *window_tokens, now);
            }
            _ => {}
        }
    }

    /// Share of the prompt tokens served from the cache, 0..=1; `None` before any prompt.
    pub fn cache_hit(&self) -> Option<f64> {
        let all = self.input_tokens + self.cache_read + self.cache_create;
        (all > 0).then(|| self.cache_read as f64 / all as f64)
    }

    pub fn uptime_ms(&self, now: i64) -> Option<i64> {
        self.started_ms.map(|s| (now - s).max(0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use synthesis_events::Usage;

    fn env(event: Event) -> Envelope {
        Envelope {
            v: 1,
            seq: 1,
            ts: "2026-11-07T10:00:00Z".into(),
            session_id: "s1".into(),
            task_id: None,
            turn_id: None,
            agent_id: None,
            event,
        }
    }

    #[test]
    fn folds_turns_tokens_cost_and_context() {
        let mut m = StatusModel::default();
        m.apply(
            &env(Event::TurnStarted {
                model: "m".into(),
                provider: "p".into(),
                iteration: 1,
            }),
            0,
        );
        let usage = Usage {
            input_tokens: 100,
            output_tokens: 20,
            cache_read_input_tokens: Some(300),
            cache_creation_input_tokens: None,
            cost_usd: Some(0.5),
        };
        m.apply(
            &env(Event::TurnCompleted {
                stop_reason: "end_turn".into(),
                usage: Some(usage),
                duration_ms: 1,
            }),
            0,
        );
        m.apply(
            &env(Event::ContextUsage {
                used_tokens: 124_000,
                window_tokens: 200_000,
                percent: 62.0,
                source: "provider".into(),
                stale: false,
            }),
            0,
        );
        assert_eq!((m.turns, m.input_tokens, m.output_tokens), (1, 100, 20));
        assert_eq!(m.cost_usd, Some(0.5));
        assert_eq!(m.cache_hit(), Some(0.75));
        assert_eq!(
            m.context.label(0),
            Some(("ctx 62% 124k/200k".into(), Level::Normal))
        );
        assert_eq!(
            StatusModel::default().cost_usd,
            None,
            "unknown cost is not zero"
        );
    }

    #[test]
    fn sub_agent_context_does_not_move_the_main_window() {
        let mut m = StatusModel::default();
        let mut e = env(Event::ContextUsage {
            used_tokens: 5,
            window_tokens: 10,
            percent: 50.0,
            source: "provider".into(),
            stale: false,
        });
        e.agent_id = Some("a1".into());
        m.apply(&e, 0);
        assert_eq!(m.context.label(0), None);
    }

    #[test]
    fn context_level_follows_percent() {
        let mut c = ContextStatus::default();
        c.on_usage(180, 200, None, false);
        assert_eq!(c.label(0).unwrap().1, Level::Critical);
        c.on_usage(160, 200, None, false);
        assert_eq!(c.label(0).unwrap().1, Level::Warn);
        c.on_compacting();
        assert_eq!(c.label(0).unwrap().0, "⟳ compacting…");
    }
}
