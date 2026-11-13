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

#[derive(Default)]
pub struct MtimeTracker {
    base: Mutex<HashMap<String, BTreeMap<PathBuf, Stamp>>>,
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

fn walk(root: &Path) -> BTreeMap<PathBuf, Stamp> {
    let mut out = BTreeMap::new();
    let mut seen = 0usize;
    let mut stack = vec![root.to_path_buf()];
    while let Some(d) = stack.pop() {
        for e in std::fs::read_dir(&d).into_iter().flatten().flatten() {
            seen += 1;
            if seen > MAX_ENTRIES {
                return out;
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
            let now = walk(&ws.root);
            let base = self.base.lock().unwrap_or_else(|e| e.into_inner());
            let Some(before) = base.get(task_id) else {
                return vec![];
            };
            now.iter()
                .filter(|(p, st)| before.get(*p) != Some(st))
                .filter_map(|(p, _)| p.strip_prefix(&ws.root).ok())
                .map(|r| r.to_string_lossy().replace('\\', "/"))
                .collect()
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_and_rewritten_files_are_reported_untouched_ones_are_not() {
        let d = tempfile::tempdir().unwrap();
        let root = d.path();
        std::fs::write(root.join("old.txt"), "a").unwrap();
        std::fs::create_dir_all(root.join(".git")).unwrap();
        let before = walk(root);
        std::fs::write(root.join("x.sh"), "echo hi").unwrap();
        std::fs::write(root.join("old.txt"), "longer").unwrap();
        std::fs::write(root.join(".git/hook"), "x").unwrap();
        let after = walk(root);
        let mut changed: Vec<_> = after
            .iter()
            .filter(|(p, s)| before.get(*p) != Some(s))
            .map(|(p, _)| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        changed.sort();
        assert_eq!(changed, ["old.txt", "x.sh"]);
    }
}
