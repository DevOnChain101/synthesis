//! Digests without secrets (super-agent-s1.md 4.5): built by the host, never by a model. Order: `Redactor`,
//! `sanitize_for_display`, the shared invisible-character set, caps, then a fence. Outcome and item digests are
//! typed fields only, so no worker transcript, brief or diff text can reach the conductor.

use serde_json::{Value, json};
use synthesis_events::Redactor;
use synthesis_harness::{fence, is_invisible_or_blank, sanitize_for_display};

use super::reads::{ItemRow, OutcomeRow, SessionReport, SessionRow};

pub const SESSION_REPORT_MAX: usize = 2 * 1024;
pub const REPORT_MAX: usize = 16 * 1024;
pub const TITLE_MAX: usize = 120;
const LIST_MAX: usize = 50;

/// A control or invisible character other than newline and tab: refused in proposals and messages (T11).
pub fn hidden(c: char) -> bool {
    c != '\n' && c != '\t' && is_invisible_or_blank(c)
}

/// An id or handle that is safe to put in a fence attribute and in the model's view.
pub fn valid_id(s: &str) -> bool {
    (1..=64).contains(&s.len())
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

pub struct Digests {
    redactor: Redactor,
}

impl Digests {
    pub fn new(secrets: Vec<String>) -> Self {
        Self {
            redactor: Redactor::new(secrets),
        }
    }

    pub fn redactor(&self) -> &Redactor {
        &self.redactor
    }

    /// Text safe to show a model or a person: redacted, hidden characters made visible, at most `max` bytes.
    pub fn clean(&self, s: &str, max: usize) -> String {
        cap(&sanitize_for_display(&self.redactor.redact_str(s)), max)
    }

    fn one_line(&self, s: &str, max: usize) -> String {
        self.clean(s, max).replace('\n', " ")
    }

    pub fn session_rows(&self, rows: &[SessionRow]) -> Value {
        let rows: Vec<Value> = rows
            .iter()
            .take(LIST_MAX)
            .map(|r| {
                json!({
                    "handle": r.handle, "title": self.one_line(&r.title, TITLE_MAX),
                    "state": self.one_line(&r.state, 32), "model": self.one_line(&r.model, 80),
                    "mode": self.one_line(&r.mode, 32), "cwd": self.one_line(&r.cwd_label, 80),
                    "spent_usd": r.spent_usd, "task": self.one_line(&r.task_summary, 120), "live": r.live,
                })
            })
            .collect();
        json!({"sessions": fenced_data("session_report", "", &json!(rows).to_string())})
    }

    pub fn outcome_row(&self, r: &OutcomeRow) -> Value {
        json!({
            "id": r.id, "title": self.one_line(&r.title, TITLE_MAX), "kind": self.one_line(&r.kind, 32),
            "state": self.one_line(&r.state, 32), "needs_you": r.needs_you,
            "spent_usd": r.spent_usd, "cap_usd": r.cap_usd,
        })
    }

    pub fn outcome_rows(&self, rows: &[OutcomeRow]) -> Value {
        let rows: Vec<Value> = rows
            .iter()
            .take(LIST_MAX)
            .map(|r| self.outcome_row(r))
            .collect();
        json!({"outcomes": fenced_data("outcome_digest", "", &json!(rows).to_string())})
    }

    /// At most 2 KiB. A pending ask shows its kind and tool name only.
    pub fn session_digest(&self, row: &SessionRow, rep: &SessionReport) -> Value {
        let mut t = format!(
            "state: {}\nlast reply: {}\nlast tools: {}",
            self.one_line(&row.state, 32),
            self.clean(&rep.last_reply, 1200),
            rep.last_tools
                .iter()
                .rev()
                .take(5)
                .map(|t| self.one_line(t, 40))
                .collect::<Vec<_>>()
                .join(", ")
        );
        if let Some((kind, tool)) = &rep.pending_ask {
            t += &format!(
                "\npending ask: {} {}",
                self.one_line(kind, 32),
                self.one_line(tool, 40)
            );
        }
        let attrs = format!(" handle=\"{}\"", row.handle);
        json!({"digest": fenced_data("session_report", &attrs, &cap(&t, SESSION_REPORT_MAX))})
    }

    pub fn outcome_digest(&self, r: &OutcomeRow) -> Value {
        let attrs = format!(" id=\"{}\"", r.id);
        json!({"digest": fenced_data("outcome_digest", &attrs, &self.outcome_row(r).to_string())})
    }

    pub fn item_digest(&self, r: &ItemRow) -> Value {
        let body = json!({
            "id": r.id, "outcome": r.outcome_id, "state": self.one_line(&r.state, 32),
            "attempts": r.attempts, "needs_you": r.needs_you, "spent_usd": r.spent_usd,
        });
        let attrs = format!(" id=\"{}\"", r.id);
        json!({"digest": fenced_data("outcome_digest", &attrs, &body.to_string())})
    }

    /// The worker's own words: cleaned, capped so the fenced string still fits one tool result after JSON escaping.
    pub fn report_digest(&self, outcome_id: &str, text: &str) -> Value {
        let mut max = REPORT_MAX;
        loop {
            let attrs = format!(" outcome=\"{outcome_id}\" trust=\"model-written\"");
            let s = fenced_data("report", &attrs, &self.clean(text, max));
            if max <= 512 || json!(s).to_string().len() < crate::host_tools::MAX_RESULT - 256 {
                return json!({"digest": s});
            }
            max = max * 9 / 10;
        }
    }
}

fn fenced_data(tag: &str, attrs: &str, text: &str) -> String {
    let trust = if attrs.contains("trust=") {
        ""
    } else {
        " trust=\"session-data\""
    };
    format!("<{tag}{attrs}{trust}>\n{}\n</{tag}>", fence(tag, text))
}

/// At most `max` bytes, on a character boundary.
pub fn cap(s: &str, max: usize) -> String {
    let mut n = max.min(s.len());
    while !s.is_char_boundary(n) {
        n -= 1;
    }
    if n == s.len() {
        s.to_string()
    } else {
        format!("{}[...]", &s[..n])
    }
}
