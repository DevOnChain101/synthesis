//! What a session is doing, as the window shows it: the working line above the prompt (every kind of activity,
//! never a busy state without it) and the state mark of a row of the Running list. Pure functions of the live
//! state and of a stored session's status, so the mapping is tested without a window.

use synthesis_core::SessionStatus;
use synthesis_view::convo::Item;

use crate::live::Live;

/// One thing the working line can say, most pressing first.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Activity {
    /// A dialog waits for the person.
    Approval,
    /// `/compact` between tasks is running.
    Compacting,
    /// A sub-agent of this kind is working.
    Agent(String),
    /// This tool (Bash, Read, Grep, an MCP tool, a web fetch...) is running.
    Tool(String),
    /// The verification engine is checking the result.
    Verifying,
    /// The engine is starting.
    Starting,
    /// The model is thinking or answering.
    Thinking,
}

/// The activity of the open session, `None` when it is idle.
pub fn activity_of(l: &Live) -> Option<Activity> {
    let c = &l.view.convo;
    if l.dialogs.iter().any(|d| d.needs_person()) {
        return Some(Activity::Approval);
    }
    if l.compacting {
        return Some(Activity::Compacting);
    }
    if !(l.busy || c.running) && !l.starting {
        return None;
    }
    if let Some((_, kind)) = c.agents.last() {
        return Some(Activity::Agent(kind.clone()));
    }
    if let Some(tool) = c.running_tool() {
        return Some(Activity::Tool(tool.to_string()));
    }
    if c.session_status == "verifying" {
        return Some(Activity::Verifying);
    }
    if l.starting {
        return Some(Activity::Starting);
    }
    Some(Activity::Thinking)
}

/// The mark of a session's row in the Running list.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum RunState {
    /// Working: an animated spinner.
    Running,
    /// Waits for the person: a blinking yellow "!".
    Approval,
    #[default]
    Idle,
    /// Its last task failed or was blocked.
    Error,
}

/// A stored session's state from its status column (what the other rows of the list have).
pub fn run_state_of_status(s: SessionStatus) -> RunState {
    use SessionStatus::*;
    match s {
        Preparing | Running | Verifying | Cancelling | Recovering => RunState::Running,
        WaitingApproval => RunState::Approval,
        Failed | Blocked | Interrupted => RunState::Error,
        _ => RunState::Idle,
    }
}

/// The open session's state, from what the window knows first-hand.
pub fn run_state_of_live(l: &Live) -> RunState {
    match activity_of(l) {
        Some(Activity::Approval) => RunState::Approval,
        Some(_) => RunState::Running,
        None => match l.view.convo.items.last() {
            Some(Item::TaskEnd {
                ok: false, word, ..
            }) if word == "failed" || word == "blocked" => RunState::Error,
            _ => RunState::Idle,
        },
    }
}

/// The status word of a row ("waiting approval", not the debug name run together).
pub fn status_word(s: SessionStatus) -> &'static str {
    use SessionStatus::*;
    match s {
        Created => "created",
        Preparing => "preparing",
        Idle => "idle",
        Running => "running",
        WaitingApproval => "waiting approval",
        Verifying => "verifying",
        Completed => "completed",
        Unverified => "unverified",
        Blocked => "blocked",
        Failed => "failed",
        Cancelling => "cancelling",
        Cancelled => "cancelled",
        Interrupted => "interrupted",
        Recovering => "recovering",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_stored_status_maps_to_a_row_state() {
        use SessionStatus::*;
        for (s, want) in [
            (Running, RunState::Running),
            (Verifying, RunState::Running),
            (WaitingApproval, RunState::Approval),
            (Idle, RunState::Idle),
            (Completed, RunState::Idle),
            (Failed, RunState::Error),
            (Blocked, RunState::Error),
        ] {
            assert_eq!(run_state_of_status(s), want, "{s:?}");
        }
        assert_eq!(status_word(WaitingApproval), "waiting approval");
    }

    #[test]
    fn the_open_session_maps_from_what_the_window_knows() {
        let mut l = Live::new();
        l.starting = false;
        assert_eq!(activity_of(&l), None);
        assert_eq!(run_state_of_live(&l), RunState::Idle);
        l.busy = true;
        assert_eq!(activity_of(&l), Some(Activity::Thinking));
        assert_eq!(run_state_of_live(&l), RunState::Running);
        l.view.convo.session_status = "verifying".into();
        assert_eq!(activity_of(&l), Some(Activity::Verifying));
        l.busy = false;
        l.compacting = true;
        assert_eq!(activity_of(&l), Some(Activity::Compacting));
        l.compacting = false;
        l.view.convo.session_status.clear();
        l.starting = true;
        assert_eq!(activity_of(&l), Some(Activity::Starting));
        // a dialog that waits for the person beats everything
        l.dialogs.push(crate::live::Dialog {
            kind: crate::live::DialogKind::Plan {
                id: "p".into(),
                plan: String::new(),
            },
            options: vec![],
            sel: 0,
            armed_until: 0,
            engaged: false,
        });
        assert_eq!(activity_of(&l), Some(Activity::Approval));
        assert_eq!(run_state_of_live(&l), RunState::Approval);
        // idle after a failed task: error
        let mut l = Live::new();
        l.starting = false;
        l.view.convo.items.push(Item::TaskEnd {
            ok: false,
            text: "x".into(),
            word: "failed".into(),
            checks: String::new(),
        });
        assert_eq!(run_state_of_live(&l), RunState::Error);
    }
}
