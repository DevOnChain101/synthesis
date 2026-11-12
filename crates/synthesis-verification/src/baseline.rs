//! Task-start snapshot of the workspace (verification.md §3b): what changed, and whether test or
//! check-definition files were weakened, no matter which tool changed them (an Edit, `sed -i`,
//! `rm`, `git checkout`). Shortcut: one content snapshot in git and non-git workspaces alike
//! (no `git diff` against the start commit); only test and definition files keep their text,
//! every other file is compared by size and mtime.

use crate::plan::{definition_view, is_definition_file};
use crate::weaken::{FileEdit, count_assertions, is_test_path};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

const SKIP_DIRS: [&str; 10] = [
    ".git",
    "node_modules",
    "target",
    ".synt",
    "dist",
    "build",
    "__pycache__",
    ".venv",
    "venv",
    ".next",
];
const MAX_FILES: usize = 100_000;
const MAX_TEXT: u64 = 1 << 20;

struct Entry {
    len: u64,
    mtime: Option<SystemTime>,
    /// Test, definition and referenced-script files up to 1 MiB.
    text: Option<String>,
}

pub struct Baseline {
    root: PathBuf,
    extra: HashSet<String>,
    files: HashMap<String, Entry>,
}

/// What differs from the baseline now.
#[derive(Default)]
pub(crate) struct Drift {
    /// Sorted workspace-relative paths added, removed or modified.
    pub changed: Vec<String>,
    /// Line diffs of modified test files (input of `weakened_tests`).
    pub edits: Vec<FileEdit>,
    /// `path (why)`: deleted, renamed or truncated test files.
    pub weakened: Vec<String>,
    /// Check-definition files that were added, removed or changed.
    pub defs: Vec<String>,
    /// The workspace was too big to scan completely.
    pub incomplete: bool,
}

impl Baseline {
    /// `extra`: further files to watch (scripts the planned checks invoke).
    pub fn take(root: &Path, extra: Vec<String>) -> Self {
        let extra: HashSet<String> = extra.into_iter().collect();
        let (files, _) = scan(root, &extra);
        Self {
            root: root.into(),
            extra,
            files,
        }
    }

    pub(crate) fn drift(&self) -> Drift {
        let (now, incomplete) = scan(&self.root, &self.extra);
        let mut d = Drift {
            incomplete,
            ..Default::default()
        };
        let text_changed = |a: &Entry, b: &Entry| match (&a.text, &b.text) {
            (Some(x), Some(y)) => x != y,
            _ => a.len != b.len || a.mtime != b.mtime,
        };
        for (p, old) in &self.files {
            let Some(new) = now.get(p) else {
                d.changed.push(p.clone());
                if is_test_path(p) {
                    d.weakened.push(format!("{p} (deleted or renamed)"));
                }
                if is_definition_file(p) || self.extra.contains(p) {
                    d.defs.push(p.clone());
                }
                continue;
            };
            if !text_changed(old, new) {
                continue;
            }
            d.changed.push(p.clone());
            let (Some(o), Some(n)) = (&old.text, &new.text) else {
                continue;
            };
            if is_test_path(p) {
                let (ol, nl) = (o.lines().count(), n.lines().count());
                if ol >= 4 && nl * 2 < ol {
                    d.weakened
                        .push(format!("{p} (truncated from {ol} to {nl} lines)"));
                }
                let e = line_diff(p, o, n);
                // Changing the expected value of an existing assertion is how a test gets made to
                // pass: it needs a human even though an assertion is still there.
                if count_assertions(&e.removed) > 0 {
                    d.weakened
                        .push(format!("{p} (existing assertions changed or removed)"));
                }
                d.edits.push(e);
            }
            if (is_definition_file(p) && definition_view(p, o) != definition_view(p, n))
                || (self.extra.contains(p) && o != n)
            {
                d.defs.push(p.clone());
            }
        }
        for (p, new) in now.iter().filter(|(p, _)| !self.files.contains_key(*p)) {
            d.changed.push(p.clone());
            if let (true, Some(n)) = (is_test_path(p), &new.text) {
                d.edits.push(line_diff(p, "", n)); // a new test file is all additions
            }
            if is_definition_file(p) || self.extra.contains(p) {
                d.defs.push(p.clone());
            }
        }
        d.changed.sort();
        d.defs.sort();
        d.weakened.sort();
        d
    }
}

/// Added and removed lines as a multiset difference (order and moves do not count).
fn line_diff(path: &str, old: &str, new: &str) -> FileEdit {
    let mut left: HashMap<&str, i64> = HashMap::new();
    for l in old.lines() {
        *left.entry(l).or_default() += 1;
    }
    let mut e = FileEdit {
        path: path.into(),
        ..Default::default()
    };
    for l in new.lines() {
        match left.get_mut(l) {
            Some(n) if *n > 0 => *n -= 1,
            _ => e.added.push(l.into()),
        }
    }
    for l in old.lines() {
        if let Some(n) = left.get_mut(l).filter(|n| **n > 0) {
            *n -= 1;
            e.removed.push(l.into());
        }
    }
    e
}

fn scan(root: &Path, extra: &HashSet<String>) -> (HashMap<String, Entry>, bool) {
    let mut out = HashMap::new();
    let mut stack = vec![(root.to_path_buf(), String::new())];
    while let Some((dir, rel)) = stack.pop() {
        let Ok(rd) = fs::read_dir(&dir) else { continue };
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            let Ok(ft) = e.file_type() else { continue };
            let rel = if rel.is_empty() {
                name.clone()
            } else {
                format!("{rel}/{name}")
            };
            if ft.is_dir() {
                if !SKIP_DIRS.contains(&name.as_str()) {
                    stack.push((e.path(), rel));
                }
            } else if ft.is_file() {
                if out.len() >= MAX_FILES {
                    return (out, true);
                }
                let Ok(m) = e.metadata() else { continue };
                let watched =
                    is_test_path(&rel) || is_definition_file(&rel) || extra.contains(&rel);
                let text = (watched && m.len() <= MAX_TEXT)
                    .then(|| fs::read_to_string(e.path()).ok())
                    .flatten();
                out.insert(
                    rel,
                    Entry {
                        len: m.len(),
                        mtime: m.modified().ok(),
                        text,
                    },
                );
            }
        }
    }
    (out, false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sees_shell_style_changes_to_tests_and_definitions() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path();
        fs::create_dir(p.join("tests")).unwrap();
        fs::write(
            p.join("tests/a.test.js"),
            "test('a',()=>{assert(1)})\nl2\nl3\nl4\nl5\n",
        )
        .unwrap();
        fs::write(p.join("tests/b.test.js"), "x\n").unwrap();
        fs::write(p.join("tests/c.test.js"), "test('c',()=>{})\n").unwrap();
        fs::write(
            p.join("package.json"),
            r#"{"scripts":{"test":"node --test"},"x":1}"#,
        )
        .unwrap();
        fs::write(p.join("src.js"), "1").unwrap();
        let b = Baseline::take(p, vec![]);
        assert!(b.drift().changed.is_empty());
        fs::write(p.join("tests/a.test.js"), "l2\n").unwrap();
        fs::remove_file(p.join("tests/b.test.js")).unwrap();
        fs::rename(p.join("tests/c.test.js"), p.join("tests/d.test.js")).unwrap();
        fs::write(
            p.join("package.json"),
            r#"{"scripts":{"test":"true"},"x":2}"#,
        )
        .unwrap();
        fs::write(p.join("src.js"), "22").unwrap();
        let d = b.drift();
        assert_eq!(d.defs, ["package.json"]);
        assert!(d.changed.contains(&"src.js".to_string()));
        let w = d.weakened.join("|");
        assert!(
            w.contains("b.test.js (deleted") && w.contains("c.test.js (deleted"),
            "{w}"
        );
        assert!(w.contains("a.test.js (truncated"), "{w}");
        assert!(
            d.edits
                .iter()
                .any(|e| e.path == "tests/a.test.js" && !e.removed.is_empty())
        );
    }
}
