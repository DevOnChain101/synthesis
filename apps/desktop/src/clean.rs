//! `/clean`: permanently delete stored sessions (and the workspaces that only they kept alive). Plain
//! Rust, no gpui. Three steps, each a dock above the prompt: (1) a picker of SCOPES with live counts,
//! (2) a confirmation that says exactly what goes and what stays, (3) the result row. The decisions
//! (what is protected, what is deleted, in what order) are `synthesis_session::Store::clean`'s; this
//! module presents and asks. Scope "all" also needs the word `delete` (or `clean`) typed.

use std::path::PathBuf;

use synthesis_harness::sanitize_for_display;
use synthesis_session::{CleanPlan, CleanResult, CleanScope, INACTIVE_DAYS, Store, StoreOptions};
use time::OffsetDateTime;

use crate::live::{Action, Dialog, DialogKind, Live, PickerKind, now_ms};
use crate::rewind::size;

/// A clean step that has not answered after this long frees the prompt and the engine again.
pub const JOB_TIMEOUT_MS: u64 = 600_000;
/// The words that enable "all".
const WORDS: [&str; 2] = ["delete", "clean"];
const MAX_TYPED: usize = 16;

pub const SCOPES: [CleanScope; 3] = [
    CleanScope::ClosedEmpty,
    CleanScope::Inactive,
    CleanScope::All,
];

pub fn scope_title(s: CleanScope) -> String {
    match s {
        CleanScope::ClosedEmpty => "Closed sessions, and empty ones older than 1 hour".into(),
        CleanScope::Inactive => format!("Sessions inactive for more than {INACTIVE_DAYS} days"),
        CleanScope::All => {
            "ALL sessions and workspaces, except the running ones and the current session".into()
        }
    }
}

fn plural(n: usize, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

/// `5 sessions \u{b7} 2 workspaces \u{b7} 300 events \u{b7} 12.0 MB (...)`.
pub fn counts_line(p: &CleanPlan) -> String {
    let c = &p.counts;
    let mut t = format!(
        "{} \u{b7} {} \u{b7} {} messages \u{b7} {} events \u{b7} {} on disk (screenshots {}, checkpoints {}, logs {})",
        plural(c.sessions, "session", "sessions"),
        plural(c.workspaces, "workspace", "workspaces"),
        c.messages,
        c.events,
        size(c.disk_bytes),
        size(c.screenshot_bytes),
        size(c.snapshot_bytes),
        size(c.log_bytes),
    );
    if c.skipped_running > 0 {
        t += &format!(" \u{b7} skipped: {} running", c.skipped_running);
    }
    if c.skipped_other > 0 {
        t += &format!(" \u{b7} kept: {} linked or unusual", c.skipped_other);
    }
    t
}

/// The state of the confirmation.
#[derive(Clone, Debug)]
pub struct CleanDialog {
    pub scope: CleanScope,
    pub plan: CleanPlan,
    /// Copy `synt.db` before deleting (default on).
    pub backup: bool,
    /// What was typed into the word field.
    pub typed: String,
    /// `$SYNT_HOME`, for the paragraphs.
    pub home: PathBuf,
}

impl CleanDialog {
    pub fn needs_word(&self) -> bool {
        self.scope == CleanScope::All
    }

    pub fn word_ok(&self) -> bool {
        !self.needs_word() || WORDS.contains(&self.typed.trim())
    }

    /// Cancel first (and selected), the backup toggle, then the destructive row.
    pub fn options(&self) -> Vec<(String, String)> {
        let n = self.plan.counts.sessions;
        let del = format!("Delete {} PERMANENTLY", plural(n, "session", "sessions"));
        vec![
            ("cancel".into(), "Cancel: delete nothing".into()),
            (
                "backup".into(),
                format!(
                    "[{}] Back up synt.db first (ALL sessions incl. tool output, never auto-deleted; not the artifact directories)",
                    if self.backup { "x" } else { " " }
                ),
            ),
            (
                "delete".into(),
                if self.word_ok() {
                    del
                } else {
                    format!("{del} (disabled: type delete or clean above first)")
                },
            ),
        ]
    }

    /// The paragraphs of the confirmation.
    pub fn lines(&self) -> Vec<String> {
        let c = &self.plan.counts;
        let home = sanitize_for_display(&self.home.display().to_string());
        let mut v = vec![
            format!("{}: {}", scope_title(self.scope), counts_line(&self.plan)),
            format!(
                "DELETED PERMANENTLY: the {} and their {} events, messages, tasks, tool calls, approvals and processes; their checkpoints and rewind snapshots ({}); their screenshots ({}) and logs ({}); and their directories {home}/sessions/<id> ({} on disk). After this /resume cannot open them and /rewind cannot restore their files.",
                plural(c.sessions, "session row", "session rows"),
                c.events,
                size(c.snapshot_bytes),
                size(c.screenshot_bytes),
                size(c.log_bytes),
                size(c.disk_bytes),
            ),
            format!(
                "Workspaces: {} listed only by these sessions disappear from the lists (a folder with a session left, or the open one, stays). No project file is touched.",
                c.workspaces
            ),
            "NOT touched: settings, auth and environment variables, the memory store, trust records and trust.key, the model cache, logs outside the sessions, the Node-format transcript copies under projects/, your project files, the current session, and every running session or one open in another window or process (skipped: running).".into(),
        ];
        v.push("Copies of these conversations remain in projects/*.jsonl and in the backup until you delete them.".into());
        v.push(if self.backup {
            format!(
                "Backup: synt.db is copied to {home}/backups/clean-<time>/synt.db (mode 0600) before anything is deleted, and nothing is deleted if the copy fails. It contains ALL sessions, not only these, including tool output, and is never auto-deleted; the artifact directories (snapshots, screenshots, logs) are NOT copied. Delete the backup yourself when you no longer need it."
            )
        } else {
            "No backup: nothing can bring these sessions back.".into()
        });
        v
    }
}

impl Live {
    /// Why `/clean` cannot start now.
    pub fn clean_blocker(&self) -> Option<&'static str> {
        if self.busy || self.compacting {
            Some("a task is running \u{b7} esc cancels it first")
        } else if !self.dialogs.is_empty()
            || !self.queue.is_empty()
            || self.rewinding
            || self.cleaning
        {
            Some(
                "approvals, questions or queued prompts are pending, or a /rewind or /clean is open: answer them first",
            )
        } else if self.engine.is_none() {
            Some("the session is still starting")
        } else {
            None
        }
    }

    /// A step was handed to a background job: its token, for [`Live::clean_timeout`].
    pub fn clean_job_start(&mut self) -> u64 {
        self.clean_seq += 1;
        self.clean_job = Some(self.clean_seq);
        self.clean_seq
    }

    /// The job `token` has not answered. A counting step (read-only) frees the flag. A deletion may
    /// still be running, so its flag stays: prompts and engine switches stay blocked until it reports.
    pub fn clean_timeout(&mut self, token: u64) {
        if self.clean_job != Some(token) {
            return;
        }
        self.clean_job = None;
        if self.clean_deleting {
            self.note(
                "clean: still working, it may be deleting. Prompts stay blocked until it reports; if it never does, restart the app",
                true,
            );
        } else {
            self.cleaning = false;
            self.note(
                "clean: counting did not answer after 10 minutes; nothing was deleted",
                true,
            );
        }
    }

    /// `/clean`.
    pub fn clean_command(&mut self, arg: &str) -> Option<Action> {
        if !arg.is_empty() {
            self.note(
                "/clean takes no arguments: it shows what each scope would delete, then asks",
                true,
            );
            return None;
        }
        if let Some(why) = self.clean_blocker() {
            self.note(format!("/clean: {why}"), true);
            return None;
        }
        self.cleaning = true;
        Some(Action::Clean)
    }

    /// The counts came back: the scope picker, docked above the prompt.
    pub fn clean_listed(&mut self, r: Result<Vec<(CleanScope, CleanPlan)>, String>) {
        self.clean_job = None;
        let rows = match r {
            Ok(v) => v,
            Err(e) => return self.clean_over(format!("/clean: {e}"), true),
        };
        if rows.iter().all(|(_, p)| p.counts.sessions == 0) {
            return self.clean_over(
                "clean: nothing to delete in any scope (the open session and running sessions are never touched)",
                false,
            );
        }
        let mut options = vec![("cancel".to_string(), "Cancel: delete nothing".to_string())];
        let mut headers = vec![(
            0,
            "Pick what to delete. Nothing is deleted yet: a confirmation follows, with exactly what goes and what stays.".to_string(),
        )];
        for (i, (s, p)) in rows.iter().enumerate() {
            headers.push((i + 1, scope_title(*s)));
            options.push((format!("{s:?}"), counts_line(p)));
        }
        self.clean_plans = rows;
        self.push_dialog(
            Dialog {
                kind: DialogKind::Picker {
                    kind: PickerKind::Clean,
                    title: "Clean: permanently delete stored sessions".into(),
                    headers,
                },
                options,
                sel: 0,
                armed_until: 0,
                engaged: false,
            },
            now_ms(),
        );
    }

    fn clean_over(&mut self, text: impl Into<String>, warning: bool) {
        self.cleaning = false;
        self.clean_plans.clear();
        self.note(text, warning);
    }

    /// Esc/close of a clean dialog deletes nothing and frees the prompt.
    pub(crate) fn clean_dismissed(&mut self) {
        self.cleaning = false;
        self.clean_plans.clear();
    }

    /// The front dialog is the confirmation and `key` is a row that acts without closing it (the backup
    /// toggle) or that is not allowed yet (delete without the word). True: handled, dialog stays.
    pub(crate) fn clean_stays(&mut self, key: &str) -> bool {
        let Some(Dialog {
            kind: DialogKind::Clean(c),
            options,
            ..
        }) = self.dialogs.first_mut()
        else {
            return false;
        };
        match key {
            "backup" => c.backup = !c.backup,
            "delete" if !c.word_ok() => {}
            _ => return false,
        }
        *options = c.options();
        true
    }

    /// A printable key (or Backspace) for the word field of the confirmation. Taken only when the dialog is
    /// armed AND the person engaged it: a key typed ahead, before the dialog appeared or was touched, goes
    /// nowhere, so type-ahead cannot spell the word. False: not for the field.
    pub fn clean_key(&mut self, text: Option<&str>, backspace: bool, now: i64) -> bool {
        let Some(Dialog {
            kind: DialogKind::Clean(c),
            options,
            engaged,
            armed_until,
            ..
        }) = self.dialogs.first_mut()
        else {
            return false;
        };
        if !c.needs_word() {
            return false;
        }
        let letters =
            text.filter(|t| !t.is_empty() && t.chars().all(|ch| ch.is_ascii_alphabetic()));
        if !backspace && letters.is_none() {
            return false; // Tab, arrows, digits...: the dialog's
        }
        if !*engaged || now < *armed_until {
            return true; // swallowed: not typed, not sent to the prompt
        }
        if backspace {
            c.typed.pop();
        } else if let Some(t) = letters
            && c.typed.len() + t.len() <= MAX_TYPED
        {
            c.typed.push_str(&t.to_ascii_lowercase());
        }
        *options = c.options();
        true
    }

    /// The person picked a scope, or answered the confirmation.
    pub(crate) fn clean_answer(&mut self, d: Dialog, key: &str) -> Option<Action> {
        match d.kind {
            DialogKind::Picker {
                kind: PickerKind::Clean,
                ..
            } => {
                let plans = std::mem::take(&mut self.clean_plans);
                let Some((scope, plan)) = plans.into_iter().find(|(s, _)| format!("{s:?}") == key)
                else {
                    self.clean_over("clean cancelled: nothing was deleted", false);
                    return None;
                };
                if plan.counts.sessions == 0 {
                    self.clean_over("clean: that scope has nothing to delete", false);
                    return None;
                }
                let home = self
                    .engine
                    .as_ref()
                    .map(|e| e.synthesis_home.clone())
                    .unwrap_or_default();
                let c = CleanDialog {
                    scope,
                    plan,
                    backup: true,
                    typed: String::new(),
                    home,
                };
                self.cleaning = true;
                self.push_dialog(
                    Dialog {
                        options: c.options(),
                        kind: DialogKind::Clean(Box::new(c)),
                        sel: 0,
                        armed_until: 0,
                        engaged: false,
                    },
                    now_ms(),
                );
                None
            }
            DialogKind::Clean(c) if key == "delete" && c.word_ok() => {
                // a task, a dialog or a queued line that appeared meanwhile: not now
                if self.busy || self.compacting || self.rewinding || !self.dialogs.is_empty() {
                    self.clean_over(
                        "clean not applied: a task or a dialog is pending now; nothing was deleted",
                        true,
                    );
                    return None;
                }
                let keep = self.engine.iter().map(|e| e.sid.clone()).collect();
                Some(Action::CleanApply {
                    scope: c.scope,
                    ids: c.plan.ids,
                    backup: c.backup,
                    keep,
                })
            }
            _ => {
                self.clean_over("clean cancelled: nothing was deleted", false);
                None
            }
        }
    }

    /// The deletion finished: the result row.
    pub fn clean_done(&mut self, r: Result<CleanResult, String>) {
        self.clean_job = None;
        self.cleaning = false;
        self.clean_deleting = false;
        match r {
            Err(e) => self.note(format!("clean: nothing was deleted: {e}"), true),
            Ok(o) => {
                let warn = !o.failed.is_empty();
                self.note(result_text(&o), warn);
            }
        }
    }
}

pub fn result_text(o: &CleanResult) -> String {
    let mut t = format!(
        "clean{}: deleted {} session{} ({} events), skipped {} running, kept {} unchanged or linked, failed {}, freed {}",
        if o.failed.is_empty() { "" } else { " PARTIAL" },
        o.deleted,
        if o.deleted == 1 { "" } else { "s" },
        o.events,
        o.skipped_running,
        o.skipped_other,
        o.failed.len(),
        size(o.freed_bytes),
    );
    if let Some(b) = &o.backup {
        t += &format!(
            "\n  backup: {}",
            sanitize_for_display(&b.display().to_string())
        );
    }
    for f in &o.failed {
        t += &format!("\n  failed {}", sanitize_for_display(f));
    }
    t
}

/// What `Action::Clean` computes off the UI thread: the three scopes' plans.
pub fn plans(
    home: &std::path::Path,
    keep: &[String],
) -> Result<Vec<(CleanScope, CleanPlan)>, String> {
    let store = Store::open(home, StoreOptions::default()).map_err(|e| e.to_string())?;
    let now = OffsetDateTime::now_utc();
    SCOPES
        .iter()
        .map(|s| {
            store
                .clean_plan(*s, keep, now)
                .map(|p| (*s, p))
                .map_err(|e| e.to_string())
        })
        .collect()
}

/// The deletion, off the UI thread. A panic in the worker becomes an `Err`, so `clean_done` always runs and
/// the flags never stick. A worker that never returns at all keeps the app blocked: that is the safe way to
/// fail for a deletion (restart the app to recover).
pub fn apply(
    home: &std::path::Path,
    scope: CleanScope,
    keep: &[String],
    ids: &[String],
    backup: bool,
) -> Result<CleanResult, String> {
    guarded(|| {
        let store = Store::open(home, StoreOptions::default()).map_err(|e| e.to_string())?;
        store
            .clean(scope, keep, OffsetDateTime::now_utc(), ids, backup)
            .map_err(|e| e.to_string())
    })
}

fn guarded(f: impl FnOnce() -> Result<CleanResult, String>) -> Result<CleanResult, String> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)).unwrap_or_else(|_| {
        Err("clean worker panicked (some sessions may already be deleted: run /clean again)".into())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use synthesis_session::CleanCounts;
    use synthesis_view::convo::Item;

    fn plan(n: usize) -> CleanPlan {
        CleanPlan {
            ids: (0..n).map(|i| format!("s{i}")).collect(),
            counts: CleanCounts {
                sessions: n,
                workspaces: 1,
                events: 9,
                messages: 4,
                disk_bytes: 2048,
                ..CleanCounts::default()
            },
        }
    }

    fn picker() -> Live {
        let mut l = Live::new();
        l.cleaning = true;
        l.clean_listed(Ok(SCOPES.iter().map(|s| (*s, plan(2))).collect()));
        l
    }

    fn confirm(scope: CleanScope) -> Live {
        let mut l = picker();
        assert!(l.answer(&format!("{scope:?}"), 0).is_none());
        l
    }

    #[test]
    fn the_picker_starts_on_cancel_lists_the_three_scopes_with_counts_and_esc_frees_the_prompt() {
        let mut l = picker();
        let d = &l.dialogs[0];
        assert_eq!(d.sel, 0);
        assert_eq!(d.options[0].0, "cancel");
        assert_eq!(d.options.len(), 4);
        assert!(
            d.options[1].1.contains("2 sessions")
                && d.options[1].1.contains("4 messages")
                && d.options[1].1.contains("9 events")
        );
        assert!(
            !d.answerable(),
            "Enter and digits wait for the person to engage it"
        );
        assert!(l.reject().is_none());
        assert!(l.dialogs.is_empty() && !l.cleaning);
    }

    #[test]
    fn nothing_to_delete_says_so_and_opens_nothing() {
        let mut l = Live::new();
        l.cleaning = true;
        l.clean_listed(Ok(SCOPES.iter().map(|s| (*s, plan(0))).collect()));
        assert!(l.dialogs.is_empty() && !l.cleaning);
        assert!(matches!(
            l.view.convo.items.last(),
            Some(Item::Notice { .. })
        ));
    }

    #[test]
    fn the_confirmation_defaults_to_cancel_and_type_ahead_cannot_delete_it() {
        let mut l = confirm(CleanScope::ClosedEmpty);
        let d = &l.dialogs[0];
        assert_eq!((d.sel, d.options[0].0.as_str()), (0, "cancel"));
        assert!(!d.answerable());
        assert!(
            l.pick(2, 0).is_none(),
            "armed: a key typed ahead answers nothing"
        );
        assert_eq!(l.reject(), None, "Esc cancels");
        assert!(l.dialogs.is_empty() && !l.cleaning);
        // Enter on the default Cancel deletes nothing either
        let mut l = confirm(CleanScope::ClosedEmpty);
        assert!(l.answer("cancel", 0).is_none() && !l.cleaning);
    }

    #[test]
    fn scope_all_needs_the_typed_word_and_type_ahead_cannot_spell_it() {
        let mut l = confirm(CleanScope::All);
        // not engaged: letters are swallowed
        for ch in "delete".chars() {
            assert!(l.clean_key(Some(&ch.to_string()), false, 10_000));
        }
        let DialogKind::Clean(c) = &l.dialogs[0].kind else {
            panic!()
        };
        assert_eq!(c.typed, "");
        // engaged but still arming: swallowed
        l.dialogs[0].engaged = true;
        l.dialogs[0].armed_until = 20_000;
        l.clean_key(Some("d"), false, 10_000);
        // engaged and armed: typed
        for ch in "delete".chars() {
            l.clean_key(Some(&ch.to_string()), false, 30_000);
        }
        let DialogKind::Clean(c) = &l.dialogs[0].kind else {
            panic!()
        };
        assert_eq!(c.typed, "delete");
        assert!(
            l.dialogs[0].options[2]
                .1
                .starts_with("Delete 2 sessions PERMANENTLY")
        );
        assert!(!l.dialogs[0].options[2].1.contains("disabled"));
    }

    #[test]
    fn delete_does_nothing_without_the_word_and_applies_with_it() {
        let mut l = confirm(CleanScope::All);
        assert!(l.dialogs[0].options[2].1.contains("disabled"));
        assert!(l.answer("delete", 0).is_none());
        assert_eq!(l.dialogs.len(), 1, "the dialog stays");
        for typed in ["Delete me", "del", "deletee", ""] {
            if let DialogKind::Clean(c) = &mut l.dialogs[0].kind {
                c.typed = typed.into();
            }
            assert!(
                l.answer("delete", 0).is_none() && l.dialogs.len() == 1,
                "{typed}"
            );
        }
        if let DialogKind::Clean(c) = &mut l.dialogs[0].kind {
            c.typed = "clean".into();
        }
        let Some(Action::CleanApply {
            scope, ids, backup, ..
        }) = l.answer("delete", 0)
        else {
            panic!()
        };
        assert_eq!((scope, ids.len(), backup), (CleanScope::All, 2, true));
    }

    #[test]
    fn the_other_scopes_need_no_word_and_the_backup_toggle_keeps_the_dialog() {
        let mut l = confirm(CleanScope::Inactive);
        assert!(l.dialogs[0].options[1].1.starts_with("[x]"));
        assert!(l.answer("backup", 0).is_none());
        assert!(l.dialogs[0].options[1].1.starts_with("[ ]"));
        let Some(Action::CleanApply { backup, scope, .. }) = l.answer("delete", 0) else {
            panic!()
        };
        assert!(!backup && scope == CleanScope::Inactive);
    }

    #[test]
    fn apply_is_refused_when_a_task_started_meanwhile_and_cleaning_blocks_engine_switches() {
        let mut l = confirm(CleanScope::ClosedEmpty);
        assert!(l.engine_switch_blocker().unwrap().contains("/clean"));
        l.busy = true;
        assert!(l.answer("delete", 0).is_none());
        assert!(!l.cleaning && l.engine_switch_blocker().is_none());
        assert!(matches!(
            l.view.convo.items.last(),
            Some(Item::Notice { warning: true, .. })
        ));
    }

    #[test]
    fn busy_pending_and_open_flows_refuse_clean_with_a_reason() {
        let setups: [fn(&mut Live); 4] = [
            |l| l.busy = true,
            |l| l.queue.push_back("x".into()),
            |l| l.rewinding = true,
            |l| l.cleaning = true,
        ];
        for setup in setups {
            let mut l = Live::new();
            setup(&mut l);
            assert!(l.clean_command("").is_none());
            assert!(matches!(
                l.view.convo.items.last(),
                Some(Item::Notice { warning: true, text }) if text.starts_with("/clean: ")
            ));
        }
        assert!(Live::new().clean_command("all").is_none());
        // a rewind is refused while a clean is open
        let mut l = Live::new();
        l.cleaning = true;
        assert!(l.rewind_blocker().is_some());
    }

    #[test]
    fn the_result_row_counts_names_failures_and_the_backup() {
        let mut l = Live::new();
        l.cleaning = true;
        l.clean_done(Ok(CleanResult {
            deleted: 3,
            events: 7,
            skipped_running: 1,
            failed: vec!["x: boom".into()],
            freed_bytes: 2048,
            backup: Some(PathBuf::from("/h/backups/clean-1/synt.db")),
            ..CleanResult::default()
        }));
        let Some(Item::Notice { warning, text }) = l.view.convo.items.last() else {
            panic!()
        };
        assert!(*warning && !l.cleaning);
        assert!(
            text.starts_with("clean PARTIAL: deleted 3 sessions (7 events), skipped 1 running")
        );
        assert!(
            text.contains("backup: /h/backups/clean-1/synt.db") && text.contains("failed x: boom")
        );
    }

    #[test]
    fn a_counting_job_that_never_answers_frees_the_flag_but_a_deleting_one_does_not() {
        let mut l = Live::new();
        l.cleaning = true;
        let t = l.clean_job_start();
        l.clean_timeout(t + 1);
        assert!(l.cleaning, "another job's token does nothing");
        l.clean_timeout(t);
        assert!(!l.cleaning);
        // a deletion: the flag stays until it reports
        l.cleaning = true;
        l.clean_deleting = true;
        let t = l.clean_job_start();
        l.clean_timeout(t);
        assert!(l.cleaning && l.engine_switch_blocker().is_some());
        l.clean_done(Ok(CleanResult::default()));
        assert!(!l.cleaning && !l.clean_deleting && l.engine_switch_blocker().is_none());
    }

    #[test]
    fn the_confirmation_text_names_what_goes_and_what_stays() {
        let mut c = CleanDialog {
            scope: CleanScope::All,
            plan: plan(2),
            backup: true,
            typed: String::new(),
            home: PathBuf::from("/h"),
        };
        let t = c.lines().join("\n");
        for w in [
            "DELETED PERMANENTLY",
            "checkpoints",
            "screenshots",
            "/h/sessions/<id>",
            "NOT touched",
            "settings",
            "memory store",
            "trust.key",
            "model cache",
            "projects/",
            "project files",
            "NOT copied",
        ] {
            assert!(t.contains(w), "{w}");
        }
        c.backup = false;
        assert!(c.lines().join("\n").contains("No backup"));
    }

    #[test]
    fn a_panicking_worker_still_reports_so_the_flags_are_freed() {
        let r = guarded(|| panic!("boom"));
        assert!(r.unwrap_err().contains("panicked"));
        let mut l = Live::new();
        l.cleaning = true;
        l.clean_deleting = true;
        l.clean_done(guarded(|| panic!("boom")));
        assert!(!l.cleaning && !l.clean_deleting);
    }
}
