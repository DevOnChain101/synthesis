//! `/doctor`: the rows of `synt doctor` that matter in a session (`synthesis_app::doctor`), one chat row
//! each with an ok / warn / fail glyph, and the summary the Settings view shows. Names of variables only.

use std::path::Path;

use synthesis_app::doctor::{Level, Row, rows, summary};
use synthesis_tui_base::complete::Command;

use crate::live::Live;

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

impl Live {
    pub fn doctor_cmd(&mut self) {
        let Some(e) = &self.engine else {
            return self.note("/doctor: the session is still starting", true);
        };
        let r = rows(&e.env, Path::new(&e.cwd), self.trusted);
        for row in &r {
            self.note(line(row), row.level != Level::Ok);
        }
        self.note(format!("doctor: {}", summary(&r)), false);
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
