//! Reads the working tree for `/files` and `/diff`: read-only `git`, bounded, run on a worker
//! thread (never the render thread). Nothing is invented: no git, or not a repository, is a note.
//! (From the removed redesigned TUI's Mission centre tabs, as data and text only.)
//!
//! What is guaranteed about the programs a repository's config can name: `core.fsmonitor`, the
//! textconv and external diff drivers, and every `filter.<name>.clean|smudge|process` found in the
//! config are switched off for each call (`required=false`, so git does not fail on the missing
//! program); submodules are not entered; `git` is the one found by absolute path on the injected
//! PATH; the process gets a minimal environment (`GIT_CONFIG_NOSYSTEM`, `GIT_PAGER=cat`, no askpass),
//! `core.hooksPath` points at the null device, `core.pager`, `core.sshCommand`, `core.askPass` and the
//! credential helper are emptied, output is bounded and the call has a wall-clock limit with a process-group kill,
//! and network protocols are refused. NOT guaranteed: other config keys git may honour that this list does not
//! name. The git scan runs only in a trusted folder (`--trust-project`); an untrusted folder gets [`scan_plain`]
//! (no git at all), which is why the file explorer needs no trust.

use std::collections::HashMap;
use std::path::Path;
use std::time::{Duration, Instant};
use synthesis_harness::sanitize_for_display as clean_line;

use synthesis_cli_ui_base::imgclip::{Output, RealExec};
use synthesis_config::Env;

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
    /// `(added, deleted, path)` of every changed file from `git diff --numstat` (the counts of a
    /// binary file are `-`); complete even when `diff` was cut at the display limit.
    pub stats: Vec<(String, String, String)>,
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
    let mut v: Vec<String> = if p.stats.is_empty() {
        files
            .iter()
            .map(|f| format!("+{} -{}  {}", count(f, '+'), count(f, '-'), f.0))
            .collect()
    } else {
        p.stats
            .iter()
            .map(|(a, d, name)| format!("+{a} -{d}  {name}"))
            .collect()
    };
    if p.truncated {
        // first, where it is seen: the list below is complete, the diff text is not
        v.insert(
            0,
            "… cut at the display limit (the file list is complete)".into(),
        );
    }
    let picked: Vec<&(String, Vec<&String>)> = files
        .iter()
        .filter(|f| which.is_empty() || f.0.contains(which))
        .collect();
    if !which.is_empty() && picked.is_empty() {
        let beyond = p.stats.iter().any(|st| st.2.contains(which));
        v.push(if beyond {
            format!("/diff: the text of \"{which}\" is past the display limit; use git diff")
        } else {
            format!("/diff: no changed file matches \"{which}\"")
        });
        return v;
    }
    v.push(String::new());
    for f in picked {
        v.extend(f.1.iter().map(|l| (*l).clone()));
    }
    v
}

const MAX_ROWS: usize = 400;
/// Rows of the project tree (every directory and file, collapsed in the window until opened).
const TREE_ROWS: usize = 5000;
const MAX_DIFF_LINES: usize = 600;
const MAX_BYTES: usize = 1 << 20;
const GIT_TIMEOUT: Duration = Duration::from_secs(5);

/// Read-only `git` for a workspace that may be hostile: its `.git/config` can name programs
/// (`core.fsmonitor`, a textconv or external diff driver, a submodule helper), so every call
/// turns those off, the process gets a minimal environment (never the API keys), a wall-clock
/// limit with a process-group kill, and a bounded read. `safe.directory` is left alone: git's own
/// refusal to read another user's repository stays.
struct Git {
    /// Absolute path of `git` on the injected PATH (never a bare name, never the cwd).
    exe: Option<std::path::PathBuf>,
    env: Vec<(String, String)>,
    exec: RealExec,
}

impl Git {
    fn new(env: &Env) -> Self {
        // from the child allowlist, narrowed again to what git needs
        let child = env.child_env();
        let mut keep: Vec<(String, String)> = child
            .iter()
            .filter(|(k, _)| {
                matches!(
                    *k,
                    "PATH"
                        | "HOME"
                        | "USERPROFILE"
                        | "TMPDIR"
                        | "TEMP"
                        | "TMP"
                        | "SystemRoot"
                        | "SYSTEMROOT"
                        | "LANG"
                ) || k.starts_with("LC_")
            })
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        keep.extend(
            [
                ("GIT_CONFIG_NOSYSTEM", "1"),
                ("GIT_OPTIONAL_LOCKS", "0"),
                ("GIT_TERMINAL_PROMPT", "0"),
                ("GIT_ATTR_NOSYSTEM", "1"),
                ("GIT_EXTERNAL_DIFF", ""),
                ("GIT_PAGER", "cat"),
                ("GIT_ASKPASS", ""),
            ]
            .map(|(k, v)| (k.to_string(), v.to_string())),
        );
        Self {
            exe: synthesis_workspace::find_git(env),
            env: keep,
            exec: RealExec {
                timeout: GIT_TIMEOUT,
                max: MAX_BYTES,
            },
        }
    }

    /// Stdout (cut at the last `cut_at` byte when the bound was hit) and whether it was cut.
    fn run(&self, cwd: &Path, args: &[&str], cut_at: u8) -> Option<(Vec<u8>, bool)> {
        self.run_with(cwd, &[], args, cut_at)
    }

    /// The `-c` overrides that switch off every `filter.<name>` driver the config defines, from
    /// `git config -z --get-regexp ^filter\.` (runs nothing; works on every git that has
    /// `--get-regexp` and `-z`, which is far older than anything supported). FAILS CLOSED: see
    /// [`filter_guards_from`].
    fn filter_guards(&self, cwd: &Path) -> Result<Vec<String>, String> {
        filter_guards_from(self.raw(cwd, &[], &["config", "-z", "--get-regexp", r"^filter\."]))
    }

    /// One hardened git call, the raw result (`None`: git is missing or could not be started).
    fn raw(&self, cwd: &Path, extra: &[String], args: &[&str]) -> Option<Output> {
        let exe = self.exe.as_ref()?;
        let mut argv: Vec<String> = [
            "-c",
            "core.fsmonitor=false",
            "-c",
            "core.untrackedCache=false",
            "-c",
            "core.quotepath=false",
            "-c",
            "protocol.allow=never",
            // no pager, no ssh or credential programs can be named by a repository's config
            "-c",
            "core.pager=cat",
            "-c",
            "core.sshCommand=",
            "-c",
            "core.askPass=",
            "-c",
            "credential.helper=",
        ]
        .iter()
        .map(|s| (*s).to_string())
        .collect();
        argv.extend(["-c".to_string(), "diff.external=".to_string()]);
        argv.extend([
            "-c".to_string(),
            format!(
                "core.hooksPath={}",
                if cfg!(windows) { "NUL" } else { "/dev/null" }
            ),
        ]);
        argv.extend(extra.iter().cloned());
        argv.extend(args.iter().map(|s| (*s).to_string()));
        self.exec
            .run_with(exe, &argv, Some(cwd), Some(&self.env))
            .ok()
    }

    fn run_with(
        &self,
        cwd: &Path,
        extra: &[String],
        args: &[&str],
        cut_at: u8,
    ) -> Option<(Vec<u8>, bool)> {
        let o = self.raw(cwd, extra, args)?;
        if o.timed_out {
            return None;
        }
        let mut out = o.stdout;
        if o.truncated {
            // a half record would be shown as a file or a diff line that is not there
            let keep = out.iter().rposition(|b| *b == cut_at).map_or(0, |i| i + 1);
            out.truncate(keep);
        } else if o.code != Some(0) && out.is_empty() {
            return None;
        }
        Some((out, o.truncated))
    }
}

/// Longest filter name that is passed on a command line.
const MAX_FILTER_NAME: usize = 256;

/// Turns the answer of the filter query into the `-c` guards. Only exit code 1 with no output
/// means "no filter is defined"; anything else that is not a complete, successful answer is
/// REFUSED, because guards that were silently dropped (a timeout, a git without the option, an
/// answer cut at the output bound that hides a later `[filter "evil"]`) would let the repository's
/// own programs run. `Err` is the note shown instead of running git.
fn filter_guards_from(o: Option<Output>) -> Result<Vec<String>, String> {
    let unsafe_note = |why: &str| {
        Err(format!(
            "git filters in this repository's config cannot be switched off safely ({why})"
        ))
    };
    let Some(o) = o else {
        return unsafe_note("git could not be asked");
    };
    if o.timed_out {
        return unsafe_note("the config query timed out");
    }
    if o.truncated {
        return unsafe_note("the config query output was cut at the size limit");
    }
    match o.code {
        Some(1) if o.stdout.is_empty() => return Ok(vec![]), // no match: none defined
        Some(0) => {}
        c => return unsafe_note(&format!("the config query failed, exit {c:?}")),
    }
    let mut names: Vec<String> = vec![];
    // entries are `key\nvalue` separated by NUL; the key is up to the first newline
    for entry in o.stdout.split(|b| *b == 0).filter(|e| !e.is_empty()) {
        let key = entry.split(|b| *b == b'\n').next().unwrap_or(entry);
        let key = String::from_utf8_lossy(key);
        // `filter.<name>.<variable>`; the name may itself contain dots
        let Some((name, _var)) = key.strip_prefix("filter.").and_then(|r| r.rsplit_once('.'))
        else {
            continue;
        };
        if name.is_empty() || name.len() > MAX_FILTER_NAME {
            return unsafe_note("a filter name is empty or too long");
        }
        if name.contains(['=', '\n', '\0']) || name.chars().any(char::is_control) {
            return unsafe_note("a filter name has characters that cannot be passed safely");
        }
        if !names.iter().any(|n| n == name) {
            names.push(name.to_string());
        }
    }
    if names.len() > 100 {
        return unsafe_note("too many filters");
    }
    Ok(names
        .iter()
        .flat_map(|n| {
            ["clean=", "smudge=", "process=", "required=false"]
                .map(|v| ["-c".to_string(), format!("filter.{n}.{v}")])
        })
        .flatten()
        .collect())
}

/// The tree rows of sorted `/`-separated file paths, cut at [`TREE_ROWS`] (and whether it was). The cut drops
/// the DEEPEST rows first, so every top-level entry stays listed: cutting in path order would spend the whole
/// budget on the first directories (`.claude/...` sorts before everything) and hide the rest of the project.
fn tree_rows(paths: &[String], status: &HashMap<String, char>) -> (Vec<TreeRow>, bool) {
    let mut rows: Vec<TreeRow> = vec![];
    let mut open: Vec<&str> = vec![];
    for p in paths {
        let parts: Vec<&str> = p.split('/').collect();
        let dirs = &parts[..parts.len() - 1];
        let common = open.iter().zip(dirs).take_while(|(a, b)| a == b).count();
        open.truncate(common);
        for (d, name) in dirs.iter().enumerate().skip(common) {
            rows.push(TreeRow {
                depth: d as u16,
                name: (*name).to_string(),
                dir: true,
                status: None,
            });
            open.push(name);
        }
        rows.push(TreeRow {
            depth: dirs.len() as u16,
            name: parts[parts.len() - 1].to_string(),
            dir: false,
            status: status.get(p).copied(),
        });
    }
    if rows.len() <= TREE_ROWS {
        return (rows, false);
    }
    let fits = |cap: u16| rows.iter().filter(|r| r.depth <= cap).count() <= TREE_ROWS;
    match (0..=rows.iter().map(|r| r.depth).max().unwrap_or(0))
        .take_while(|c| fits(*c))
        .last()
    {
        Some(cap) => rows.retain(|r| r.depth <= cap),
        None => rows.truncate(TREE_ROWS),
    }
    (rows, true)
}

/// Most files the plain walk collects, and the deepest directory it enters.
const WALK_FILES: usize = 20_000;
const WALK_DEPTH: usize = 32;
/// Entries (files and directories) the walk looks at at most, and its wall-clock limit: a folder of empty
/// directories or a slow mount ends the walk instead of holding the worker.
const WALK_ENTRIES: usize = 100_000;
const WALK_TIME: Duration = Duration::from_secs(2);

/// The tree of a folder WITHOUT git: a plain directory walk, so nothing the folder holds (a `.git/config`
/// naming programs, attributes, hooks) can run. Links are not followed (nor listed), `.git` is skipped, no
/// ignore rules and no status letters. For a folder that is not trusted: the user only looks at file names.
pub fn scan_plain(cwd: &Path) -> ProjectData {
    fn walk(
        dir: &Path,
        rel: &str,
        depth: usize,
        out: &mut Vec<String>,
        seen: &mut usize,
        end: Instant,
    ) {
        let Ok(rd) = std::fs::read_dir(dir) else {
            return;
        };
        for e in rd.flatten() {
            *seen += 1;
            if out.len() >= WALK_FILES || *seen >= WALK_ENTRIES || Instant::now() >= end {
                *seen = WALK_ENTRIES;
                return;
            }
            let (Ok(ft), Some(name)) = (e.file_type(), e.file_name().to_str().map(str::to_string))
            else {
                continue;
            };
            if name == ".git" || ft.is_symlink() {
                continue;
            }
            let path = if rel.is_empty() {
                name
            } else {
                format!("{rel}/{name}")
            };
            if ft.is_dir() {
                if depth < WALK_DEPTH {
                    walk(&e.path(), &path, depth + 1, out, seen, end);
                }
            } else if ft.is_file() {
                out.push(path);
            }
        }
    }
    let mut paths = vec![];
    let mut seen = 0;
    walk(
        cwd,
        "",
        0,
        &mut paths,
        &mut seen,
        Instant::now() + WALK_TIME,
    );
    paths.sort();
    let (rows, cut) = tree_rows(&paths, &HashMap::new());
    ProjectData {
        loaded: true,
        rows,
        truncated: cut || seen >= WALK_ENTRIES || paths.len() >= WALK_FILES,
        ..Default::default()
    }
}

/// `git status --porcelain=v1 -z`: `XY path`, and for a rename or a copy (in either column) the
/// origin follows as an entry of its own, which is not a file of its own.
fn parse_status(st: &[u8]) -> HashMap<String, char> {
    let mut status = HashMap::new();
    let mut it = st.split(|c| *c == 0).filter(|p| p.len() > 3);
    while let Some(e) = it.next() {
        let (x, y) = (e[0] as char, e[1] as char);
        let path = clean_line(&String::from_utf8_lossy(&e[3..]));
        if matches!(x, 'R' | 'C') || matches!(y, 'R' | 'C') {
            it.next();
        }
        let letter = if x == '?' {
            '?'
        } else if x == 'A' || y == 'A' || x == 'C' || y == 'C' {
            'A'
        } else if x == 'D' || y == 'D' {
            'D'
        } else if x == 'R' || y == 'R' {
            'R'
        } else {
            'M'
        };
        status.insert(path, letter);
    }
    status
}

fn split_nul(b: &[u8]) -> Vec<String> {
    b.split(|c| *c == 0)
        .filter(|p| !p.is_empty())
        .map(|p| clean_line(&String::from_utf8_lossy(p)))
        .collect()
}

/// `env`: the process environment as the app holds it (only what git needs is passed on).
pub fn scan(cwd: &Path, env: &Env) -> ProjectData {
    let git = Git::new(env);
    let Some((files, files_cut)) = git.run(
        cwd,
        &[
            "ls-files",
            "-z",
            "--cached",
            "--others",
            "--exclude-standard",
        ],
        0,
    ) else {
        let note = if git.exe.is_some() {
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
    // filter drivers run during status and diff (to compare content): off, every one
    let guards = match git.filter_guards(cwd) {
        Ok(g) => g,
        Err(why) => {
            return ProjectData {
                loaded: true,
                note: Some(format!("{why}; git was not run")),
                ..Default::default()
            };
        }
    };
    // submodules are not looked into (their own config could name programs too)
    if let Some((st, _)) = git.run_with(
        cwd,
        &guards,
        &["status", "--porcelain=v1", "-z", "--ignore-submodules=all"],
        0,
    ) {
        status = parse_status(&st);
    }
    let (rows, rows_cut) = tree_rows(&paths, &status);
    let mut truncated = files_cut || rows_cut;
    // no textconv and no external diff driver: both are programs named by the repository's config
    let diff = |extra: &[&str]| {
        let mut a = vec![
            "diff",
            "--no-color",
            "--no-ext-diff",
            "--no-textconv",
            "--ignore-submodules=all",
            "--no-renames",
        ];
        a.extend(extra);
        git.run_with(cwd, &guards, &a, b'\n')
    };
    let (raw, diff_cut) = diff(&["HEAD"]).or_else(|| diff(&[])).unwrap_or_default();
    truncated |= diff_cut;
    let all: Vec<String> = String::from_utf8_lossy(&raw)
        .lines()
        .map(clean_line)
        .collect();
    truncated |= all.len() > MAX_DIFF_LINES;
    // counts of every file, not just the ones inside the displayed lines
    let (stat_raw, _) = diff(&["--numstat", "HEAD"])
        .or_else(|| diff(&["--numstat"]))
        .unwrap_or_default();
    let stats = String::from_utf8_lossy(&stat_raw)
        .lines()
        .filter_map(|l| {
            let mut it = l.splitn(3, '\t');
            Some((
                it.next()?.to_string(),
                it.next()?.to_string(),
                clean_line(it.next()?),
            ))
        })
        .take(MAX_ROWS)
        .collect();
    ProjectData {
        loaded: true,
        note: None,
        rows,
        diff: all.into_iter().take(MAX_DIFF_LINES).collect(),
        stats,
        truncated,
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_plain_scan_lists_files_without_git_or_links() {
        let d = tempfile::tempdir().unwrap();
        let w = d.path();
        std::fs::create_dir_all(w.join("src")).unwrap();
        std::fs::create_dir_all(w.join(".git")).unwrap();
        std::fs::write(w.join("src/a.rs"), "x").unwrap();
        std::fs::write(w.join("b.txt"), "x").unwrap();
        std::fs::write(w.join(".git/config"), "[core]\nfsmonitor = touch pwned\n").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink("/etc", w.join("out")).unwrap();
        let d = super::scan_plain(w);
        let names: Vec<_> = d
            .rows
            .iter()
            .map(|r| (r.depth, r.name.as_str(), r.dir))
            .collect();
        assert_eq!(
            names,
            [(0, "b.txt", false), (0, "src", true), (1, "a.rs", false)]
        );
        assert!(d.loaded && d.note.is_none() && !d.truncated);
    }

    #[test]
    fn a_big_dot_directory_does_not_use_up_the_listing_of_the_project() {
        let paths: Vec<String> = (0..TREE_ROWS + 50)
            .map(|i| format!(".claude/worktrees/w{i}/f"))
            .chain([
                "README.md".into(),
                "src/main.rs".into(),
                "src/lib/a.rs".into(),
            ])
            .collect();
        let mut paths = paths;
        paths.sort();
        let (rows, cut) = tree_rows(&paths, &HashMap::new());
        let top: Vec<_> = rows
            .iter()
            .filter(|r| r.depth == 0)
            .map(|r| r.name.as_str())
            .collect();
        assert!(cut && rows.len() <= TREE_ROWS);
        assert_eq!(top, [".claude", "README.md", "src"]);
        // under the limit nothing is cut
        let (rows, cut) = tree_rows(&paths[paths.len() - 3..], &HashMap::new());
        assert!(!cut && rows.len() == 5, "{rows:?}");
    }

    #[test]
    fn git_runs_with_a_closed_environment_and_no_pager_or_helper_programs() {
        let g = Git::new(&Env::from_pairs([
            ("PATH", "/usr/bin"),
            ("API_KEY", "secret"),
        ]));
        let has = |k: &str, v: &str| g.env.iter().any(|(a, b)| a == k && b == v);
        for (k, v) in [
            ("GIT_CONFIG_NOSYSTEM", "1"),
            ("GIT_PAGER", "cat"),
            ("GIT_TERMINAL_PROMPT", "0"),
            ("GIT_ASKPASS", ""),
            ("GIT_EXTERNAL_DIFF", ""),
        ] {
            assert!(has(k, v), "{k}");
        }
        assert!(
            !g.env.iter().any(|(k, _)| k == "API_KEY"),
            "no key reaches git"
        );
        assert_eq!((g.exec.timeout, g.exec.max), (GIT_TIMEOUT, MAX_BYTES));
    }

    use super::*;
    use std::process::Command;

    fn env() -> Env {
        Env::from_pairs([(
            "PATH",
            Env::from_process()
                .get("PATH")
                .unwrap_or_default()
                .to_string(),
        )])
    }

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
        let p = scan(d.path(), &env());
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
        let p = scan(w, &env());
        assert_eq!(p.note, None);
        let find = |n: &str| p.rows.iter().find(|r| r.name == n).unwrap().clone();
        assert!(find("src").dir);
        assert_eq!(find("a.rs").depth, 1);
        assert_eq!(find("a.rs").status, Some('M'));
        assert_eq!(find("new.txt").status, Some('?'));
        assert!(p.diff.iter().any(|l| l == "+two"));
    }

    #[test]
    fn diff_counts_cover_files_past_the_display_limit_and_names_are_not_quoted() {
        let d = tempfile::tempdir().unwrap();
        let w = d.path();
        run(w, &["init", "-q"]);
        std::fs::write(w.join("a.txt"), "x\n").unwrap();
        std::fs::write(w.join("ü.txt"), "x\n").unwrap();
        std::fs::write(w.join("z.txt"), "x\n").unwrap();
        run(w, &["add", "."]);
        run(w, &["commit", "-qm", "x"]);
        let many = |n: usize| (0..n).map(|i| format!("l{i}\n")).collect::<String>();
        std::fs::write(w.join("a.txt"), many(1000)).unwrap();
        std::fs::write(w.join("ü.txt"), many(300)).unwrap();
        std::fs::write(w.join("z.txt"), many(200)).unwrap();
        let p = scan(w, &env());
        assert!(p.truncated);
        let v = diff_lines(&p, "");
        assert!(v[0].starts_with("… cut at the display limit"), "{v:?}");
        assert_eq!(v[1], "+1000 -1  a.txt");
        assert_eq!(v[3], "+300 -1  ü.txt", "no octal quoting");
        assert_eq!(
            v[2], "+200 -1  z.txt",
            "a file past line 600 is listed with its count"
        );
        assert!(
            diff_lines(&p, "z.txt")
                .last()
                .unwrap()
                .contains("past the display limit")
        );
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
        let p = scan(w, &env());
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
            diff_lines(&scan(tempfile::tempdir().unwrap().path(), &env()), "")[0]
                .starts_with("/diff:")
        );
    }

    #[test]
    fn status_origins_of_renames_and_copies_are_not_files() {
        let st = b"R  new.rs\0old.rs\0C  copy.rs\0orig.rs\0 M plain.rs\0?? u.txt\0 R moved\0was\0";
        let m = parse_status(st);
        assert_eq!(m.get("new.rs"), Some(&'R'));
        assert_eq!(m.get("copy.rs"), Some(&'A'));
        assert_eq!(m.get("plain.rs"), Some(&'M'));
        assert_eq!(m.get("u.txt"), Some(&'?'));
        assert_eq!(m.get("moved"), Some(&'R'));
        for origin in ["old.rs", "orig.rs", "was"] {
            assert!(!m.contains_key(origin), "{origin} is an origin, not a file");
        }
        assert_eq!(m.len(), 5);
    }

    #[cfg(unix)]
    #[test]
    fn a_hostile_git_config_runs_nothing() {
        use std::os::unix::fs::PermissionsExt;
        let d = tempfile::tempdir().unwrap();
        let w = std::fs::canonicalize(d.path()).unwrap();
        let (repo, tools) = (w.join("repo"), w.join("tools"));
        std::fs::create_dir_all(&repo).unwrap();
        std::fs::create_dir_all(&tools).unwrap();
        let marker = |n: &str| w.join(format!("marker-{n}"));
        let script = |name: &str, body: String| {
            let p = tools.join(name);
            std::fs::write(&p, format!("#!/bin/sh\n{body}\n")).unwrap();
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
            p
        };
        let fsm = script(
            "fsm.sh",
            format!("touch {}\nprintf '\\0'", marker("fsmonitor").display()),
        );
        let conv = script(
            "conv.sh",
            format!("touch {}\ncat \"$1\"", marker("textconv").display()),
        );
        let ext = script(
            "ext.sh",
            format!("touch {}\nexit 0", marker("extdiff").display()),
        );
        run(&repo, &["init", "-q"]);
        std::fs::write(repo.join("a.x"), "one\n").unwrap();
        std::fs::write(repo.join(".gitattributes"), "*.x diff=x\n").unwrap();
        run(&repo, &["add", "."]);
        run(&repo, &["commit", "-qm", "c"]);
        std::fs::write(repo.join("a.x"), "two\n").unwrap();
        run(
            &repo,
            &["config", "core.fsmonitor", &fsm.display().to_string()],
        );
        run(
            &repo,
            &["config", "diff.x.textconv", &conv.display().to_string()],
        );
        run(
            &repo,
            &["config", "diff.external", &ext.display().to_string()],
        );
        // clean / smudge / process filters, one of them with dots in its name
        let fclean = script(
            "fclean.sh",
            format!("touch {}\ncat", marker("filter-clean").display()),
        );
        let fproc = script(
            "fproc.sh",
            format!("touch {}\nexit 1", marker("filter-process").display()),
        );
        let fdots = script(
            "fdots.sh",
            format!("touch {}\ncat", marker("filter-dots").display()),
        );
        std::fs::write(
            repo.join(".gitattributes"),
            "*.x diff=x\n*.c filter=c\n*.p filter=p\n*.d filter=a.b\n",
        )
        .unwrap();
        run(
            &repo,
            &["config", "filter.c.clean", &fclean.display().to_string()],
        );
        run(
            &repo,
            &["config", "filter.c.smudge", &fclean.display().to_string()],
        );
        run(
            &repo,
            &["config", "filter.p.process", &fproc.display().to_string()],
        );
        run(
            &repo,
            &["config", "filter.a.b.clean", &fdots.display().to_string()],
        );
        for f in ["a.c", "b.p", "c.d"] {
            std::fs::write(repo.join(f), "old\n").unwrap();
        }
        run(
            &repo,
            &[
                "-c",
                "filter.c.clean=",
                "-c",
                "filter.a.b.clean=",
                "-c",
                "filter.p.process=",
                "-c",
                "filter.c.smudge=",
                "-c",
                "core.fsmonitor=false",
                "add",
                "-f",
                ".",
            ],
        );
        run(
            &repo,
            &[
                "-c",
                "filter.c.clean=",
                "-c",
                "filter.a.b.clean=",
                "-c",
                "filter.p.process=",
                "-c",
                "filter.c.smudge=",
                "-c",
                "core.fsmonitor=false",
                "commit",
                "-qam",
                "more",
            ],
        );
        for f in ["a.c", "b.p", "c.d"] {
            std::fs::write(repo.join(f), "new content\n").unwrap();
        }
        std::fs::write(repo.join("a.x"), "three\n").unwrap();
        for n in ["filter-clean", "filter-process", "filter-dots"] {
            let _ = std::fs::remove_file(marker(n));
        }
        // positive control: plain git DOES run them here (otherwise this test proves nothing)
        let plain = |args: &[&str]| {
            Command::new("git")
                .args(args)
                .current_dir(&repo)
                .output()
                .unwrap();
        };
        plain(&["status"]);
        plain(&["diff"]);
        let ran: Vec<&str> = [
            "fsmonitor",
            "textconv",
            "extdiff",
            "filter-clean",
            "filter-process",
            "filter-dots",
        ]
        .into_iter()
        .filter(|n| marker(n).exists())
        .collect();
        assert!(
            !ran.is_empty(),
            "plain git ran none of the configured programs: the control failed"
        );
        for n in &ran {
            std::fs::remove_file(marker(n)).unwrap();
        }
        // the scan reads the tree and the diff and runs none of them
        let p = scan(&repo, &env());
        assert_eq!(p.note, None);
        assert!(
            p.rows
                .iter()
                .any(|r| r.name == "a.x" && r.status == Some('M')),
            "{:?}",
            p.rows
        );
        assert!(p.diff.iter().any(|l| l == "+three"), "{:?}", p.diff);
        for n in [
            "fsmonitor",
            "textconv",
            "extdiff",
            "filter-clean",
            "filter-process",
            "filter-dots",
        ] {
            assert!(!marker(n).exists(), "{n} ran from the repository's config");
        }
        // the control must have run a filter, or the filter cases prove nothing
        assert!(
            ran.iter().any(|n| n.starts_with("filter")),
            "plain git ran no filter ({ran:?}): the filter cases proved nothing"
        );
        assert!(
            p.rows
                .iter()
                .any(|r| r.name == "a.c" && r.status == Some('M')),
            "{:?}",
            p.rows
        );

        // a filter name that cannot be passed on a command line safely: git is not run at all
        run(
            &repo,
            &["config", "filter.x=y.clean", &fclean.display().to_string()],
        );
        let refused = scan(&repo, &env());
        assert!(
            refused
                .note
                .as_deref()
                .is_some_and(|n| n.contains("cannot be switched off safely")),
            "{:?}",
            refused.note
        );
        assert!(refused.rows.is_empty() && !marker("filter-clean").exists());
    }

    #[test]
    fn git_gets_a_minimal_environment_and_a_cut_diff_ends_on_a_line() {
        // the environment: the allowlist only, never a key
        let g = Git::new(&Env::from_pairs([
            ("PATH", "/usr/bin"),
            ("HOME", "/h"),
            ("LC_ALL", "C"),
            ("ANTHROPIC_API_KEY", "sk-secret"),
            ("SYNT_API_KEY", "sk-secret2"),
            ("GIT_DIR", "/elsewhere"),
            ("GITHUB_TOKEN", "t"),
        ]));
        let keys: Vec<&str> = g.env.iter().map(|(k, _)| k.as_str()).collect();
        for want in [
            "PATH",
            "HOME",
            "LC_ALL",
            "GIT_CONFIG_NOSYSTEM",
            "GIT_OPTIONAL_LOCKS",
            "GIT_TERMINAL_PROMPT",
        ] {
            assert!(keys.contains(&want), "{want}: {keys:?}");
        }
        for bad in [
            "ANTHROPIC_API_KEY",
            "SYNT_API_KEY",
            "GIT_DIR",
            "GITHUB_TOKEN",
        ] {
            assert!(!keys.contains(&bad), "{bad} reached git");
        }
        // a diff over the byte bound is cut on a line boundary and says so
        let d = tempfile::tempdir().unwrap();
        let w = d.path();
        run(w, &["init", "-q"]);
        let long = |c: char| format!("{}\n", c.to_string().repeat(200_000));
        std::fs::write(
            w.join("big.txt"),
            (0..10).map(|_| long('a')).collect::<String>(),
        )
        .unwrap();
        run(w, &["add", "."]);
        run(w, &["commit", "-qm", "c"]);
        std::fs::write(
            w.join("big.txt"),
            (0..10).map(|_| long('b')).collect::<String>(),
        )
        .unwrap();
        let p = scan(w, &env());
        assert!(p.truncated, "the byte bound was hit");
        for l in &p.diff {
            let n = l.chars().count();
            assert!(n < 400 || n == 200_001, "a half line was shown ({n} chars)");
        }
    }

    fn out(code: Option<i32>, stdout: &[u8], truncated: bool, timed_out: bool) -> Option<Output> {
        Some(Output {
            code,
            stdout: stdout.to_vec(),
            truncated,
            timed_out,
        })
    }

    #[test]
    fn the_filter_query_fails_closed() {
        let refused = |o: Option<Output>| {
            let e = filter_guards_from(o).unwrap_err();
            assert!(e.contains("cannot be switched off safely"), "{e}");
        };
        // git before 2.38 has no --name-only (exit 129), a timeout, a cut answer, no git at all
        refused(out(Some(129), b"", false, false));
        refused(out(None, b"", false, true));
        refused(out(Some(0), b"filter.a.clean\nx\0", true, false));
        refused(out(Some(2), b"", false, false));
        refused(out(Some(1), b"filter.a.clean\nx\0", false, false));
        refused(None);
        // an over-long name is refused, not passed
        let long = format!("filter.{}.clean\nx\0", "n".repeat(MAX_FILTER_NAME + 1));
        refused(out(Some(0), long.as_bytes(), false, false));
        // only exit 1 with no output is "none defined"
        assert_eq!(
            filter_guards_from(out(Some(1), b"", false, false)),
            Ok(vec![])
        );
        // `key\nvalue` entries: the key is up to the first newline, dotted names work
        let g = filter_guards_from(out(
            Some(0),
            b"filter.c.clean\n/bin/x\0filter.a.b.smudge\nline1\nline2\0filter.c.process\ny\0",
            false,
            false,
        ))
        .unwrap();
        let joined = g.join(" ");
        for want in [
            "filter.c.clean=",
            "filter.c.process=",
            "filter.a.b.smudge=",
            "filter.a.b.process=",
            "filter.a.b.required=false",
        ] {
            assert!(joined.contains(want), "{want}: {joined}");
        }
        assert_eq!(g.len(), 16, "two names x four overrides x `-c`");
    }

    /// A config whose filter query answer is over the 1 MiB bound: the evil filter named after
    /// the huge one must not be lost to the cut (that dropped every guard before).
    #[cfg(unix)]
    #[test]
    fn a_huge_filter_name_before_an_evil_filter_refuses_the_scan() {
        use std::os::unix::fs::PermissionsExt;
        let d = tempfile::tempdir().unwrap();
        let w = std::fs::canonicalize(d.path()).unwrap();
        let repo = w.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        let marker = w.join("marker-evil");
        let evil = w.join("evil.sh");
        std::fs::write(
            &evil,
            format!("#!/bin/sh\ntouch {}\ncat\n", marker.display()),
        )
        .unwrap();
        std::fs::set_permissions(&evil, std::fs::Permissions::from_mode(0o755)).unwrap();
        run(&repo, &["init", "-q"]);
        std::fs::write(repo.join("a.e"), "one\n").unwrap();
        std::fs::write(repo.join(".gitattributes"), "*.e filter=evil\n").unwrap();
        run(&repo, &["add", "."]);
        run(&repo, &["commit", "-qm", "c"]);
        let cfg = repo.join(".git/config");
        let mut text = std::fs::read_to_string(&cfg).unwrap();
        text.push_str(&format!(
            "[filter \"{}\"]\n\tclean = true\n",
            "n".repeat(1_200_000)
        ));
        text.push_str(&format!(
            "[filter \"evil\"]\n\tclean = {}\n",
            evil.display()
        ));
        std::fs::write(&cfg, text).unwrap();
        std::fs::write(repo.join("a.e"), "two\n").unwrap();
        // positive control: plain git runs the evil filter through .gitattributes
        Command::new("git")
            .args(["status"])
            .current_dir(&repo)
            .output()
            .unwrap();
        Command::new("git")
            .args(["diff"])
            .current_dir(&repo)
            .output()
            .unwrap();
        assert!(
            marker.exists(),
            "control: plain git did not run the filter, the case proves nothing"
        );
        std::fs::remove_file(&marker).unwrap();
        let p = scan(&repo, &env());
        assert!(
            p.note
                .as_deref()
                .is_some_and(|n| n.contains("cannot be switched off safely")),
            "{:?}",
            p.note
        );
        assert!(p.rows.is_empty());
        assert!(!marker.exists(), "the evil filter ran");
    }
}
