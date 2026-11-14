//! Reading a repository for verification without trusting it (ADR-020 section 3).
//!
//! The attempt's commit lives in the user's repository, whose config (filters such as git-lfs,
//! `core.fsmonitor`, diff drivers, `core.attributesFile`...) and refs (`refs/replace`) are not
//! ours to run on the host outside the sandbox. So nothing here reads that repository's config or
//! refs: a SCRATCH bare repository is made next to the work (empty config, no refs, no
//! `info/attributes`), its `objects/info/alternates` points at the user's object database, and
//! every git command runs against the scratch one with a scrubbed environment, replace objects
//! off, system and global config off, attributes files off, hooks off and a timeout. A filter
//! driver is defined in config only, so a committed `.gitattributes` that names one selects
//! nothing. Entries named `.git` are refused.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use synthesis_core::definitions::is_definition;
use synthesis_verification::{FileEdit, removed_prerequisite_guards, weakened_tests};

use crate::VerifyError;

/// Longest any single git command may run.
const GIT_TIMEOUT: Duration = Duration::from_secs(120);
/// Most output read from one git command.
const GIT_OUTPUT_CAP: u64 = 64 * 1024 * 1024;
/// Most changed files whose line diffs are scanned.
const MAX_SCANNED_FILES: usize = 300;

fn err<T>(m: impl Into<String>) -> Result<T, VerifyError> {
    Err(VerifyError(m.into()))
}

fn null_device() -> &'static str {
    if cfg!(windows) { "NUL" } else { "/dev/null" }
}

fn valid_commit(c: &str) -> bool {
    matches!(c.len(), 40 | 64) && c.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Runs git with a scrubbed environment and a timeout; stdout is returned, a non-zero exit is an error.
fn run(
    gitdir: Option<&Path>,
    index: Option<&Path>,
    work_tree: Option<&Path>,
    args: &[&str],
    stop: &AtomicBool,
) -> Result<Vec<u8>, VerifyError> {
    let null = null_device();
    let mut c = Command::new("git");
    c.env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", null)
        .env("GIT_NO_REPLACE_OBJECTS", "1")
        .env("GIT_ATTR_NOSYSTEM", "1")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_LITERAL_PATHSPECS", "1")
        .env("GIT_OPTIONAL_LOCKS", "0");
    if let Some(root) = std::env::var_os("SystemRoot") {
        c.env("SystemRoot", root);
    }
    if let Some(i) = index {
        c.env("GIT_INDEX_FILE", i);
    }
    if let Some(w) = work_tree {
        c.env("GIT_WORK_TREE", w);
    }
    if let Some(d) = gitdir {
        c.arg("--git-dir").arg(d);
    }
    c.args([
        "-c",
        "core.fsmonitor=false",
        "-c",
        "core.quotePath=false",
        "-c",
        "core.autocrlf=false",
        "-c",
    ])
    .arg(format!("core.attributesFile={null}"))
    .arg("-c")
    .arg(format!("core.hooksPath={null}"))
    .args([
        "-c",
        "diff.noprefix=false",
        "-c",
        concat!("diff.mne", "monicPrefix=false"),
    ])
    .args(["-c", "protocol.allow=never"])
    .args(args)
    .stdin(Stdio::null())
    .stdout(Stdio::piped())
    .stderr(Stdio::piped());
    let mut child = c.spawn().map_err(|e| VerifyError(format!("git: {e}")))?;
    let reader = |s: Option<Box<dyn Read + Send>>| {
        std::thread::spawn(move || {
            let mut b = vec![];
            if let Some(s) = s {
                let _ = s.take(GIT_OUTPUT_CAP).read_to_end(&mut b);
            }
            b
        })
    };
    let out = reader(
        child
            .stdout
            .take()
            .map(|s| Box::new(s) as Box<dyn Read + Send>),
    );
    let errs = reader(
        child
            .stderr
            .take()
            .map(|s| Box::new(s) as Box<dyn Read + Send>),
    );
    let deadline = Instant::now() + GIT_TIMEOUT;
    let status = loop {
        match child.try_wait() {
            Ok(Some(st)) => break Ok(st),
            Ok(None) => {}
            Err(e) => break Err(format!("git: {e}")),
        }
        if stop.load(Ordering::SeqCst) || Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            let why = if stop.load(Ordering::SeqCst) {
                "stopped"
            } else {
                "timed out"
            };
            break Err(format!("git {} {why}", args.first().unwrap_or(&"")));
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    let (out, errs) = (
        out.join().unwrap_or_default(),
        errs.join().unwrap_or_default(),
    );
    match status {
        Ok(st) if st.success() => Ok(out),
        Ok(_) => err(format!(
            "git {}: {}",
            args.first().unwrap_or(&""),
            String::from_utf8_lossy(&errs).trim()
        )),
        Err(e) => err(e),
    }
}

/// A scratch bare repository whose objects are the user's repository's (alternates) and whose
/// config and refs are empty. Removed when dropped.
pub struct Scratch {
    dir: PathBuf,
}

impl Scratch {
    pub fn new(repo: &Path, dir: PathBuf, stop: &AtomicBool) -> Result<Self, VerifyError> {
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir)
            .map_err(|e| VerifyError(format!("{}: {e}", dir.display())))?;
        let this = Self { dir };
        // where the user's objects are, found from the files themselves: no git command, and so
        // none of the repository's config, is read for this
        let objects = objects_dir(repo)?;
        // `canonicalize` gives `\\?\C:\...` on Windows; git's alternates file wants the plain path
        let objects_text = objects.to_string_lossy().into_owned();
        let objects_text = objects_text
            .strip_prefix(r"\\?\")
            .unwrap_or(&objects_text)
            .to_owned();
        if objects_text.contains('\n') {
            return err("the repository's object directory is not usable");
        }
        run(
            None,
            None,
            None,
            &[
                "init",
                "--bare",
                "-q",
                "--template=",
                &this.dir.to_string_lossy(),
            ],
            stop,
        )?;
        let info = this.dir.join("objects/info");
        std::fs::create_dir_all(&info).map_err(|e| VerifyError(e.to_string()))?;
        std::fs::write(info.join("alternates"), format!("{objects_text}\n"))
            .map_err(|e| VerifyError(e.to_string()))?;
        Ok(this)
    }

    fn git(&self, args: &[&str], stop: &AtomicBool) -> Result<Vec<u8>, VerifyError> {
        run(Some(&self.dir), None, None, args, stop)
    }

    fn git_index(
        &self,
        index: &Path,
        work_tree: Option<&Path>,
        args: &[&str],
        stop: &AtomicBool,
    ) -> Result<Vec<u8>, VerifyError> {
        run(Some(&self.dir), Some(index), work_tree, args, stop)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// The object directory of a repository root: `.git/objects`, or for a linked worktree's `.git`
/// file the common directory's, or `objects` of a bare repository.
fn objects_dir(repo: &Path) -> Result<PathBuf, VerifyError> {
    let dot = repo.join(".git");
    let gitdir = if dot.is_dir() {
        dot
    } else if dot.is_file() {
        let text = std::fs::read_to_string(&dot).map_err(|e| VerifyError(e.to_string()))?;
        let target = text
            .lines()
            .find_map(|l| l.strip_prefix("gitdir:"))
            .map(|t| repo.join(t.trim()));
        let Some(target) = target else {
            return err("the .git file names no git directory");
        };
        match std::fs::read_to_string(target.join("commondir")) {
            Ok(c) => target.join(c.trim()),
            Err(_) => target,
        }
    } else {
        repo.to_path_buf()
    };
    let objects = gitdir.join("objects");
    objects
        .is_dir()
        .then(|| std::fs::canonicalize(&objects).unwrap_or(objects))
        .map_or_else(|| err("the repository has no object directory"), Ok)
}

/// A fresh copy of `commit`'s tree in `dest` (emptied first): straight from the object database
/// through a scratch repository, so no `.git`, no hooks, no `export-ignore`, no repository
/// config, no filters and no replace refs are involved.
pub fn fresh_checkout_stoppable(
    repo: &Path,
    commit: &str,
    dest: &Path,
    stop: &AtomicBool,
) -> Result<(), VerifyError> {
    if !valid_commit(commit) {
        return err("not a commit id");
    }
    let _ = std::fs::remove_dir_all(dest);
    std::fs::create_dir_all(dest).map_err(|e| VerifyError(format!("{}: {e}", dest.display())))?;
    let scratch = Scratch::new(repo, dest.with_extension("git"), stop)?;
    let index = dest.with_extension("index");
    let _ = std::fs::remove_file(&index);
    let result = (|| {
        scratch.git_index(&index, None, &["read-tree", commit], stop)?;
        let listing = scratch.git_index(&index, None, &["ls-files", "-z"], stop)?;
        for path in listing.split(|b| *b == 0).filter(|p| !p.is_empty()) {
            let p = String::from_utf8_lossy(path);
            if p.split('/')
                .any(|c| c.trim_end_matches([' ', '.']).eq_ignore_ascii_case(".git"))
            {
                return err(format!(
                    "the commit has an entry inside a .git directory: {p}"
                ));
            }
        }
        scratch
            .git_index(&index, Some(dest), &["checkout-index", "-a", "-f"], stop)
            .map(|_| ())
    })();
    let _ = std::fs::remove_file(&index);
    result
}

pub fn fresh_checkout(repo: &Path, commit: &str, dest: &Path) -> Result<(), VerifyError> {
    fresh_checkout_stoppable(repo, commit, dest, &AtomicBool::new(false))
}

/// One changed path of `base..commit`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub old_mode: String,
    pub new_mode: String,
    pub status: char,
    pub path: String,
}

/// What `base..commit` changed, and what that earns.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ChangeReport {
    pub files: Vec<String>,
    /// Symlinks and submodule links the commit adds or changes: not files, never inside an envelope.
    pub special: Vec<String>,
    pub flags: Vec<&'static str>,
}

fn parse_raw(out: &[u8]) -> Result<Vec<Entry>, VerifyError> {
    // `:<old mode> <new mode> <old sha> <new sha> <status>\0<path>\0`
    let mut parts = out.split(|b| *b == 0).filter(|p| !p.is_empty());
    let mut entries = vec![];
    while let (Some(meta), Some(path)) = (parts.next(), parts.next()) {
        let meta = String::from_utf8_lossy(meta);
        let f: Vec<&str> = meta.trim_start_matches(':').split(' ').collect();
        if f.len() >= 5 {
            // a path that is not UTF-8 cannot be scanned or compared: the commit is not verified
            let path = String::from_utf8(path.to_vec())
                .map_err(|_| VerifyError("a changed path is not valid UTF-8".into()))?;
            entries.push(Entry {
                old_mode: f[0].to_owned(),
                new_mode: f[1].to_owned(),
                status: f[4].chars().next().unwrap_or('?'),
                path,
            });
        }
    }
    Ok(entries)
}

fn entries(
    scratch: &Scratch,
    base: &str,
    commit: &str,
    stop: &AtomicBool,
) -> Result<Vec<Entry>, VerifyError> {
    if !valid_commit(base) || !valid_commit(commit) {
        return err("not a commit id");
    }
    let out = scratch.git(
        &[
            "diff",
            "--raw",
            "-z",
            "-r",
            "--no-renames",
            "--no-ext-diff",
            "--no-textconv",
            base,
            commit,
        ],
        stop,
    )?;
    parse_raw(&out)
}

/// The added and removed lines of each changed file, parsed from the hunks by their line counts
/// (`@@ -a,b +c,d @@` is followed by exactly `b` removed and `d` added lines): a content line that
/// looks like a file header cannot re-attribute anything, and the paths come from `--raw -z`.
fn edits(
    scratch: &Scratch,
    base: &str,
    commit: &str,
    entries: &[Entry],
    stop: &AtomicBool,
) -> Result<Vec<FileEdit>, VerifyError> {
    if entries.len() > MAX_SCANNED_FILES {
        return err(format!(
            "{} files changed: too many to scan for weakened tests",
            entries.len()
        ));
    }
    let mut all = vec![];
    for e in entries {
        let out = scratch.git(
            &[
                "diff",
                "-a",
                "-U0",
                "--no-renames",
                "--no-ext-diff",
                "--no-textconv",
                "--no-color",
                "--src-prefix=a/",
                "--dst-prefix=b/",
                base,
                commit,
                "--",
                &e.path,
            ],
            stop,
        )?;
        all.push(parse_hunks(&e.path, &String::from_utf8_lossy(&out)));
    }
    Ok(all)
}

fn parse_hunks(path: &str, patch: &str) -> FileEdit {
    let mut edit = FileEdit {
        path: path.to_owned(),
        ..FileEdit::default()
    };
    let count = |s: &str| -> usize {
        // `-12,3` or `+7` (a missing count is 1)
        s.get(1..)
            .unwrap_or("")
            .split_once(',')
            .map_or(1, |(_, n)| n.parse().unwrap_or(0))
    };
    let mut lines = patch.lines();
    while let Some(l) = lines.next() {
        // outside a hunk only a hunk header counts: everything else (file headers) is skipped
        let Some(rest) = l.strip_prefix("@@ ") else {
            continue;
        };
        let mut it = rest.split(' ');
        let (Some(old), Some(new)) = (it.next(), it.next()) else {
            continue;
        };
        let (mut rem, mut add) = (count(old), count(new));
        while rem + add > 0 {
            let Some(line) = lines.next() else { break };
            if line.starts_with('\\') {
                continue;
            } else if let (true, Some(t)) = (rem > 0, line.strip_prefix('-')) {
                edit.removed.push(t.to_owned());
                rem -= 1;
            } else if let (true, Some(t)) = (add > 0, line.strip_prefix('+')) {
                edit.added.push(t.to_owned());
                add -= 1;
            } else {
                break;
            }
        }
    }
    edit
}

/// Inline tests live in the same file as the code in some languages (Rust): removing a test
/// function or an assertion from any `.rs` file is a weakening, whatever the file is called.
fn inline_tests_weakened(edits: &[FileEdit]) -> bool {
    let tests = |l: &String| {
        let t = l.trim();
        t.starts_with("#[test") || t.starts_with("#[tokio::test") || t.starts_with("#[cfg(test")
    };
    let asserts = |l: &String| {
        let t = l.trim();
        [
            "assert!",
            "assert_eq!",
            "assert_ne!",
            "debug_assert",
            "assert_matches!",
        ]
        .iter()
        .any(|a| t.contains(a))
    };
    edits.iter().filter(|e| e.path.ends_with(".rs")).any(|e| {
        e.removed.iter().filter(|l| tests(l)).count() > e.added.iter().filter(|l| tests(l)).count()
            || e.removed.iter().filter(|l| asserts(l)).count()
                > e.added.iter().filter(|l| asserts(l)).count()
    })
}

/// Files whose change can make a check pass without testing anything (build scripts, test
/// configuration, toolchain pins) beyond the shared definition list.
fn extra_definition(path: &str) -> bool {
    let low = path.replace('\\', "/").to_lowercase();
    let name = low.rsplit('/').next().unwrap_or("");
    matches!(
        name,
        "build.rs" | "conftest.py" | "pytest.ini" | "tox.ini" | "setup.cfg" | "noxfile.py"
    ) || low.starts_with(".cargo/")
        || low.contains("/.cargo/")
        || name.starts_with("rust-toolchain")
}

/// Everything a commit changed against `base`, with the flags it earns: definition files or test
/// weakening (`definitions_changed`, `tests_weakened`), a changed file mode (`mode_changed`).
pub fn inspect_change(
    repo: &Path,
    scratch_dir: PathBuf,
    base: &str,
    commit: &str,
    stop: &AtomicBool,
) -> Result<ChangeReport, VerifyError> {
    let scratch = Scratch::new(repo, scratch_dir, stop)?;
    let ents = entries(&scratch, base, commit, stop)?;
    let mut r = ChangeReport {
        files: ents.iter().map(|e| e.path.clone()).collect(),
        ..ChangeReport::default()
    };
    for e in &ents {
        if matches!(e.new_mode.as_str(), "120000" | "160000") {
            r.special.push(e.path.clone());
        }
    }
    if ents
        .iter()
        .any(|e| e.status == 'M' && e.old_mode != e.new_mode)
    {
        r.flags.push("mode_changed");
    }
    if r.files
        .iter()
        .any(|f| is_definition(f) || extra_definition(f))
    {
        r.flags.push("definitions_changed");
    }
    let ed = edits(&scratch, base, commit, &ents, stop)?;
    if weakened_tests(&ed).is_some()
        || removed_prerequisite_guards(&ed).is_some()
        || inline_tests_weakened(&ed)
    {
        r.flags.push("tests_weakened");
    }
    Ok(r)
}

/// Where a throwaway scratch repository for `inspect_change` can live.
pub fn temp_scratch() -> PathBuf {
    std::env::temp_dir().join(format!("synt-scratch-{}", uuid::Uuid::new_v4()))
}

/// Files changed between `base` and `commit` (relative, `/` separators).
pub fn changed_files(repo: &Path, base: &str, commit: &str) -> Result<Vec<String>, VerifyError> {
    Ok(inspect_change(repo, temp_scratch(), base, commit, &AtomicBool::new(false))?.files)
}

/// The line diff of each changed file, for the weakening scan.
pub fn file_edits(repo: &Path, base: &str, commit: &str) -> Result<Vec<FileEdit>, VerifyError> {
    let stop = AtomicBool::new(false);
    let scratch = Scratch::new(repo, temp_scratch(), &stop)?;
    let ents = entries(&scratch, base, commit, &stop)?;
    edits(&scratch, base, commit, &ents, &stop)
}

/// The flags the changes of `base..commit` earn.
pub fn change_flags(
    repo: &Path,
    base: &str,
    commit: &str,
) -> Result<Vec<&'static str>, VerifyError> {
    Ok(inspect_change(repo, temp_scratch(), base, commit, &AtomicBool::new(false))?.flags)
}
