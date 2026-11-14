//! `ChangeTracker` over file metadata: which files a task changed through tools whose effects the
//! harness cannot read from their input (`Bash`). The permission engine uses the list to ask before
//! running what the agent wrote (including files a shell, a compiler or a download created).
//! shortcut: compares `(len, mtime)`, so a rewrite that keeps both is missed, and `.git`, `target`,
//! `node_modules` and similar are not walked; more than 50000 entries are not followed. Upgrade
//! by hashing, or by a filesystem watcher.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::SystemTime;

use synthesis_core::WorkspaceContext;
use synthesis_harness::{BoxFut, ChangeTracker};

type Stamp = (u64, Option<SystemTime>);
/// A walk's files, and whether it saw everything.
type Snapshot = (BTreeMap<PathBuf, Stamp>, bool);

#[derive(Default)]
pub struct MtimeTracker {
    base: Mutex<HashMap<String, Snapshot>>,
}

const SKIP: &[&str] = &[
    ".git",
    "target",
    "node_modules",
    ".venv",
    "venv",
    "__pycache__",
    ".next",
];
const MAX_ENTRIES: usize = 50_000;

/// Reported as a changed path when a walk was cut short: what changed cannot be known, so the gate must run.
const INCOMPLETE: &str = "(workspace too large to track completely)";

fn walk(root: &Path) -> Snapshot {
    walk_max(root, MAX_ENTRIES)
}

/// The files with their stamps, and whether the walk saw everything (false: it hit `max` entries).
fn walk_max(root: &Path, max: usize) -> Snapshot {
    let mut out = BTreeMap::new();
    let mut seen = 0usize;
    let mut stack = vec![root.to_path_buf()];
    while let Some(d) = stack.pop() {
        for e in std::fs::read_dir(&d).into_iter().flatten().flatten() {
            seen += 1;
            if seen > max {
                return (out, false);
            }
            let Ok(ft) = e.file_type() else { continue };
            if ft.is_dir() {
                if !SKIP.contains(&e.file_name().to_string_lossy().as_ref()) {
                    stack.push(e.path());
                }
            } else if ft.is_file()
                && let Ok(m) = e.metadata()
            {
                out.insert(e.path(), (m.len(), m.modified().ok()));
            }
        }
    }
    (out, true)
}

fn diff(
    root: &Path,
    before: &BTreeMap<PathBuf, Stamp>,
    now: &BTreeMap<PathBuf, Stamp>,
    complete: bool,
) -> Vec<String> {
    let mut out: Vec<String> = now
        .iter()
        .filter(|(p, st)| before.get(*p) != Some(st))
        .filter_map(|(p, _)| p.strip_prefix(root).ok())
        .map(|r| r.to_string_lossy().replace('\\', "/"))
        .collect();
    if !complete {
        out.push(INCOMPLETE.into());
    }
    out
}

impl ChangeTracker for MtimeTracker {
    fn begin<'a>(&'a self, ws: &'a WorkspaceContext, task_id: &'a str) -> BoxFut<'a, ()> {
        Box::pin(async move {
            let snap = walk(&ws.root);
            self.base
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(task_id.to_string(), snap);
        })
    }

    fn changed<'a>(
        &'a self,
        ws: &'a WorkspaceContext,
        task_id: &'a str,
    ) -> BoxFut<'a, Vec<String>> {
        Box::pin(async move {
            let (now, now_full) = walk(&ws.root);
            let base = self.base.lock().unwrap_or_else(|e| e.into_inner());
            let Some((before, before_full)) = base.get(task_id) else {
                return vec![];
            };
            diff(&ws.root, before, &now, *before_full && now_full)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_walk_cut_short_reports_a_change_so_the_gate_still_runs() {
        let d = tempfile::tempdir().unwrap();
        for i in 0..20 {
            std::fs::write(d.path().join(format!("f{i}")), "x").unwrap();
        }
        let (full, ok) = walk_max(d.path(), 100);
        assert!(ok && diff(d.path(), &full, &full, ok).is_empty());
        let (cut, ok) = walk_max(d.path(), 5);
        assert!(!ok);
        assert_eq!(diff(d.path(), &cut, &cut, ok), [INCOMPLETE]);
    }

    #[test]
    fn new_and_rewritten_files_are_reported_untouched_ones_are_not() {
        let d = tempfile::tempdir().unwrap();
        let root = d.path();
        std::fs::write(root.join("old.txt"), "a").unwrap();
        std::fs::create_dir_all(root.join(".git")).unwrap();
        let (before, _) = walk(root);
        std::fs::write(root.join("x.sh"), "echo hi").unwrap();
        std::fs::write(root.join("old.txt"), "longer").unwrap();
        std::fs::write(root.join(".git/hook"), "x").unwrap();
        let (after, _) = walk(root);
        let mut changed: Vec<_> = after
            .iter()
            .filter(|(p, s)| before.get(*p) != Some(s))
            .map(|(p, _)| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        changed.sort();
        assert_eq!(changed, ["old.txt", "x.sh"]);
    }
}
