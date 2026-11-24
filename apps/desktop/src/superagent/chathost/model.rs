//! The pure side of the session chat: the shared conversation (`Convo`, the one the TUI view draws, already
//! redacted and cleaned for display by the view model) grouped into message turns, the line under the transcript,
//! and what Enter in the composer does. No window state lives here.

use synthesis_harness::sanitize_for_display;
use synthesis_view::convo::{ApprovalState, Item};

/// Most items drawn at once (the rest is counted in a line at the top): the transcript is a plain column, not a
/// virtual list. shortcut: older items are not scrollable, upgrade to a list when a session needs them.
pub const MAX_ITEMS: usize = 300;

/// One thing inside an assistant turn. `at` is the index of its `Convo` item (ids of elements and of cards).
#[derive(Debug, Clone, PartialEq)]
pub enum Block {
    Text {
        at: usize,
        text: String,
        streaming: bool,
        tainted: bool,
    },
    /// A tool call, drawn from `toolview::shape` of the item at `at`.
    Tool {
        at: usize,
    },
    Thinking {
        at: usize,
        text: String,
    },
    /// A past permission ask, kept in the transcript as one quiet line (a pending one is the card above the composer).
    Ask {
        at: usize,
        state: ApprovalState,
        prompt: String,
    },
    Note {
        warning: bool,
        text: String,
    },
    /// A task that did not complete, or a verification report.
    End {
        ok: bool,
        text: String,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub enum Turn {
    User { at: usize, text: String },
    Assistant { blocks: Vec<Block> },
}

/// Items from `skip` on, grouped: a user line is its own turn; everything between two of them is one assistant turn.
pub fn turns(items: &[Item], skip: usize, show_thinking: bool, verbose: bool) -> Vec<Turn> {
    let mut out: Vec<Turn> = vec![];
    for (at, it) in items.iter().enumerate().skip(skip) {
        let block = match it {
            Item::User(t) => {
                out.push(Turn::User {
                    at,
                    text: sanitize_for_display(t),
                });
                continue;
            }
            Item::Assistant {
                text,
                streaming,
                tainted,
            } => Block::Text {
                at,
                text: text.clone(),
                streaming: *streaming,
                tainted: *tainted,
            },
            Item::Tool { .. } => Block::Tool { at },
            Item::Thinking(t) if show_thinking => Block::Thinking {
                at,
                text: sanitize_for_display(t),
            },
            Item::Approval { prompt, state, .. } => Block::Ask {
                at,
                state: *state,
                prompt: sanitize_for_display(prompt),
            },
            Item::Notice { warning, text } => Block::Note {
                warning: *warning,
                text: sanitize_for_display(text),
            },
            // only a task that did not complete is a line of its own (a completed one is the reply above it)
            Item::TaskEnd {
                ok: false,
                text,
                word,
                ..
            } => Block::End {
                ok: false,
                text: sanitize_for_display(&format!("{word}: {text}")),
            },
            Item::Verification { status, summary } => Block::End {
                ok: status == "passed",
                text: sanitize_for_display(&format!("verification {status}\n{summary}")),
            },
            Item::Internal(t) if verbose => Block::Note {
                warning: false,
                text: sanitize_for_display(t),
            },
            _ => continue,
        };
        match out.last_mut() {
            Some(Turn::Assistant { blocks }) => blocks.push(block),
            _ => out.push(Turn::Assistant {
                blocks: vec![block],
            }),
        }
    }
    out
}

/// How a past ask reads in the transcript: the word and whether it is the "waiting" state.
pub fn ask_word(s: ApprovalState) -> &'static str {
    match s {
        ApprovalState::Pending => "Waiting for your answer",
        ApprovalState::Allowed => "Allowed",
        ApprovalState::Denied => "Denied",
    }
}

/// What the line under the transcript says (`None`: nothing, the session is idle).
pub fn status_line(
    starting: bool,
    busy: bool,
    dialogs: usize,
    queued: usize,
    session_status: &str,
) -> Option<String> {
    let mut s = if starting {
        "Starting the session".to_string()
    } else if dialogs > 0 {
        // never lost: the line says it even when the card is scrolled away
        format!(
            "Waiting for your answer ({dialogs} open request{})",
            if dialogs == 1 { "" } else { "s" }
        )
    } else if busy {
        match session_status {
            "verifying" => "Verifying the result".to_string(),
            "" | "running" => "Working".to_string(),
            other => sanitize_for_display(other),
        }
    } else {
        return None;
    };
    if queued > 0 {
        s.push_str(&format!(" \u{b7} {queued} queued"));
    }
    Some(s)
}
