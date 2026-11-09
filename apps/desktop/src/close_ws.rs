//! `/close-workspace` and the Workspace panel's close button: one confirmation, then every session of
//! the folder is marked closed (`Store::close_workspace`; nothing is deleted, `/resume <id>` still opens
//! one). Plain Rust, no gpui. This module only asks; the closing is `tui_view::driver`'s.

use synthesis_core::SessionStatus;
use synthesis_harness::sanitize_for_display;
use synthesis_view::workspaces::fold_dir;

use crate::live::{Dialog, DialogKind, Live, PickerKind, age};

/// Sessions named per list before "and N more".
const LISTED: usize = 8;

/// A session is working now (or waits for an answer), by its stored status.
fn working(s: SessionStatus) -> bool {
    matches!(
        s,
        SessionStatus::Running | SessionStatus::WaitingApproval | SessionStatus::Verifying
    )
}

fn lines(head: &str, items: &[String], out: &mut Vec<String>) {
    if items.is_empty() {
        return;
    }
    out.push(head.into());
    out.extend(items.iter().take(LISTED).map(|i| format!("  - {i}")));
    if items.len() > LISTED {
        out.push(format!("  ... and {} more", items.len() - LISTED));
    }
}

impl Live {
    /// The folder the engine runs in, as a workspace key.
    pub fn live_ws_key(&self) -> String {
        fold_dir(self.engine.as_ref().map_or(&self.cwd, |e| &e.root))
    }

    /// Opens the confirmation for the open folder (`key` None) or for the listed workspace `key`.
    /// Cancel is the selected option, Esc cancels, and the dialog has to be engaged like the other
    /// confirmations. Another open dialog (an approval, a picker) is answered first.
    pub fn ask_close_workspace(&mut self, key: Option<&str>, now: i64) {
        const CMD: &str = "/close-workspace";
        if !self.dialogs.is_empty() {
            return self.note(format!("{CMD}: answer the open question first"), true);
        }
        let key = key.map_or_else(|| self.live_ws_key(), String::from);
        let is_live = key == self.live_ws_key();
        if is_live {
            if self.starting {
                return self.note(format!("{CMD}: the session is still starting"), true);
            }
            if let Some(why) = self.engine_switch_blocker() {
                return self.note(format!("{CMD}: {why}"), true);
            }
        }
        let row = self
            .view
            .workspaces
            .workspaces
            .iter()
            .find(|w| w.key == key);
        if row.is_none() && !is_live {
            return self.note(format!("{CMD}: that workspace is no longer listed"), true);
        }
        let (name, root) = match (row, &self.engine) {
            (Some(w), _) => (w.label.clone(), w.root.clone()),
            (None, Some(e)) => (synthesis_view::text::basename(&e.root), e.root.clone()),
            (None, None) => (synthesis_view::text::basename(&self.cwd), self.cwd.clone()),
        };
        let (mut to_close, mut here, mut elsewhere) = (0usize, vec![], vec![]);
        let mut seen_open = false;
        for s in row.iter().flat_map(|w| &w.sessions) {
            let title = sanitize_for_display(&s.title);
            if is_live && s.id == self.sid {
                seen_open = true;
                to_close += 1;
                if self.busy {
                    here.push(format!("{title} (running now)"));
                }
            } else if s.is_live {
                let what = if working(s.status) {
                    "running elsewhere"
                } else {
                    "open in another process"
                };
                elsewhere.push(format!(
                    "{title} ({what}, {}): will NOT be closed",
                    age(now - s.mtime)
                ));
            } else {
                to_close += 1;
            }
        }
        // the open session is closed too, even before it has a message
        if is_live && !seen_open && !self.sid.is_empty() {
            to_close += 1;
            if self.busy {
                here.push(format!(
                    "{} (running now)",
                    sanitize_for_display(&self.session_name())
                ));
            }
        }
        if to_close == 0 {
            return self.note(
                format!(
                    "{CMD}: nothing to close in {name}: every session is open in another process"
                ),
                false,
            );
        }
        let mut body = vec![
            format!("Workspace: {}", sanitize_for_display(&name)),
            format!("Path: {}", sanitize_for_display(&root)),
            format!(
                "Sessions to close: {to_close}{}",
                if is_live {
                    " (this window then starts a fresh session in the same folder)"
                } else {
                    ""
                }
            ),
        ];
        lines(
            "Running here, the task will be CANCELLED:",
            &here,
            &mut body,
        );
        lines("Not closed:", &elsewhere, &mut body);
        body.push(
            "Nothing is deleted: a closed session leaves the lists, and /resume <id> still opens it."
                .into(),
        );
        let (title, yes) = if here.is_empty() {
            (
                format!(
                    "Close workspace {} ({to_close} session{})?",
                    sanitize_for_display(&name),
                    if to_close == 1 { "" } else { "s" }
                ),
                "Close the workspace".to_string(),
            )
        } else {
            (
                format!(
                    "Close workspace {} and cancel the running task?",
                    sanitize_for_display(&name)
                ),
                "Cancel the running task and close the workspace".to_string(),
            )
        };
        self.push_dialog(
            Dialog {
                kind: DialogKind::Picker {
                    kind: PickerKind::CloseWorkspace,
                    title,
                    headers: vec![(0, body.join("\n"))],
                },
                options: vec![
                    ("cancel".into(), "Cancel (close nothing)".into()),
                    (format!("close:{key}"), yes),
                ],
                sel: 0,
                armed_until: 0,
                engaged: false,
            },
            now,
        );
    }
}

/// The note after the closing: how many were closed, left open, failed.
pub fn report(name: &str, r: &synthesis_session::WorkspaceClose) -> String {
    let mut s = format!(
        "/close-workspace: {} closed {} session{}; nothing was deleted, /resume <id> opens one",
        sanitize_for_display(name),
        r.closed.len(),
        if r.closed.len() == 1 { "" } else { "s" }
    );
    if !r.held.is_empty() {
        s += &format!("; {} left open (another process holds them)", r.held.len());
    }
    if !r.failed.is_empty() {
        let why: Vec<String> = r
            .failed
            .iter()
            .take(3)
            .map(|(id, e)| format!("{id}: {}", sanitize_for_display(e)))
            .collect();
        s += &format!("; {} failed ({})", r.failed.len(), why.join(", "));
    }
    s
}

/// The store half, blocking: every listed session of the folder `key` that no other process holds.
pub fn close_in_store(
    home: &std::path::Path,
    key: &str,
) -> Result<synthesis_session::WorkspaceClose, String> {
    synthesis_session::Store::open(home, synthesis_session::StoreOptions::default())
        .and_then(|s| s.close_workspace(|root| fold_dir(root) == key))
        .map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use synthesis_view::workspaces::{SessionRow, WorkspaceRow};

    fn sess(id: &str, status: SessionStatus, is_live: bool) -> SessionRow {
        SessionRow {
            id: id.into(),
            title: format!("title {id}"),
            status,
            updated_at: String::new(),
            mtime: 0,
            is_live,
        }
    }

    fn ws(key: &str, sessions: Vec<SessionRow>) -> WorkspaceRow {
        WorkspaceRow {
            root: format!("/w/{key}"),
            key: key.into(),
            label: key.into(),
            sessions,
            available: true,
            changes: None,
        }
    }

    /// The open folder is `/w/live` (no engine: `cwd` names it); `other` is a listed workspace.
    fn live() -> Live {
        let mut l = Live::new();
        l.starting = false;
        l.cwd = "/w/live".into();
        l.sid = "cur".into();
        l.view.workspaces.workspaces = vec![
            ws(
                &fold_dir("/w/live"),
                vec![
                    sess("cur", SessionStatus::Idle, true),
                    sess("old", SessionStatus::Completed, false),
                    sess("far", SessionStatus::Running, true),
                ],
            ),
            ws(
                "other",
                vec![
                    sess("a", SessionStatus::Idle, false),
                    sess("b", SessionStatus::Idle, false),
                ],
            ),
        ];
        l
    }

    fn front(l: &Live) -> (&str, String, &[(String, String)]) {
        let d = &l.dialogs[0];
        let DialogKind::Picker { title, headers, .. } = &d.kind else {
            panic!("not a picker")
        };
        (title, headers[0].1.clone(), &d.options)
    }

    #[test]
    fn a_listed_workspace_asks_with_cancel_first_and_says_what_it_does() {
        let mut l = live();
        l.ask_close_workspace(Some("other"), 1000);
        let (title, body, opts) = front(&l);
        assert!(
            title.contains("other") && title.contains("2 sessions"),
            "{title}"
        );
        assert!(body.contains("Path: /w/other") && body.contains("Sessions to close: 2"));
        assert!(body.contains("Nothing is deleted") && body.contains("/resume <id>"));
        assert!(!body.contains("CANCELLED"));
        assert_eq!(opts[0].0, "cancel");
        assert_eq!(opts[1].0, "close:other");
        let d = &l.dialogs[0];
        // Cancel is selected, the dialog needs engaging, and Esc cancels
        assert!(d.sel == 0 && d.needs_person() && !d.answerable() && d.armed_until >= 1000);
        assert_eq!(l.reject(), None);
        assert!(l.dialogs.is_empty());
    }

    #[test]
    fn the_open_folder_counts_its_session_and_lists_what_another_process_holds() {
        let mut l = live();
        l.ask_close_workspace(None, 0);
        let (_, body, opts) = front(&l);
        // cur + old close; far is held by another process
        assert!(body.contains("Sessions to close: 2"), "{body}");
        assert!(body.contains("fresh session in the same folder"));
        assert!(
            body.contains("title far (running elsewhere") && body.contains("will NOT be closed")
        );
        assert!(!body.contains("CANCELLED"));
        assert_eq!(opts[1].0, format!("close:{}", fold_dir("/w/live")));
    }

    #[test]
    fn a_running_task_here_is_named_and_the_yes_says_it_cancels() {
        let mut l = live();
        l.busy = true;
        l.ask_close_workspace(None, 0);
        let (title, body, opts) = front(&l);
        assert!(title.contains("cancel the running task"));
        assert!(
            body.contains("CANCELLED") && body.contains("(running now)"),
            "{body}"
        );
        assert!(opts[1].1.contains("Cancel the running task"));
    }

    #[test]
    fn typing_ahead_does_not_answer_and_the_close_key_names_the_folder() {
        let mut l = live();
        l.ask_close_workspace(Some("other"), 1000);
        assert!(
            !l.dialogs[0].answerable(),
            "Enter and digits wait for the person"
        );
        assert_eq!(l.pick(1, 1000), None, "still arming");
        l.move_sel(true);
        assert!(l.dialogs[0].answerable());
        let k = l.pick(1, 1000 + crate::live::ARM_MS).unwrap();
        assert_eq!(
            l.answer(&k, 5000),
            Some(crate::live::Action::CloseWorkspace("other".into()))
        );
        // Cancel answers nothing
        l.ask_close_workspace(Some("other"), 9000);
        let k = l.pick(0, 9000 + crate::live::ARM_MS).unwrap();
        assert_eq!(l.answer(&k, 9500), None);
    }

    #[test]
    fn it_refuses_beside_another_dialog_a_rewind_an_unknown_row_and_nothing_to_close() {
        let mut l = live();
        l.ask_close_workspace(Some("other"), 0);
        l.ask_close_workspace(Some("other"), 0);
        assert_eq!(l.dialogs.len(), 1, "no second confirmation");
        l.dialogs.clear();

        l.ask_close_workspace(Some("gone"), 0);
        assert!(l.dialogs.is_empty());

        l.rewinding = true;
        l.ask_close_workspace(None, 0);
        assert!(l.dialogs.is_empty(), "a rewind blocks the open folder");
        l.ask_close_workspace(Some("other"), 0);
        assert_eq!(l.dialogs.len(), 1, "another folder needs no restart");
        l.dialogs.clear();
        l.rewinding = false;

        l.view.workspaces.workspaces[1].sessions = vec![sess("x", SessionStatus::Running, true)];
        l.ask_close_workspace(Some("other"), 0);
        assert!(l.dialogs.is_empty(), "everything is held elsewhere");
    }

    #[test]
    fn the_report_counts_closed_held_and_failed() {
        let r = synthesis_session::WorkspaceClose {
            closed: vec!["a".into(), "b".into()],
            held: vec!["c".into()],
            failed: vec![("d".into(), "disk".into())],
        };
        let s = report("api", &r);
        assert!(s.contains("closed 2 sessions") && s.contains("1 left open"));
        assert!(s.contains("1 failed (d: disk)"));
    }
}
