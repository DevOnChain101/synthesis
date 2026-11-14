//! Task-start snapshot of the workspace (verification.md §3b): what changed, and whether test or
//! check-definition files were weakened, no matter which tool changed them (an Edit, `sed -i`,
//! `rm`, `git checkout`). One content snapshot in git and non-git workspaces alike (no `git diff`
//! against the start commit). Test, definition, golden and inline-test files keep their text;
//! every other file is compared by size and mtime only, so a same-size edit that restores the
//! mtime is not seen (shortcut: a hash of every file would read the whole workspace).

use crate::plan::{definition_view, is_definition_file};
use crate::weaken::{
    FileEdit, count_assertions, count_table_rows, count_test_cases, is_golden_path, is_test_path,
};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// Never walked, at any depth (caches and vendored code).
const SKIP_DIRS: [&str; 11] = [
    ".git",
    "node_modules",
    ".synt",
    "__pycache__",
    ".venv",
    "venv",
    ".next",
    ".pytest_cache",
    ".mypy_cache",
    ".ruff_cache",
    ".tox",
];
/// Build output, skipped only at the workspace root so a `tests/build` dir stays visible.
const ROOT_SKIP_DIRS: [&str; 3] = ["build", "dist", ".gradle"];
/// Directories whose entries decide what a check executes; recorded although not walked.
const TOOL_DIRS: [&str; 3] = ["node_modules/.bin", ".venv/bin", "venv/bin"];
const MAX_FILES: usize = 100_000;
const MAX_TEXT: u64 = 1 << 20;
const MAX_RS_TEXT: u64 = 256 << 10;

#[derive(Serialize, Deserialize)]
struct Entry {
    len: u64,
    mtime: Option<SystemTime>,
    /// Test, definition, golden, referenced-script and inline-test files up to 1 MiB; for a
    /// symlink its target.
    text: Option<String>,
    /// A Rust source file with `#[cfg(test)]` / `#[test]`.
    inline_tests: bool,
    /// A symlink (to a file or a directory): `text` is its target.
    #[serde(default)]
    is_link: bool,
}

#[derive(Serialize, Deserialize)]
pub struct Baseline {
    root: PathBuf,
    extra: HashSet<String>,
    files: HashMap<String, Entry>,
    incomplete: bool,
    /// Built from `git HEAD` (resume without a saved manifest): holds watched files only.
    #[serde(default)]
    from_git: bool,
}

/// What differs from the baseline now.
#[derive(Default)]
pub(crate) struct Drift {
    /// Sorted workspace-relative paths added, removed or modified (new symlinks included).
    pub changed: Vec<String>,
    /// Line diffs of modified (or new) test and inline-test files (input of `weakened_tests`).
    pub edits: Vec<FileEdit>,
    /// `path (why)`: deleted, renamed, truncated or altered tests, fixtures and goldens.
    pub weakened: Vec<String>,
    /// Check-definition files that were added, removed or changed.
    pub defs: Vec<String>,
    /// The workspace was too big to scan completely (now or at task start).
    pub incomplete: bool,
}

impl Baseline {
    /// `extra`: further files to watch (scripts the planned checks invoke).
    pub fn take(root: &Path, extra: Vec<String>) -> Self {
        let extra: HashSet<String> = extra.into_iter().collect();
        let (files, incomplete) = scan(root, &extra);
        Self {
            root: root.into(),
            extra,
            files,
            incomplete,
            from_git: false,
        }
    }

    pub(crate) fn root(&self) -> &Path {
        &self.root
    }

    /// A baseline from `git HEAD` for a resumed session that has no saved manifest: the text of
    /// the test, definition, golden and Rust files as committed. `None` outside a git repository.
    pub fn from_git_head(root: &Path, extra: Vec<String>) -> Option<Self> {
        use std::io::{BufRead, BufReader, Read, Write};
        use std::process::{Command, Stdio};
        let ls = Command::new("git")
            .args(["-C"])
            .arg(root)
            .args(["ls-tree", "-r", "-z", "--name-only", "HEAD"])
            .output()
            .ok()
            .filter(|o| o.status.success())?;
        let extra: HashSet<String> = extra.into_iter().collect();
        let paths: Vec<String> = String::from_utf8_lossy(&ls.stdout)
            .split('\0')
            .filter(|p| {
                !p.is_empty()
                    && (is_test_path(p)
                        || is_definition_file(p)
                        || extra.contains(*p)
                        || p.ends_with(".rs"))
            })
            .map(String::from)
            .collect();
        let mut child = Command::new("git")
            .args(["-C"])
            .arg(root)
            .args(["cat-file", "--batch"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .ok()?;
        let mut stdin = child.stdin.take()?;
        let want = paths.clone();
        let writer = std::thread::spawn(move || {
            for p in &want {
                if writeln!(stdin, "HEAD:./{p}").is_err() {
                    break;
                }
            }
        });
        let mut out = BufReader::new(child.stdout.take()?);
        let mut files = HashMap::new();
        for p in &paths {
            let mut head = String::new();
            out.read_line(&mut head).ok()?;
            let size: usize = match head.split_whitespace().collect::<Vec<_>>().as_slice() {
                [_, "blob", n] => n.parse().ok()?,
                _ => continue, // missing or not a blob
            };
            let mut buf = vec![0; size + 1];
            out.read_exact(&mut buf).ok()?;
            buf.pop();
            let Ok(text) = String::from_utf8(buf) else {
                continue;
            };
            let inline_tests =
                p.ends_with(".rs") && (text.contains("#[cfg(test)]") || text.contains("#[test]"));
            let watched = is_test_path(p) || is_definition_file(p) || extra.contains(p);
            if (!watched && !inline_tests) || text.len() as u64 > MAX_TEXT {
                continue;
            }
            files.insert(
                p.clone(),
                Entry {
                    len: text.len() as u64,
                    mtime: None,
                    text: Some(text),
                    inline_tests,
                    is_link: false,
                },
            );
        }
        let _ = writer.join();
        let _ = child.wait();
        Some(Self {
            root: root.into(),
            extra,
            files,
            incomplete: false,
            from_git: true,
        })
    }

    pub(crate) fn drift(&self) -> Drift {
        let (now, incomplete) = scan(&self.root, &self.extra);
        let mut d = Drift {
            incomplete: incomplete || self.incomplete,
            ..Default::default()
        };
        let differs = |a: &Entry, b: &Entry| match (&a.text, &b.text) {
            (Some(x), Some(y)) => x != y,
            _ => a.len != b.len || a.mtime != b.mtime,
        };
        for (p, old) in &self.files {
            let watched_def = is_definition_file(p) || self.extra.contains(p);
            let Some(new) = now.get(p) else {
                d.changed.push(p.clone());
                if old.is_link {
                    d.weakened
                        .push(format!("{p} (symlink removed or replaced)"));
                }
                if is_test_path(p) || old.inline_tests {
                    d.weakened.push(format!("{p} (deleted or renamed)"));
                }
                if watched_def {
                    d.defs.push(p.clone());
                }
                continue;
            };
            if !differs(old, new) {
                continue;
            }
            d.changed.push(p.clone());
            if old.is_link || new.is_link {
                // a retargeted symlink can point a test directory somewhere else
                d.weakened.push(format!("{p} (symlink target changed)"));
                continue;
            }
            let (Some(o), Some(n)) = (&old.text, &new.text) else {
                if watched_def {
                    d.defs.push(p.clone()); // too big or not text: any change counts
                }
                continue;
            };
            if is_golden_path(p) {
                d.weakened.push(format!(
                    "{p} (existing fixture, snapshot or golden file changed)"
                ));
            }
            if is_test_path(p) || old.inline_tests {
                let (ol, nl) = (o.lines().count(), n.lines().count());
                if is_test_path(p) && ol >= 4 && nl * 2 < ol {
                    d.weakened
                        .push(format!("{p} (truncated from {ol} to {nl} lines)"));
                }
                let e = line_diff(p, o, n);
                // Changing the expected value of an existing assertion or table row is how a
                // test gets made to pass: it needs a human even though an assertion is still there.
                if count_assertions(&e.removed) > 0 {
                    d.weakened
                        .push(format!("{p} (existing assertions changed or removed)"));
                }
                if count_table_rows(&e.removed) > 0 {
                    d.weakened
                        .push(format!("{p} (existing table rows changed or removed)"));
                }
                if old.inline_tests && count_test_cases(&e.removed) > count_test_cases(&e.added) {
                    d.weakened.push(format!("{p} (inline test cases removed)"));
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
            // a git-built baseline only knows the watched files; other new files are not "new"
            if self.from_git && new.text.is_none() {
                continue;
            }
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

fn entry(path: &Path, rel: &str, extra: &HashSet<String>, is_link: bool) -> Option<Entry> {
    if is_link {
        let m = fs::symlink_metadata(path).ok()?;
        let target = fs::read_link(path).ok()?.to_string_lossy().into_owned();
        return Some(Entry {
            len: 0,
            mtime: m.modified().ok(),
            text: Some(target),
            inline_tests: false,
            is_link: true,
        });
    }
    let m = fs::metadata(path).ok()?;
    let watched = is_test_path(rel) || is_definition_file(rel) || extra.contains(rel);
    let rs = rel.ends_with(".rs") && m.len() <= MAX_RS_TEXT;
    let text = (watched && m.len() <= MAX_TEXT || rs)
        .then(|| fs::read_to_string(path).ok())
        .flatten();
    let inline_tests = rs
        && text
            .as_ref()
            .is_some_and(|t| t.contains("#[cfg(test)]") || t.contains("#[test]"));
    Some(Entry {
        len: m.len(),
        mtime: m.modified().ok(),
        text: text.filter(|_| watched || inline_tests),
        inline_tests,
        is_link: false,
    })
}

/// A directory that is genuine, untracked build output (cargo's `target`, wherever it is): it carries cargo's own
/// `CACHEDIR.TAG` (a regular file, never a link, with the signature line), no file of it is tracked by git, and its
/// top level holds nothing a check or a reviewer cares about (only directories and dotfiles besides the tag, no test
/// or definition path). A committed or planted tag in `tests/` or `src/` therefore hides nothing.
fn is_build_output_tree(root: &Path, dir: &Path, rel: &str) -> bool {
    use std::io::Read;
    const SIGNATURE: &[u8] = b"Signature: 8a477f597d28d172789f06886806bc55";
    let tag = dir.join("CACHEDIR.TAG");
    if !fs::symlink_metadata(&tag).is_ok_and(|m| m.file_type().is_file()) {
        return false;
    }
    let mut head = [0u8; SIGNATURE.len()];
    if !fs::File::open(&tag).is_ok_and(|mut f| f.read_exact(&mut head).is_ok())
        || head != *SIGNATURE
    {
        return false;
    }
    if is_test_path(&format!("{rel}/x")) {
        return false;
    }
    let Ok(rd) = fs::read_dir(dir) else {
        return false;
    };
    for e in rd.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        let Ok(ft) = e.file_type() else { return false };
        let child = format!("{rel}/{name}");
        if is_test_path(&child) || is_definition_file(&child) {
            return false;
        }
        if !ft.is_dir() && name != "CACHEDIR.TAG" && !name.starts_with('.') {
            return false;
        }
    }
    // tracked files under it: a committed directory is source, whatever it is named (no git: nothing is tracked)
    let tracked = std::process::Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["ls-files", "-z", "--cached", "--"])
        .arg(rel)
        .output()
        .is_ok_and(|o| o.status.success() && !o.stdout.is_empty());
    !tracked
}

fn scan(root: &Path, extra: &HashSet<String>) -> (HashMap<String, Entry>, bool) {
    let mut out = HashMap::new();
    let mut stack = vec![(root.to_path_buf(), String::new())];
    while let Some((dir, rel)) = stack.pop() {
        let Ok(rd) = fs::read_dir(&dir) else { continue };
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            let Ok(ft) = e.file_type() else { continue };
            let child = if rel.is_empty() {
                name.clone()
            } else {
                format!("{rel}/{name}")
            };
            if ft.is_dir() {
                let skip = SKIP_DIRS.contains(&name.as_str())
                    || (rel.is_empty() && ROOT_SKIP_DIRS.contains(&name.as_str()))
                    || (name == "target" && dir.join("Cargo.toml").exists())
                    || is_build_output_tree(root, &e.path(), &child);
                if !skip {
                    stack.push((e.path(), child));
                }
            } else if ft.is_file() || ft.is_symlink() {
                if out.len() >= MAX_FILES {
                    return (out, true);
                }
                if let Some(en) = entry(&e.path(), &child, extra, ft.is_symlink()) {
                    out.insert(child, en);
                }
            }
        }
    }
    for d in TOOL_DIRS {
        let Ok(rd) = fs::read_dir(root.join(d)) else {
            continue;
        };
        for e in rd.flatten() {
            let Ok(ft) = e.file_type() else { continue };
            let child = format!("{d}/{}", e.file_name().to_string_lossy());
            if let Some(en) = entry(&e.path(), &child, extra, ft.is_symlink()) {
                out.insert(child, en);
            }
        }
    }
    (out, false)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn w(p: &Path, rel: &str, c: &str) {
        let f = p.join(rel);
        fs::create_dir_all(f.parent().unwrap()).unwrap();
        fs::write(f, c).unwrap();
    }

    const SIG: &str = "Signature: 8a477f597d28d172789f06886806bc55\n# cargo";

    #[test]
    fn untracked_build_output_trees_are_not_changes_or_weakened_tests() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path();
        // a cargo target dir that is not beside a Cargo.toml
        w(p, "out/target/CACHEDIR.TAG", SIG);
        w(
            p,
            "out/target/debug/build/x/depfile_tests.rs",
            "#[test]\nfn a() {}\n",
        );
        w(p, "src.js", "1");
        let b = Baseline::take(p, vec![]);
        fs::remove_file(p.join("out/target/debug/build/x/depfile_tests.rs")).unwrap();
        let d = b.drift();
        assert!(
            d.changed.is_empty() && d.weakened.is_empty(),
            "{:?} {:?}",
            d.changed,
            d.weakened
        );
    }

    fn weakened_after_deleting(p: &Path, rel: &str) -> Vec<String> {
        let b = Baseline::take(p, vec![]);
        fs::remove_file(p.join(rel)).unwrap();
        b.drift().weakened
    }

    #[test]
    fn a_tag_does_not_hide_tests_when_it_is_planted_unsigned_linked_or_in_a_tracked_dir() {
        let git = |p: &Path, a: &[&str]| {
            assert!(
                std::process::Command::new("git")
                    .arg("-C")
                    .arg(p)
                    .args(a)
                    .status()
                    .unwrap()
                    .success()
            );
        };
        // valid signature, but the directory is committed (tracked): still watched
        let d = tempfile::tempdir().unwrap();
        let p = d.path();
        w(p, "gen/CACHEDIR.TAG", SIG);
        w(p, "gen/sub/a.test.js", "test('a',()=>{assert(1)})\n");
        git(p, &["init", "-q"]);
        git(p, &["add", "-A"]);
        let wk = weakened_after_deleting(p, "gen/sub/a.test.js");
        assert!(
            wk.iter().any(|x| x.contains("a.test.js (deleted")),
            "tracked: {wk:?}"
        );
        // untracked dir with a test file at its top level (a task made it): watched
        let d = tempfile::tempdir().unwrap();
        w(d.path(), "tests/CACHEDIR.TAG", SIG);
        w(d.path(), "tests/a.test.js", "x\n");
        assert!(!weakened_after_deleting(d.path(), "tests/a.test.js").is_empty());
        // a source file at the top level of the tagged dir: watched
        let d = tempfile::tempdir().unwrap();
        w(d.path(), "src/CACHEDIR.TAG", SIG);
        w(
            d.path(),
            "src/lib.rs",
            "#[cfg(test)]\nmod t { #[test] fn a(){} }\n",
        );
        assert!(!weakened_after_deleting(d.path(), "src/lib.rs").is_empty());
        // no signature
        let d = tempfile::tempdir().unwrap();
        w(d.path(), "tests/CACHEDIR.TAG", "hello");
        w(d.path(), "tests/sub/a.test.js", "x\n");
        assert!(!weakened_after_deleting(d.path(), "tests/sub/a.test.js").is_empty());
        // a symlinked tag
        #[cfg(unix)]
        {
            let d = tempfile::tempdir().unwrap();
            let real = d.path().join("real-tag");
            fs::write(&real, SIG).unwrap();
            fs::create_dir_all(d.path().join("tests/sub")).unwrap();
            std::os::unix::fs::symlink(&real, d.path().join("tests/CACHEDIR.TAG")).unwrap();
            w(d.path(), "tests/sub/a.test.js", "x\n");
            assert!(!weakened_after_deleting(d.path(), "tests/sub/a.test.js").is_empty());
        }
    }

    #[test]
    fn sees_shell_style_changes_to_tests_and_definitions() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path();
        w(
            p,
            "tests/a.test.js",
            "test('a',()=>{assert(1)})\nl2\nl3\nl4\nl5\n",
        );
        w(p, "tests/b.test.js", "x\n");
        w(p, "tests/c.test.js", "test('c',()=>{})\n");
        w(
            p,
            "package.json",
            r#"{"scripts":{"test":"node --test"},"x":1}"#,
        );
        w(p, "src.js", "1");
        let b = Baseline::take(p, vec![]);
        assert!(b.drift().changed.is_empty());
        w(p, "tests/a.test.js", "l2\n");
        fs::remove_file(p.join("tests/b.test.js")).unwrap();
        fs::rename(p.join("tests/c.test.js"), p.join("tests/d.test.js")).unwrap();
        w(p, "package.json", r#"{"scripts":{"test":"true"},"x":2}"#);
        w(p, "src.js", "22");
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

    #[test]
    fn nested_definitions_tool_dirs_goldens_inline_tests_and_symlinks() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path();
        w(p, "web/package.json", r#"{"scripts":{"test":"jest"}}"#);
        w(p, "web/.npmrc", "a=1");
        w(p, "tests/build/x.test.js", "assert(1)\nb\nc\nd\n");
        w(p, "testdata/out.golden", "expected\n");
        w(
            p,
            "src/lib.rs",
            "fn f() {}\n#[cfg(test)]\nmod t {\n    #[test]\n    fn a() {\n        assert_eq!(1, 1);\n    }\n}\n",
        );
        w(p, "node_modules/.bin/jest", "#!/bin/sh\njest\n");
        w(p, "build/out.txt", "skipped at the root");
        let b = Baseline::take(p, vec![]);
        w(p, "web/package.json", r#"{"scripts":{"test":"true"}}"#);
        w(p, "web/.npmrc", "a=2");
        w(p, "node_modules/.bin/jest", "#!/bin/sh\ntrue\n");
        w(p, "testdata/out.golden", "whatever\n");
        w(p, "tests/build/x.test.js", "b\nc\nd\n");
        w(
            p,
            "src/lib.rs",
            "fn f() {}\n#[cfg(test)]\nmod t {\n    #[test]\n    #[ignore]\n    fn a() {\n    }\n}\n",
        );
        w(p, "build/out.txt", "changed");
        #[cfg(unix)]
        std::os::unix::fs::symlink("src", p.join("tests/link")).unwrap();
        let d = b.drift();
        for want in ["web/package.json", "web/.npmrc", "node_modules/.bin/jest"] {
            assert!(d.defs.iter().any(|f| f == want), "{want} in {:?}", d.defs);
        }
        let weak = d.weakened.join("|");
        for want in [
            "testdata/out.golden (existing fixture",
            "tests/build/x.test.js (existing assertions",
            "src/lib.rs (existing assertions",
        ] {
            assert!(weak.contains(want), "{want} in {weak}");
        }
        assert!(
            d.edits
                .iter()
                .any(|e| e.path == "src/lib.rs" && e.added.iter().any(|l| l.contains("#[ignore]")))
        );
        assert!(!d.changed.iter().any(|f| f == "build/out.txt"));
        #[cfg(unix)]
        assert!(d.changed.iter().any(|f| f == "tests/link"));
    }

    #[cfg(unix)]
    #[test]
    fn a_retargeted_directory_symlink_is_flagged() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path();
        w(p, "real/a.test.js", "assert(1)\n");
        w(p, "other/a.test.js", "assert(1)\n");
        std::os::unix::fs::symlink("real", p.join("tests")).unwrap();
        let b = Baseline::take(p, vec![]);
        fs::remove_file(p.join("tests")).unwrap();
        std::os::unix::fs::symlink("other", p.join("tests")).unwrap();
        let d = b.drift();
        assert!(
            d.weakened
                .iter()
                .any(|w| w.starts_with("tests (symlink target changed)")),
            "{:?}",
            d.weakened
        );
    }

    #[test]
    fn git_head_baseline_flags_a_deletion_after_the_commit() {
        let git = |p: &Path, a: &[&str]| {
            std::process::Command::new("git")
                .arg("-C")
                .arg(p)
                .args(["-c", "user.email=a@b", "-c", "user.name=x"])
                .args(a)
                .output()
                .map(|o| o.status.success())
                .unwrap_or(false)
        };
        let d = tempfile::tempdir().unwrap();
        let p = d.path();
        w(p, "tests/a.test.js", "assert(1)\nb\nc\nd\n");
        w(p, "src.js", "1");
        if !(git(p, &["init", "-q"]) && git(p, &["add", "-A"]) && git(p, &["commit", "-qm", "x"])) {
            return; // no git here
        }
        let b = Baseline::from_git_head(p, vec![]).expect("git repo");
        assert!(b.drift().changed.is_empty(), "{:?}", b.drift().changed);
        fs::remove_file(p.join("tests/a.test.js")).unwrap();
        w(p, "new-untracked.txt", "x");
        let d = b.drift();
        assert!(
            d.weakened
                .iter()
                .any(|w| w.contains("tests/a.test.js (deleted")),
            "{:?}",
            d.weakened
        );
        assert!(!d.changed.iter().any(|f| f == "new-untracked.txt"));
    }

    /// Timing of the whole-workspace scan on this repository (`cargo test -p synthesis-verification
    /// --release -- --ignored --nocapture scan_timing`).
    #[test]
    #[ignore]
    fn scan_timing() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let t = std::time::Instant::now();
        let b = Baseline::take(&root, vec![]);
        let take = t.elapsed();
        let t = std::time::Instant::now();
        let d = b.drift();
        println!(
            "files={} take={take:?} drift={:?} changed={}",
            b.files.len(),
            t.elapsed(),
            d.changed.len()
        );
    }
}
