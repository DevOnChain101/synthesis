//! Reads the working tree for `/files` and `/diff`: read-only `git`, bounded, run on a worker
//! thread (never the render thread). Nothing is invented: no git, or not a repository, is a note.
//! (From the removed new TUI's Mission centre tabs, as data and text only.)
//!
//! What is guaranteed about the programs a repository's config can name: `core.fsmonitor`, the
//! textconv and external diff drivers, and every `filter.<name>.clean|smudge|process` found in the
//! config are switched off for each call (`required=false`, so git does not fail on the missing
//! program); submodules are not entered; `git` is the one found by absolute path on the injected
//! PATH; the process gets a minimal environment, `core.hooksPath` points at the null device, and
//! network protocols are refused. NOT guaranteed: other config keys git may honour that this list does not
//! name. The scan runs only in a trusted folder (`--trust-project`), which is the real gate.

use crate::sanitize::clean_line;
use std::collections::HashMap;
use std::path::Path;
use std::time::Duration;

use synthesis_config::Env;
use synthesis_tui_base::imgclip::RealExec;

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

    /// `git config --name-only --get-regexp ^filter\.` runs nothing: from its names, the
    /// `-c` overrides that switch off every filter driver. `Err`: a name that cannot be passed
    /// on a command line safely (so git is not run at all).
    fn filter_guards(&self, cwd: &Path) -> Result<Vec<String>, String> {
        let Some((out, _)) = self.run(
            cwd,
            &["config", "--name-only", "-z", "--get-regexp", r"^filter\."],
            0,
        ) else {
            return Ok(vec![]); // none defined (exit 1) or no config
        };
        let mut names: Vec<String> = vec![];
        for key in out.split(|b| *b == 0).filter(|k| !k.is_empty()) {
            let key = String::from_utf8_lossy(key);
            // `filter.<name>.<variable>`; the name may itself contain dots
            let Some((name, _var)) = key.strip_prefix("filter.").and_then(|r| r.rsplit_once('.'))
            else {
                continue;
            };
            if name.is_empty()
                || name.contains(['=', '\n', '\0'])
                || name.chars().any(char::is_control)
            {
                return Err("a git filter in this repository's config has a name that cannot be switched off safely".into());
            }
            if !names.iter().any(|n| n == name) {
                names.push(name.to_string());
            }
        }
        if names.len() > 100 {
            return Err(
                "this repository's config defines too many git filters to switch off".into(),
            );
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

    fn run_with(
        &self,
        cwd: &Path,
        extra: &[String],
        args: &[&str],
        cut_at: u8,
    ) -> Option<(Vec<u8>, bool)> {
        let exe = self.exe.as_ref()?;
        let mut argv: Vec<String> = [
            "-c",
            "core.fsmonitor=false",
            "-c",
            "core.untrackedCache=false",
            "-c",
            "protocol.allow=never",
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
        let o = self
            .exec
            .run_with(exe, &argv, Some(cwd), Some(&self.env))
            .ok()?;
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
    let mut truncated = files_cut;
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
    // no textconv and no external diff driver: both are programs named by the repository's config
    let diff = |extra: &[&str]| {
        let mut a = vec![
            "diff",
            "--no-color",
            "--no-ext-diff",
            "--no-textconv",
            "--ignore-submodules=all",
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
            ("SYNTHESIS_API_KEY", "sk-secret2"),
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
            "SYNTHESIS_API_KEY",
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
}
