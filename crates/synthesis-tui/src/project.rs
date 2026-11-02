//! Reads the working tree for `/files` and `/diff`: read-only `git`, bounded, run on a worker
//! thread (never the render thread). Nothing is invented: no git, or not a repository, is a note.
//! (From the removed new TUI's Mission centre tabs, as data and text only.)

use crate::sanitize::clean_line;
use std::collections::HashMap;
use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TreeRow {
    pub depth: u16,
    pub name: String,
    pub dir: bool,
    /// `git status --porcelain` letter: `M`, `A`, `D`, `R`, `?`.
    pub status: Option<char>,
}

/// What was read; `note` is why there is nothing to show (`not a git repository`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ProjectData {
    pub loaded: bool,
    pub note: Option<String>,
    pub rows: Vec<TreeRow>,
    /// Unified diff of the tracked changes, one line per entry.
    pub diff: Vec<String>,
    /// The tree or the diff was cut at the bound.
    pub truncated: bool,
}

/// `/files`: the tree, indented, with the git status letter before changed files.
pub fn tree_lines(p: &ProjectData) -> Vec<String> {
    if let Some(n) = &p.note {
        return vec![format!("/files: {n}")];
    }
    let mut v: Vec<String> = p
        .rows
        .iter()
        .map(|r| {
            let pad = "  ".repeat(usize::from(r.depth));
            match (r.dir, r.status) {
                (true, _) => format!("{pad}{}/", r.name),
                (false, Some(s)) => format!("{pad}{s} {}", r.name),
                (false, None) => format!("{pad}  {}", r.name),
            }
        })
        .collect();
    if v.is_empty() {
        v.push("/files: no files".into());
    }
    if p.truncated {
        v.push("… cut at the display limit".into());
    }
    v
}

/// `/diff [file]`: one line per changed file (`+a -d path`), then the diff of the file asked for
/// (a path or a part of it) or, without an argument, the whole bounded diff.
pub fn diff_lines(p: &ProjectData, which: &str) -> Vec<String> {
    if let Some(n) = &p.note {
        return vec![format!("/diff: {n}")];
    }
    // `diff --git a/x b/x` starts a file
    let mut files: Vec<(String, Vec<&String>)> = vec![];
    for l in &p.diff {
        if let Some(rest) = l.strip_prefix("diff --git ") {
            let name = rest.rsplit_once(" b/").map_or(rest, |x| x.1).to_string();
            files.push((name, vec![]));
        }
        if let Some(f) = files.last_mut() {
            f.1.push(l);
        }
    }
    if files.is_empty() {
        return vec!["/diff: no uncommitted changes".into()];
    }
    let count = |f: &(String, Vec<&String>), c: char| {
        f.1.iter()
            .filter(|l| l.starts_with(c) && !l.starts_with("+++") && !l.starts_with("---"))
            .count()
    };
    let mut v: Vec<String> = files
        .iter()
        .map(|f| format!("+{} -{}  {}", count(f, '+'), count(f, '-'), f.0))
        .collect();
    let picked: Vec<&(String, Vec<&String>)> = files
        .iter()
        .filter(|f| which.is_empty() || f.0.contains(which))
        .collect();
    if !which.is_empty() && picked.is_empty() {
        v.push(format!("/diff: no changed file matches \"{which}\""));
        return v;
    }
    v.push(String::new());
    for f in picked {
        v.extend(f.1.iter().map(|l| (*l).clone()));
    }
    if p.truncated {
        v.push("… cut at the display limit".into());
    }
    v
}

const MAX_ROWS: usize = 400;
const MAX_DIFF_LINES: usize = 600;
const MAX_BYTES: u64 = 1 << 20;

fn git(cwd: &Path, args: &[&str]) -> Option<Vec<u8>> {
    let mut child = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let mut buf = Vec::new();
    child
        .stdout
        .take()?
        .take(MAX_BYTES)
        .read_to_end(&mut buf)
        .ok()?;
    // the bound may stop the read early: kill instead of waiting on a full pipe
    let _ = child.kill();
    let ok = child.wait().ok()?;
    // a killed child has no success status; the output read before the kill is what we want
    (ok.success() || !buf.is_empty()).then_some(buf)
}

fn split_nul(b: &[u8]) -> Vec<String> {
    b.split(|c| *c == 0)
        .filter(|p| !p.is_empty())
        .map(|p| clean_line(&String::from_utf8_lossy(p)))
        .collect()
}

pub fn scan(cwd: &Path) -> ProjectData {
    let Some(files) = git(
        cwd,
        &[
            "ls-files",
            "-z",
            "--cached",
            "--others",
            "--exclude-standard",
        ],
    ) else {
        let note = if Command::new("git").arg("--version").output().is_ok() {
            "not a git repository"
        } else {
            "git was not found"
        };
        return ProjectData {
            loaded: true,
            note: Some(note.into()),
            ..Default::default()
        };
    };
    let mut paths = split_nul(&files);
    paths.sort();
    paths.dedup();
    let mut status: HashMap<String, char> = HashMap::new();
    if let Some(st) = git(cwd, &["status", "--porcelain=v1", "-z"]) {
        let mut it = st.split(|c| *c == 0).filter(|p| p.len() > 3);
        while let Some(e) = it.next() {
            let (x, y) = (e[0] as char, e[1] as char);
            let path = clean_line(&String::from_utf8_lossy(&e[3..]));
            let letter = if x == '?' {
                '?'
            } else if x == 'A' || y == 'A' {
                'A'
            } else if x == 'D' || y == 'D' {
                'D'
            } else if x == 'R' {
                it.next(); // the old name of a rename follows as its own entry
                'R'
            } else {
                'M'
            };
            status.insert(path, letter);
        }
    }
    let mut truncated = false;
    let mut rows: Vec<TreeRow> = vec![];
    let mut open: Vec<&str> = vec![];
    'files: for p in &paths {
        let parts: Vec<&str> = p.split('/').collect();
        let dirs = &parts[..parts.len() - 1];
        let common = open.iter().zip(dirs).take_while(|(a, b)| a == b).count();
        open.truncate(common);
        for (d, name) in dirs.iter().enumerate().skip(common) {
            if rows.len() >= MAX_ROWS {
                truncated = true;
                break 'files;
            }
            rows.push(TreeRow {
                depth: d as u16,
                name: (*name).to_string(),
                dir: true,
                status: None,
            });
            open.push(name);
        }
        if rows.len() >= MAX_ROWS {
            truncated = true;
            break;
        }
        rows.push(TreeRow {
            depth: dirs.len() as u16,
            name: parts[parts.len() - 1].to_string(),
            dir: false,
            status: status.get(p).copied(),
        });
    }
    let raw = git(cwd, &["diff", "--no-color", "--no-ext-diff", "HEAD"])
        .or_else(|| git(cwd, &["diff", "--no-color", "--no-ext-diff"]))
        .unwrap_or_default();
    let all: Vec<String> = String::from_utf8_lossy(&raw)
        .lines()
        .map(clean_line)
        .collect();
    truncated |= all.len() > MAX_DIFF_LINES;
    ProjectData {
        loaded: true,
        note: None,
        rows,
        diff: all.into_iter().take(MAX_DIFF_LINES).collect(),
        truncated,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(dir: &Path, args: &[&str]) {
        let ok = Command::new("git")
            .args(["-c", "user.name=t", "-c", "user.email=t@t"])
            .args(args)
            .current_dir(dir)
            .output()
            .unwrap()
            .status
            .success();
        assert!(ok, "git {args:?}");
    }

    #[test]
    fn a_plain_folder_is_a_note_not_an_empty_tree() {
        let d = tempfile::tempdir().unwrap();
        let p = scan(d.path());
        assert!(p.loaded && p.rows.is_empty());
        assert!(p.note.is_some());
    }

    #[test]
    fn tree_carries_status_letters_and_the_diff() {
        let d = tempfile::tempdir().unwrap();
        let w = d.path();
        run(w, &["init", "-q"]);
        std::fs::create_dir(w.join("src")).unwrap();
        std::fs::write(w.join("src/a.rs"), "one\n").unwrap();
        run(w, &["add", "."]);
        run(w, &["commit", "-qm", "x"]);
        std::fs::write(w.join("src/a.rs"), "two\n").unwrap();
        std::fs::write(w.join("new.txt"), "n\n").unwrap();
        let p = scan(w);
        assert_eq!(p.note, None);
        let find = |n: &str| p.rows.iter().find(|r| r.name == n).unwrap().clone();
        assert!(find("src").dir);
        assert_eq!(find("a.rs").depth, 1);
        assert_eq!(find("a.rs").status, Some('M'));
        assert_eq!(find("new.txt").status, Some('?'));
        assert!(p.diff.iter().any(|l| l == "+two"));
    }

    #[test]
    fn diff_lists_files_and_narrows_to_one() {
        let d = tempfile::tempdir().unwrap();
        let w = d.path();
        run(w, &["init", "-q"]);
        std::fs::write(w.join("a.rs"), "one\n").unwrap();
        std::fs::write(w.join("b.rs"), "x\n").unwrap();
        run(w, &["add", "."]);
        run(w, &["commit", "-qm", "x"]);
        std::fs::write(w.join("a.rs"), "two\nthree\n").unwrap();
        std::fs::write(w.join("b.rs"), "y\n").unwrap();
        let p = scan(w);
        let all = diff_lines(&p, "");
        assert_eq!(all[0], "+2 -1  a.rs");
        assert_eq!(all[1], "+1 -1  b.rs");
        assert!(all.iter().any(|l| l == "+two") && all.iter().any(|l| l == "+y"));
        let one = diff_lines(&p, "b.rs");
        assert!(one.iter().any(|l| l == "+y") && !one.iter().any(|l| l == "+two"));
        assert!(
            diff_lines(&p, "zzz")
                .last()
                .unwrap()
                .contains("no changed file matches")
        );
        let t = tree_lines(&p);
        assert!(t.iter().any(|l| l == "M a.rs") && t.iter().any(|l| l == "M b.rs"));
        assert!(
            diff_lines(&scan(tempfile::tempdir().unwrap().path()), "")[0].starts_with("/diff:")
        );
    }
}
