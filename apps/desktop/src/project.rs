//! The working tree for the Explorer and Changes tabs: read-only `git`, bounded. The parsers are
//! pure and tested; `scan` only runs the commands.

use std::path::Path;
use std::process::{Command, Stdio};

const MAX_ROWS: usize = 500;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    pub depth: usize,
    pub name: String,
    pub dir: bool,
    /// Path relative to the root (files only; empty for directories).
    pub path: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Change {
    /// `M`, `A`, `D`, `R` or `?`.
    pub letter: char,
    pub path: String,
}

#[derive(Debug, Clone, Default)]
pub struct Project {
    pub tree: Vec<Row>,
    pub changes: Vec<Change>,
    /// `(added, removed)` lines of the tracked changes.
    pub stat: (u32, u32),
    /// Why there is nothing to show (not a repository, no git).
    pub note: Option<String>,
}

fn git(cwd: &Path, args: &[&str]) -> Option<Vec<u8>> {
    let o = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    o.status.success().then_some(o.stdout)
}

/// Rows of a directory tree from sorted `git ls-files` paths.
pub fn tree_rows(paths: &[String]) -> Vec<Row> {
    let mut rows = vec![];
    let mut open: Vec<&str> = vec![];
    for p in paths {
        let parts: Vec<&str> = p.split('/').collect();
        let dirs = &parts[..parts.len() - 1];
        let common = open.iter().zip(dirs).take_while(|(a, b)| a == b).count();
        open.truncate(common);
        for (d, name) in dirs.iter().enumerate().skip(common) {
            rows.push(Row {
                depth: d,
                name: (*name).into(),
                dir: true,
                path: String::new(),
            });
            open.push(name);
        }
        rows.push(Row {
            depth: dirs.len(),
            name: parts[parts.len() - 1].into(),
            dir: false,
            path: p.clone(),
        });
        if rows.len() >= MAX_ROWS {
            break;
        }
    }
    rows
}

/// `git status --porcelain=v1 -z` into one change per path.
pub fn parse_status(z: &[u8]) -> Vec<Change> {
    let mut out = vec![];
    let mut it = z.split(|c| *c == 0).filter(|p| p.len() > 3);
    while let Some(e) = it.next() {
        let (x, y) = (e[0] as char, e[1] as char);
        let letter = match (x, y) {
            ('?', _) => '?',
            ('R', _) => {
                it.next(); // the old name follows as its own entry
                'R'
            }
            ('A', _) | (_, 'A') => 'A',
            ('D', _) | (_, 'D') => 'D',
            _ => 'M',
        };
        out.push(Change {
            letter,
            path: String::from_utf8_lossy(&e[3..]).into_owned(),
        });
    }
    out
}

/// `git diff --numstat` into total (added, removed); binary files (`-`) count as 0.
pub fn parse_numstat(text: &str) -> (u32, u32) {
    text.lines().fold((0, 0), |(a, r), l| {
        let mut f = l.split('\t');
        let n = |s: Option<&str>| s.and_then(|v| v.parse::<u32>().ok()).unwrap_or(0);
        (a + n(f.next()), r + n(f.next()))
    })
}

pub fn scan(cwd: &Path) -> Project {
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
        return Project {
            note: Some(note.into()),
            ..Project::default()
        };
    };
    let mut paths: Vec<String> = files
        .split(|c| *c == 0)
        .filter(|p| !p.is_empty())
        .map(|p| String::from_utf8_lossy(p).into_owned())
        .collect();
    paths.sort();
    paths.dedup();
    Project {
        tree: tree_rows(&paths),
        changes: git(cwd, &["status", "--porcelain=v1", "-z"])
            .map(|s| parse_status(&s))
            .unwrap_or_default(),
        stat: git(cwd, &["diff", "--numstat", "HEAD"])
            .map(|d| parse_numstat(&String::from_utf8_lossy(&d)))
            .unwrap_or_default(),
        note: None,
    }
}

/// The current branch name (empty when detached or not a repository).
pub fn branch(cwd: &Path) -> String {
    git(cwd, &["branch", "--show-current"])
        .map(|b| String::from_utf8_lossy(&b).trim().to_string())
        .unwrap_or_default()
}

/// Unified diff of one path against HEAD (untracked files: the whole file as added), capped.
pub fn diff_of(cwd: &Path, path: &str) -> Vec<String> {
    let raw = git(
        cwd,
        &["diff", "--no-color", "--no-ext-diff", "HEAD", "--", path],
    )
    .filter(|d| !d.is_empty())
    .or_else(|| {
        std::fs::read(cwd.join(path)).ok().map(|b| {
            String::from_utf8_lossy(&b)
                .lines()
                .map(|l| format!("+{l}\n"))
                .collect::<String>()
                .into_bytes()
        })
    })
    .unwrap_or_default();
    String::from_utf8_lossy(&raw)
        .lines()
        .take(400)
        .map(str::to_string)
        .collect()
}

/// The first lines of a text file for the viewer (binary or unreadable files give a note).
pub fn read_head(cwd: &Path, path: &str) -> Vec<String> {
    match std::fs::read(cwd.join(path)) {
        Ok(b) if !b.contains(&0) => String::from_utf8_lossy(&b)
            .lines()
            .take(400)
            .map(str::to_string)
            .collect(),
        Ok(_) => vec!["(binary file)".into()],
        Err(e) => vec![format!("cannot read: {e}")],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tree_rows_nest_directories_once() {
        let p = ["a/b/c.rs", "a/b/d.rs", "a/e.rs", "z.md"].map(String::from);
        let r = tree_rows(&p);
        let shape: Vec<_> = r
            .iter()
            .map(|r| (r.depth, r.name.as_str(), r.dir))
            .collect();
        assert_eq!(
            shape,
            [
                (0, "a", true),
                (1, "b", true),
                (2, "c.rs", false),
                (2, "d.rs", false),
                (1, "e.rs", false),
                (0, "z.md", false)
            ]
        );
    }

    #[test]
    fn status_letters_and_renames() {
        let z = b" M src/a.rs\0?? new.txt\0A  added.rs\0R  to.rs\0from.rs\0 D gone.rs\0";
        let c: Vec<_> = parse_status(z)
            .into_iter()
            .map(|c| (c.letter, c.path))
            .collect();
        assert_eq!(
            c,
            [
                ('M', "src/a.rs".to_string()),
                ('?', "new.txt".into()),
                ('A', "added.rs".into()),
                ('R', "to.rs".into()),
                ('D', "gone.rs".into())
            ]
        );
    }

    #[test]
    fn numstat_sums_and_skips_binary() {
        assert_eq!(parse_numstat("3\t1\ta\n-\t-\tb.png\n2\t0\tc\n"), (5, 1));
    }
}
