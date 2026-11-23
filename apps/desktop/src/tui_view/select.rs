//! Which session the chat shows when a workspace gets the selection (owner decisions 2026-11-24, docs/specs/desktop.md
//! "Workspace focus"). Pure rules: the driver feeds them the listed sessions and applies the answer.

use std::collections::HashMap;

/// One listed session of the selected workspace, in list order.
pub struct Cand<'a> {
    pub id: &'a str,
    /// A process holds the session (its engine is live), whatever it is doing.
    pub running: bool,
    /// Its last activity (the store's `mtime`, always set for a stored session).
    pub last: i64,
}

/// The row to show: the running sessions are the pool when there are any, else all of them; in the pool the
/// workspace's last active session, else the one with the newest activity (the earlier row on a tie). `None`:
/// the workspace has no session (a new draft).
pub fn pick(rows: &[Cand], active: Option<&str>) -> Option<usize> {
    let any_running = rows.iter().any(|c| c.running);
    let pool = || (0..rows.len()).filter(|&i| !any_running || rows[i].running);
    pool()
        .find(|&i| Some(rows[i].id) == active)
        .or_else(|| pool().max_by_key(|&i| (rows[i].last, std::cmp::Reverse(i))))
}

/// The session each workspace last had focus, started or activity in (workspace key -> session id). Kept in
/// `desktop-state.json` (`last_session::{load,save}_active`).
#[derive(Default)]
pub struct LastActive(pub HashMap<String, String>);

impl LastActive {
    /// Records `sid` for `ws`; true when that changed something (a draft, `""`, is never recorded).
    pub fn set(&mut self, ws: &str, sid: &str) -> bool {
        if sid.is_empty() || self.get(ws) == Some(sid) {
            return false;
        }
        self.0.insert(ws.to_string(), sid.to_string());
        true
    }

    pub fn get(&self, ws: &str) -> Option<&str> {
        self.0.get(ws).map(String::as_str)
    }
}

/// The chat pane wears the focus frame while it holds the keys, and while a sidebar list holds them and the chat
/// follows its selection (a stored session or a draft on show); the keys stay in the list.
pub fn chat_highlight(chat_focused: bool, list_focused: bool, following: bool) -> bool {
    chat_focused || (list_focused && following)
}

/// The wait before a stored transcript is read after a selection (rapid arrow travel reads only where it stops).
pub const DEBOUNCE_MS: u64 = 120;

#[cfg(test)]
mod tests {
    use super::*;

    fn rows<'a>(v: &[(&'a str, bool, i64)]) -> Vec<Cand<'a>> {
        v.iter()
            .map(|&(id, running, last)| Cand { id, running, last })
            .collect()
    }

    #[test]
    fn running_first_then_last_active_then_newest_activity() {
        assert_eq!(pick(&[], Some("x")), None, "no session: a new draft");
        // one running wins over a remembered stored one
        let r = rows(&[("a", false, 1), ("b", true, 2), ("c", false, 3)]);
        assert_eq!(pick(&r, Some("c")), Some(1));
        // several running: the last active among them, else the newest activity (not the first listed)
        let r = rows(&[("a", true, 1), ("b", true, 5), ("c", false, 9)]);
        assert_eq!(pick(&r, Some("a")), Some(0));
        assert_eq!(pick(&r, Some("c")), Some(1));
        assert_eq!(pick(&r, None), Some(1));
        // none running: the last active if listed, else the newest activity, a tie going to the earlier row
        let r = rows(&[("a", false, 4), ("b", false, 7), ("c", false, 7)]);
        assert_eq!(pick(&r, Some("a")), Some(0));
        assert_eq!(pick(&r, Some("gone")), Some(1));
        assert_eq!(pick(&r, None), Some(1));
    }

    #[test]
    fn the_memory_is_per_workspace_and_a_draft_is_not_remembered() {
        let mut m = LastActive::default();
        assert!(m.set("w1", "s1") && m.set("w2", "s9"));
        assert!(
            !m.set("w1", "") && !m.set("w1", "s1"),
            "no change, no write"
        );
        assert_eq!(
            (m.get("w1"), m.get("w2"), m.get("w3")),
            (Some("s1"), Some("s9"), None)
        );
        assert!(m.set("w1", "s2") && m.get("w1") == Some("s2"));
    }

    #[test]
    fn the_chat_highlight_is_a_function_of_the_focus_and_the_selection() {
        assert!(
            chat_highlight(true, false, false),
            "the composer holds the keys"
        );
        assert!(
            chat_highlight(false, true, true),
            "a list holds them and the chat follows it"
        );
        assert!(
            !chat_highlight(false, true, false),
            "nothing followed: the open session's own frame"
        );
    }
}
