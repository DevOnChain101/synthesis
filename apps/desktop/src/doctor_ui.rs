//! `/doctor`: the rows of `synt doctor` that matter in a session (`synthesis_app::doctor`), one chat row
//! each with an ok / warn / fail glyph, and the summary the Settings view shows. Names of variables only.

use synthesis_app::doctor::{Level, Row, summary};
use synthesis_tui_base::complete::Command;

use crate::live::{Action, Live, Submitted};

pub fn doctor_command() -> Command {
    Command {
        name: "doctor".into(),
        description: "check the installation: providers, models, environment, sandbox, MCP, hooks, memory, stores".into(),
        local: true,
        ..Command::default()
    }
}

/// The text of one chat row.
pub fn line(r: &Row) -> String {
    format!("{} {}", r.level.glyph(), r.text)
}

/// How long the checks may take before the window stops waiting.
pub const LIMIT: std::time::Duration = std::time::Duration::from_secs(20);

impl Live {
    /// Says "checking" and asks the window to run the checks off the UI thread.
    pub fn doctor_cmd(&mut self) -> Submitted {
        if self.engine.is_none() {
            self.note("/doctor: the session is still starting", true);
            return Submitted::Nothing;
        }
        self.note("doctor: checking...", false);
        Submitted::Do(Action::Doctor)
    }

    pub fn doctor_done(&mut self, r: Result<Vec<Row>, String>) {
        match r {
            Err(e) => self.note(format!("/doctor: {e}"), true),
            Ok(r) => {
                for row in &r {
                    self.note(line(row), row.level != Level::Ok);
                }
                self.note(format!("doctor: {}", summary(&r)), false);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use synthesis_view::convo::Item;

    #[test]
    fn rows_carry_a_glyph_per_level() {
        let r = |level| {
            line(&Row {
                level,
                text: "x".into(),
            })
        };
        assert_eq!(
            [r(Level::Ok), r(Level::Warn), r(Level::Fail)],
            ["\u{2713} x", "! x", "\u{2717} x"]
        );
    }

    #[test]
    fn doctor_reports_rows_and_a_timeout_in_the_chat() {
        let mut l = Live::new();
        l.doctor_done(Err("timed out after 20 s".into()));
        l.doctor_done(Ok(vec![Row {
            level: Level::Warn,
            text: "w".into(),
        }]));
        let t: Vec<String> = l
            .view
            .convo
            .items
            .iter()
            .filter_map(|i| match i {
                Item::Notice { text, .. } => Some(text.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(
            t,
            [
                "/doctor: timed out after 20 s",
                "! w",
                "doctor: 0 ok, 1 warn, 0 fail"
            ]
        );
    }

    #[test]
    fn doctor_waits_for_the_session() {
        let mut l = Live::new();
        l.commands.push(doctor_command());
        l.starting = false;
        l.submit("/doctor");
        assert!(
            matches!(l.view.convo.items.last(), Some(Item::Notice { warning: true, text }) if text.contains("still starting"))
        );
    }
}
