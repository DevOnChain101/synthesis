//! `/rewind`: put the files the session's runs changed back to their checkpoints. Plain Rust, no
//! gpui. A dry run first, always: the list of checkpoints, then the exact files a rewind would
//! overwrite or delete, then an explicit confirmation in the dock. The decisions (what is inside
//! the workspace, what is a secret, what changed since the run) are `synthesis_app::rewind`'s, the
//! same code `synt rewind` runs; this module only presents them and asks.
//!
//! Like `synt rewind` it puts back files only: the conversation history stays as it is.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use synthesis_app::rewind::{self, Entry, Step};
use synthesis_harness::sanitize_for_display;
use synthesis_view::agent::CmdState;

use crate::live::{Action, Dialog, DialogKind, Live, PickerKind, now_ms};

/// Rows of a group shown before "Show more" (the chat's clamp).
pub const GROUP_LINES: usize = 5;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CheckpointRow {
    pub name: String,
    /// `2026-11-09 14:02 UTC`.
    pub when: String,
    /// Messages of the conversation when it was taken.
    pub after_message: u64,
    pub files: Vec<String>,
}

impl CheckpointRow {
    pub fn label(&self) -> String {
        let files = match self.files.as_slice() {
            [one] => one.clone(),
            v => format!("{} files", v.len()),
        };
        format!(
            "{}  \u{b7}  {}  \u{b7}  after message {}  \u{b7}  {files}",
            self.name, self.when, self.after_message
        )
    }
}

/// What a rewind of `checkpoint` (None: every checkpoint) would do now.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Preview {
    pub checkpoint: Option<String>,
    pub entries: Vec<Entry>,
}

/// One block of rows in the confirmation: a title and the rows, CLI style.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Group {
    pub title: String,
    pub rows: Vec<String>,
}

pub fn size(n: u64) -> String {
    match n {
        0..=1023 => format!("{n} B"),
        1024..=1_048_575 => format!("{:.1} KB", n as f64 / 1024.),
        _ => format!("{:.1} MB", n as f64 / 1_048_576.),
    }
}

impl Preview {
    /// (files overwritten, files deleted, files left alone)
    pub fn counts(&self) -> (usize, usize, usize) {
        let n = |f: fn(&Step) -> bool| self.entries.iter().filter(|e| f(&e.step)).count();
        (
            n(|s| matches!(s, Step::Restore { .. })),
            n(|s| matches!(s, Step::Remove)),
            n(|s| matches!(s, Step::Skip { .. })),
        )
    }

    pub fn actionable(&self) -> bool {
        let (r, d, _) = self.counts();
        r + d > 0
    }

    /// `restore`/`remove`/`skip` rows the way `synt rewind` prints them, grouped.
    pub fn groups(&self) -> Vec<Group> {
        let mut g = [
            ("Will be overwritten with the checkpoint", vec![]),
            (
                "Will be deleted (the checkpoint recorded them as absent before the run)",
                vec![],
            ),
            ("Will NOT be touched", vec![]),
        ];
        for e in &self.entries {
            let (i, row) = match &e.step {
                Step::Restore { .. } => (0, format!("restore  {}  ({})", e.shown, size(e.size))),
                Step::Remove => (1, format!("remove   {}  ({} now)", e.shown, size(e.size))),
                Step::Skip { reason, .. } => (2, format!("skip     {}: {reason}", e.shown)),
            };
            g[i].1.push(row);
        }
        g.into_iter()
            .filter(|(_, rows)| !rows.is_empty())
            .map(|(t, rows)| Group {
                title: format!("{t} ({})", rows.len()),
                rows,
            })
            .collect()
    }

    /// What the confirmation says will be written and deleted, and that it is final.
    pub fn warning(&self) -> String {
        let (r, d, _) = self.counts();
        format!(
            "Apply writes {r} file{} back to its checkpoint contents and deletes {d} file{}: the checkpoint recorded that they did not exist before the run's first write (that record is the only proof the run created them). \
             It cannot be undone: what is in those files now is not saved anywhere, so the checkpoint is the only copy you can go back to. \
             The snapshots are checked against the hashes stored with the checkpoint, but both live in the session store: a program that can write that store can forge them. \
             The conversation is not rewound.",
            if r == 1 { "" } else { "s" },
            if d == 1 { "" } else { "s" },
        )
    }
}

/// The state of the confirmation dialog.
#[derive(Clone, Debug)]
pub struct RewindDialog {
    pub preview: Preview,
    /// Groups opened with "Show more".
    pub expanded: HashSet<usize>,
}

/// What an apply did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Outcome {
    pub restored: usize,
    pub removed: usize,
    pub skipped: usize,
    /// `path: reason`
    pub failed: Vec<String>,
    /// Files written or deleted (the Diff and Files tabs forget these).
    pub changed: Vec<PathBuf>,
}

impl Outcome {
    pub fn text(&self) -> String {
        let done = self.restored + self.removed;
        let mut t = format!(
            "rewind{}: restored {}, removed {}, left alone {}, failed {}",
            if self.failed.is_empty() {
                ""
            } else if done > 0 {
                " PARTIAL (the other files are already written; run /rewind again for the rest)"
            } else {
                " FAILED (nothing was written)"
            },
            self.restored,
            self.removed,
            self.skipped,
            self.failed.len()
        );
        for f in &self.failed {
            t += &format!("\n  failed {f}");
        }
        t
    }
}

/// Where the session store and the person's home are: the desktop never allows a session whose
/// workspace root is the home directory or a filesystem root.
#[derive(Clone, Debug)]
pub struct Homes {
    pub synthesis: PathBuf,
    pub user: Option<PathBuf>,
}

/// The person's home directory, or None when it is not known (only `SYNT_HOME` set gives an empty
/// path): `synthesis_app::rewind::load` then refuses, it does not skip its home check.
pub fn user_home(env: &synthesis_config::Env) -> Option<PathBuf> {
    synthesis_app::homes(env)
        .ok()
        .map(|h| h.user)
        .filter(|p| !p.as_os_str().is_empty())
}

/// A rewind step that has not answered after this long frees the prompt and the engine again.
pub const JOB_TIMEOUT_MS: u64 = 120_000;

fn load(h: &Homes, sid: &str) -> Result<rewind::Loaded, String> {
    rewind::load(&h.synthesis, h.user.as_deref(), sid, false)
}

/// The picker's key for "every checkpoint": a NUL cannot be part of a checkpoint name.
pub const ALL: &str = "\u{0}all";

pub fn list(h: &Homes, sid: &str) -> Result<Vec<CheckpointRow>, String> {
    Ok(load(h, sid)?
        .cps
        .into_iter()
        .map(|c| CheckpointRow {
            when: c.created_at.get(..16).map_or_else(
                || sanitize_for_display(&c.created_at),
                |t| format!("{} UTC", t.replace('T', " ")),
            ),
            name: sanitize_for_display(&c.name),
            after_message: c.message_count,
            files: c
                .files
                .iter()
                .map(|f| sanitize_for_display(&f.path))
                .collect(),
        })
        .collect())
}

/// The dry run. Never forces: a file changed since the run stays "left alone".
pub fn preview(h: &Homes, sid: &str, checkpoint: Option<String>) -> Result<Preview, String> {
    let entries = rewind::plan(&load(h, sid)?, checkpoint.as_deref(), false)?;
    Ok(Preview {
        checkpoint,
        entries,
    })
}

/// Writes what the person confirmed, if it is still exactly that.
pub fn apply(h: &Homes, sid: &str, p: &Preview) -> Result<Outcome, String> {
    let done = rewind::apply_confirmed(&load(h, sid)?, p.checkpoint.as_deref(), &p.entries)?;
    let mut o = Outcome::default();
    for (e, res) in done {
        match (&e.step, res) {
            (Step::Skip { .. }, _) => o.skipped += 1,
            (_, Err(why)) => o.failed.push(why),
            (Step::Restore { .. }, Ok(())) => {
                o.restored += 1;
                o.changed.push(e.path);
            }
            (_, Ok(())) => {
                o.removed += 1;
                o.changed.push(e.path);
            }
        }
    }
    Ok(o)
}

impl Live {
    /// Why a rewind cannot start or apply now: files must not change under a running task.
    pub fn rewind_blocker(&self) -> Option<&'static str> {
        if self.busy || self.compacting {
            Some("a task is running \u{b7} esc cancels it first")
        } else if !self.dialogs.is_empty()
            || !self.queue.is_empty()
            || self.rewinding
            || self.cleaning
        {
            Some(
                "approvals, questions or queued prompts are pending, or a rewind is open: answer them first",
            )
        } else if !self.agents.is_empty()
            || self
                .view
                .panes
                .commands
                .iter()
                .any(|c| c.state == CmdState::Running)
        {
            Some(
                "a background command or sub-agent is still running: files must not change under it",
            )
        } else if self.engine.is_none() {
            Some("the session is still starting")
        } else {
            None
        }
    }

    /// Why the engine must not be switched or restarted now (`/resume`, `/model`, `/new`, `/close`,
    /// `/open`, a restart): a rewind is open, and its answer belongs to this session.
    pub fn engine_switch_blocker(&self) -> Option<&'static str> {
        if self.cleaning {
            Some("a /clean is open or deleting: finish or cancel it first")
        } else {
            self.rewinding
                .then_some("a /rewind is open or writing: finish or cancel it first")
        }
    }

    /// A step (list, dry run, apply) was handed to a background job: its token, for [`Live::rewind_timeout`].
    pub fn rewind_job_start(&mut self) -> u64 {
        self.rewind_seq += 1;
        self.rewind_job = Some(self.rewind_seq);
        self.rewind_seq
    }

    /// The job `token` never answered: free the flag so the prompt and engine switches work again.
    /// The apply may still be writing under its lock; the note says to look before trying again.
    pub fn rewind_timeout(&mut self, token: u64) {
        if self.rewind_job == Some(token) {
            self.rewind_job = None;
            self.rewinding = false;
            self.note(
                "rewind: no answer after 2 minutes. Files may still be being written: check the Files tab, then run /rewind again",
                true,
            );
        }
    }

    /// `/rewind`.
    pub fn rewind_command(&mut self, arg: &str) -> Option<Action> {
        if !arg.is_empty() {
            self.note(
                "/rewind takes no arguments: it lists the checkpoints, shows a dry run, then asks. Files only, the conversation stays",
                true,
            );
            return None;
        }
        if let Some(why) = self.rewind_blocker() {
            self.note(format!("/rewind: {why}"), true);
            return None;
        }
        self.rewinding = true;
        Some(Action::Rewind)
    }

    /// The listing came back: the picker, docked above the prompt.
    pub fn rewind_listed(&mut self, r: Result<Vec<CheckpointRow>, String>) {
        self.rewind_job = None;
        let rows = match r {
            Ok(v) if v.is_empty() => {
                return self.rewind_over("/rewind: this session has no checkpoint yet (one is taken before the first write to each file)", true);
            }
            Ok(v) => v,
            Err(e) => return self.rewind_over(format!("/rewind: {e}"), true),
        };
        let mut options = vec![(
            ALL.to_string(),
            format!("Every checkpoint of this session ({})", rows.len()),
        )];
        options.extend(rows.iter().map(|c| (c.name.clone(), c.label())));
        self.rewind_dialog(Dialog {
            kind: DialogKind::Picker {
                kind: PickerKind::Rewind,
                title: "Rewind files: pick a checkpoint (nothing is written yet)".into(),
                headers: vec![(
                    0,
                    "Next you see exactly which files would be overwritten or deleted. Nothing changes until you confirm that. The conversation is not rewound.".into(),
                )],
            },
            options,
            sel: 0,
            armed_until: 0,
            engaged: false,
        });
    }

    /// The dry run came back: the file list and the confirmation. Cancel is last (Esc picks it) and selected.
    pub fn rewind_previewed(&mut self, r: Result<Preview, String>) {
        self.rewind_job = None;
        let p = match r {
            Ok(p) if !p.actionable() => {
                let rows: Vec<String> = p.groups().into_iter().flat_map(|g| g.rows).collect();
                return self.rewind_over(
                    format!("rewind: nothing to put back\n{}", rows.join("\n")),
                    false,
                );
            }
            Ok(p) => p,
            Err(e) => return self.rewind_over(format!("/rewind: {e}"), true),
        };
        let (r, d, _) = p.counts();
        self.rewind_dialog(Dialog {
            options: vec![
                (
                    "apply".into(),
                    format!("Apply: overwrite {r}, delete {d} (cannot be undone)"),
                ),
                ("cancel".into(), "Cancel: write nothing".into()),
            ],
            kind: DialogKind::Rewind(Box::new(RewindDialog {
                preview: p,
                expanded: HashSet::new(),
            })),
            sel: 1,
            armed_until: 0,
            engaged: false,
        });
    }

    fn rewind_dialog(&mut self, d: Dialog) {
        self.rewinding = true;
        self.push_dialog(d, now_ms());
    }

    /// The flow ended without writing: say why.
    fn rewind_over(&mut self, text: impl Into<String>, warning: bool) {
        self.rewinding = false;
        self.note(text, warning);
    }

    /// The person picked a checkpoint, or answered the confirmation.
    pub(crate) fn rewind_answer(&mut self, d: Dialog, key: &str) -> Option<Action> {
        match d.kind {
            DialogKind::Picker {
                kind: PickerKind::Rewind,
                ..
            } => {
                self.rewinding = true;
                Some(Action::RewindPreview((key != ALL).then(|| key.to_string())))
            }
            DialogKind::Rewind(r) if key == "apply" => {
                // answers and prompts queued behind the dialog are pending work too
                if self.busy || self.compacting || !self.dialogs.is_empty() {
                    self.rewind_over(
                        "rewind not applied: a task or a dialog is pending now; nothing was written",
                        true,
                    );
                    return None;
                }
                Some(Action::RewindApply(r.preview))
            }
            _ => {
                self.rewind_over("rewind cancelled: nothing was written", false);
                None
            }
        }
    }

    /// Esc/close of the rewind pickers writes nothing and frees the prompt.
    pub(crate) fn rewind_dismissed(&mut self) {
        self.rewinding = false;
    }

    /// A group's "Show more" / "Show less".
    pub fn rewind_toggle(&mut self, group: usize) {
        if let Some(Dialog {
            kind: DialogKind::Rewind(r),
            ..
        }) = self.dialogs.first_mut()
            && !r.expanded.remove(&group)
        {
            r.expanded.insert(group);
        }
    }

    /// The apply finished: the result row, and the Diff tab forgets the files that are back.
    pub fn rewind_done(&mut self, r: Result<Outcome, String>) {
        self.rewind_job = None;
        self.rewinding = false;
        match r {
            Err(e) => self.note(format!("rewind: {e}"), true),
            Ok(o) => {
                let cwd = PathBuf::from(&self.cwd);
                self.view.panes.files.retain(|f| {
                    let p = Path::new(&f.path);
                    let abs = if p.is_absolute() {
                        p.to_path_buf()
                    } else {
                        cwd.join(p)
                    };
                    !o.changed.contains(&abs)
                });
                self.note(o.text(), !o.failed.is_empty());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::live::Dialog;
    use synthesis_view::convo::Item;

    fn entry(name: &str, step: Step, size: u64) -> Entry {
        Entry {
            path: PathBuf::from(format!("/ws/{name}")),
            shown: format!("/ws/{name}"),
            step,
            size,
            state: "x".into(),
        }
    }

    fn preview() -> Preview {
        let mut entries: Vec<Entry> = (0..7)
            .map(|i| {
                let step = Step::Restore {
                    artifact_id: "a".into(),
                    sha256: None,
                };
                entry(&format!("f{i}"), step, 2048)
            })
            .collect();
        entries.push(entry("new.txt", Step::Remove, 7));
        let skip = Step::Skip {
            reason: "the snapshot is not the exact file (secrets were masked)".into(),
            forceable: false,
        };
        entries.push(entry(".env", skip, 0));
        Preview {
            checkpoint: None,
            entries,
        }
    }

    fn confirm_live() -> Live {
        let mut l = Live::new();
        l.rewind_previewed(Ok(preview()));
        l
    }

    #[test]
    fn the_dry_run_rows_are_grouped_with_action_path_and_size() {
        let p = preview();
        assert_eq!(p.counts(), (7, 1, 1));
        let g = p.groups();
        assert_eq!(g.len(), 3);
        assert_eq!(g[0].rows.len(), 7);
        assert_eq!(g[0].rows[0], "restore  /ws/f0  (2.0 KB)");
        assert_eq!(g[1].rows[0], "remove   /ws/new.txt  (7 B now)");
        assert!(g[2].rows[0].starts_with("skip     /ws/.env: the snapshot is not the exact file"));
        let w = p.warning();
        assert!(
            w.contains("7 files") && w.contains("deletes 1 file") && w.contains("cannot be undone")
        );
    }

    #[test]
    fn busy_pending_work_and_arguments_refuse_with_a_reason_and_start_nothing() {
        let setups: [fn(&mut Live); 3] = [
            |l| l.busy = true,
            |l| l.queue.push_back("x".into()),
            |l| l.rewinding = true,
        ];
        for setup in setups {
            let mut l = Live::new();
            setup(&mut l);
            assert!(l.rewind_command("").is_none());
            assert!(matches!(
                l.view.convo.items.last(),
                Some(Item::Notice { warning: true, text }) if text.starts_with("/rewind: ")
            ));
        }
        let mut l = Live::new();
        assert!(l.rewind_command("3").is_none());
    }

    #[test]
    fn the_confirmation_defaults_to_cancel_and_type_ahead_cannot_apply_it() {
        let mut l = confirm_live();
        let d: &Dialog = &l.dialogs[0];
        assert_eq!(d.options[d.sel].0, "cancel");
        assert!(
            !d.answerable(),
            "Enter and digits are swallowed until the person engages it"
        );
        assert!(
            l.pick(0, 0).is_none(),
            "a key typed while it arms answers nothing"
        );
        let k = l.reject().unwrap();
        assert_eq!(k, "cancel", "Esc is the default");
        assert!(l.answer(&k, 0).is_none());
        assert!(!l.rewinding && l.dialogs.is_empty());
    }

    #[test]
    fn apply_is_refused_when_a_task_started_meanwhile() {
        let mut l = confirm_live();
        l.busy = true;
        assert!(l.answer("apply", 0).is_none());
        assert!(!l.rewinding);
        let mut l = confirm_live();
        assert!(matches!(l.answer("apply", 0), Some(Action::RewindApply(_))));
    }

    #[test]
    fn a_rewind_blocks_engine_switches_background_work_blocks_a_rewind_and_all_has_a_sentinel() {
        let mut l = Live::new();
        assert!(l.engine_switch_blocker().is_none());
        l.rewinding = true;
        assert!(l.engine_switch_blocker().unwrap().contains("/rewind"));
        let mut l = Live::new();
        l.agents.push("sub-1".into());
        assert!(l.rewind_blocker().unwrap().contains("sub-agent"));
        assert!(ALL.contains('\u{0}'));
    }

    #[test]
    fn a_job_that_never_answers_frees_the_flag_once_and_only_for_its_own_token() {
        let mut l = Live::new();
        l.rewinding = true;
        let t = l.rewind_job_start();
        l.rewind_timeout(t + 1);
        assert!(l.rewinding, "another job's token does nothing");
        l.rewind_timeout(t);
        assert!(!l.rewinding && l.engine_switch_blocker().is_none());
        assert!(matches!(
            l.view.convo.items.last(),
            Some(Item::Notice { warning: true, .. })
        ));
        // an answer that arrived in time cancels the timeout
        l.rewinding = true;
        let t = l.rewind_job_start();
        l.rewind_done(Ok(Outcome::default()));
        l.rewinding = true;
        l.rewind_timeout(t);
        assert!(l.rewinding);
    }

    #[test]
    fn only_synt_home_set_gives_no_user_home() {
        let only = synthesis_config::Env::from_pairs([("SYNT_HOME", "/tmp/x".to_string())]);
        assert_eq!(user_home(&only), None);
        let with = synthesis_config::Env::from_pairs([("HOME", "/Users/a".to_string())]);
        assert_eq!(user_home(&with), Some(PathBuf::from("/Users/a")));
    }

    #[test]
    fn a_partial_apply_says_so_and_names_what_failed() {
        let o = Outcome {
            restored: 1,
            failed: vec!["/ws/x: boom".into()],
            ..Outcome::default()
        };
        assert!(o.text().starts_with("rewind PARTIAL"));
        let o = Outcome {
            failed: vec!["/ws/x: boom".into()],
            ..Outcome::default()
        };
        assert!(o.text().starts_with("rewind FAILED"));
    }

    #[test]
    fn the_result_row_counts_and_names_each_failure() {
        let mut l = Live::new();
        l.rewinding = true;
        let o = Outcome {
            restored: 2,
            removed: 1,
            skipped: 1,
            failed: vec!["/ws/x: permission denied".into()],
            changed: vec![],
        };
        l.rewind_done(Ok(o));
        let Some(Item::Notice { warning, text }) = l.view.convo.items.last() else {
            panic!()
        };
        assert!(*warning);
        assert!(text.starts_with("rewind PARTIAL"), "{text}");
        assert!(text.contains("restored 2, removed 1, left alone 1, failed 1"));
        assert!(text.contains("failed /ws/x: permission denied"));
        assert!(!l.rewinding);
    }
}
