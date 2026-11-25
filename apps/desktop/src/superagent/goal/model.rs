//! The pieces of the goal form that need no window: which field an error line belongs to, why Send is off, and
//! the rule for the approve button (the decision card's: acts only on a fresh, deliberate press, 400 ms after the
//! dialog showed). The contract, its validation and the flow are `superagent::outcomes::form`, unchanged.

use std::time::Instant;

use synthesis_view::outcomes::WSL2_REFUSAL;

use crate::superagent::chat::model::ARM;
use crate::superagent::frames::GoalKind;
use crate::superagent::outcomes::form::{Form, Stage};
use crate::superagent::outcomes::model::Screen;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Field {
    Goal,
    /// Acceptance criteria (writes) or the rubric (read-only).
    Criteria,
    Checks,
    Scope,
    Budget,
    Attempts,
    Folder,
}

/// The field a one-line error of `form::build` / `build_read_only` / `Form::submit` is about, by its prefix;
/// `None` for the core's own `validate_contract` text and for refusals, which go above the form.
pub fn field_of(err: &str) -> Option<Field> {
    let head = err.split(':').next()?;
    Some(match head {
        "Goal" => Field::Goal,
        "Acceptance criteria" => Field::Criteria,
        "Checks" => Field::Checks,
        "Write scope" => Field::Scope,
        "Budget" => Field::Budget,
        "Attempts" => Field::Attempts,
        "Workspace" | "Folder" | "Base commit" => Field::Folder,
        _ => return None,
    })
}

pub const READ_ONLY_SOON: &str = crate::superagent::outcomes::kind_picker::READ_ONLY_SOON;

/// Why this form cannot be used at all right now (it is then replaced by the explanation), or `None`.
pub fn blocked(screen: &Screen) -> Option<&'static str> {
    match screen {
        Screen::Windows => Some(WSL2_REFUSAL),
        Screen::Off => Some(
            "Super Agent is off. Turn it on in Settings, then come back: nothing is created while it is off.",
        ),
        _ => None,
    }
}

/// Why Send does nothing now, or `None` when it would send. Enter and Cmd+Enter obey the same answer.
pub fn why_not_send(
    kind: GoalKind,
    form: &Form,
    screen: &Screen,
    read_only_goal: bool,
    goal: &str,
) -> Option<&'static str> {
    Some(match screen {
        _ if form.stage != Stage::Edit => "Sending...",
        Screen::Ready if kind == GoalKind::ReadOnly && !read_only_goal => READ_ONLY_SOON,
        Screen::Ready => {
            if goal.trim().is_empty() {
                "Write the goal first"
            } else {
                return None;
            }
        }
        Screen::Loading => "Connecting to the orchestrator...",
        _ => "Not connected to the orchestrator",
    })
}

/// The approve button acts on a deliberate press only: a click or Enter, not a held key, not before the dialog
/// has been on screen for `ARM`, and Enter only on the button the person tabbed to.
pub fn approve_ok(opened: Instant, now: Instant, held: bool, key: bool, focused: bool) -> bool {
    !held && (focused || !key) && now.saturating_duration_since(opened) >= ARM
}
