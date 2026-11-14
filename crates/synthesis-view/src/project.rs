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

use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::Path;
use std::time::{Duration, Instant};
use synthesis_harness::sanitize_for_display as clean_line;

use synthesis_cli_ui_base::imgclip::{Output, RealExec};
use synthesis_config::Env;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TreeRow {
    pub depth: u16,
    pub name: String,
    pub dir: bool,
    /// `git status --porcelain` letter: `M`, `A`, `D`, `R`, `?`.
    pub status: Option<char>,
    /// A symbolic link: shown, never entered and never followed.
    pub link: bool,
    /// A build or vendor directory (`target`, `node_modules`, a valid `CACHEDIR.TAG`): listed only when opened.
    pub generated: bool,
    /// Not a file or a folder but the "... N more" row of a directory cut at [`DIR_ROWS`]: N entries are left.
    pub more: usize,
    /// A line of text, not an entry (see [`notice_row`]): not selectable, not opened.
    pub notice: bool,
}

/// What a lazy listing needs to know besides the disk: `git status` (letters, ignored paths) when the folder is
/// a repository in a trusted folder.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Listing {
    pub git: bool,
    pub status: HashMap<String, char>,
    pub ignored: HashSet<String>,
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
    /// Kept for listing the directories opened later (see [`scan_lazy`]).
    pub listing: Listing,
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
                ..Default::default()
            });
            open.push(name);
        }
        rows.push(TreeRow {
            depth: dirs.len() as u16,
            name: parts[parts.len() - 1].to_string(),
            dir: false,
            status: status.get(p).copied(),
            ..Default::default()
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

/// Entries of one directory listed at a time; the rest is behind a "... N more" row.
pub const DIR_ROWS: usize = 1000;
/// Entries of one directory looked at at most (a directory of millions of files is not read whole).
const DIR_ENTRIES: usize = 200_000;
/// Directories that hold build output or vendored code; listed only when the person opens them.
const GENERATED: &[&str] = &["target", "node_modules", ".venv", "dist", "build"];
/// First line of a valid `CACHEDIR.TAG` (https://bford.info/cachedir/), which cargo writes into `target/`.
const CACHEDIR_SIGNATURE: &[u8; 43] = b"Signature: 8a477f597d28d172789f06886806bc55";
/// Open directories read again by one scan (Refresh), and the changed-file parents opened by the first one.
const MAX_OPEN: usize = 100;

/// One page of a directory: its entries from `skip` (at most `limit`) and how many it has in all.
pub struct Children {
    pub rows: Vec<TreeRow>,
    pub total: usize,
    /// Entries are missing from the directory: more than 200 000, or names that are not printable text.
    pub partial: bool,
}

/// A directory as it was read: the entries that are listed, sorted. The engine keeps the last one to page from.
pub struct Kids {
    dir: String,
    names: Vec<(String, bool, bool)>,
    partial: bool,
}

/// The directory `comps` of `root` and what is needed to look into it WITHOUT following a link, even one put
/// in place after a check (unix: a handle, then one `openat` with NOFOLLOW per component and the entries read
/// from the handle; elsewhere a path that was checked component by component).
#[cfg(unix)]
type Here = std::os::fd::OwnedFd;
#[cfg(not(unix))]
type Here = std::path::PathBuf;

#[cfg(unix)]
fn open_here(root: &Path, comps: &[&str]) -> Option<Here> {
    use rustix::fs::{Mode, OFlags, open, openat};
    // the folder itself may be a link (the person chose it); nothing below it may be one
    let mut cur = open(
        root,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .ok()?;
    for c in comps {
        cur = openat(
            &cur,
            *c,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .ok()?;
    }
    Some(cur)
}

#[cfg(not(unix))]
fn open_here(root: &Path, comps: &[&str]) -> Option<Here> {
    let mut here = root.to_path_buf();
    for c in comps {
        here.push(c);
        if !std::fs::symlink_metadata(&here).ok()?.is_dir() {
            return None;
        }
    }
    Some(here)
}

/// `(name, dir, link)` of the entries (at most [`DIR_ENTRIES`]), whether more were left, and how many names are
/// not text.
type Names = (Vec<(String, bool, bool)>, bool, usize);

#[cfg(unix)]
fn read_names(h: &Here) -> Option<Names> {
    use rustix::fs::{AtFlags, Dir, FileType, statat};
    let (mut out, mut cut, mut odd) = (vec![], false, 0);
    for e in Dir::read_from(h).ok()? {
        let Ok(e) = e else { continue };
        let raw = e.file_name();
        if matches!(raw.to_bytes(), b"." | b"..") {
            continue;
        }
        if out.len() >= DIR_ENTRIES {
            cut = true;
            break;
        }
        let Ok(name) = raw.to_str() else {
            odd += 1;
            continue;
        };
        let ft = match e.file_type() {
            FileType::Unknown => statat(h, raw, AtFlags::SYMLINK_NOFOLLOW)
                .map_or(FileType::Unknown, |st| FileType::from_raw_mode(st.st_mode)),
            t => t,
        };
        out.push((
            name.to_string(),
            ft == FileType::Directory,
            ft == FileType::Symlink,
        ));
    }
    Some((out, cut, odd))
}

#[cfg(not(unix))]
fn read_names(h: &Here) -> Option<Names> {
    let (mut out, mut cut, mut odd) = (vec![], false, 0);
    for e in std::fs::read_dir(h).ok()?.flatten() {
        if out.len() >= DIR_ENTRIES {
            cut = true;
            break;
        }
        let Ok(ft) = e.file_type() else { continue };
        match e.file_name().into_string() {
            Ok(n) => out.push((n, ft.is_dir(), ft.is_symlink())),
            Err(_) => odd += 1,
        }
    }
    Some((out, cut, odd))
}

/// Whether the directory `name` of `h` holds a valid `CACHEDIR.TAG` (a regular file: a pipe of that name
/// must not block, so it is opened NONBLOCK and judged by `fstat`).
#[cfg(unix)]
fn has_cachedir_tag(h: &Here, name: &str) -> bool {
    use rustix::fs::{Mode, OFlags, openat};
    use std::io::Read as _;
    let flags = OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
    let Ok(sub) = openat(h, name, flags | OFlags::DIRECTORY, Mode::empty()) else {
        return false;
    };
    let Ok(tag) = openat(
        &sub,
        "CACHEDIR.TAG",
        flags | OFlags::NONBLOCK,
        Mode::empty(),
    ) else {
        return false;
    };
    let mut f = std::fs::File::from(tag);
    let mut sig = [0u8; 43];
    f.metadata().is_ok_and(|m| m.is_file())
        && f.read_exact(&mut sig).is_ok()
        && &sig == CACHEDIR_SIGNATURE
}

#[cfg(not(unix))]
fn has_cachedir_tag(h: &Here, name: &str) -> bool {
    use std::io::Read as _;
    let tag = h.join(name).join("CACHEDIR.TAG");
    let mut sig = [0u8; 43];
    std::fs::symlink_metadata(&tag).is_ok_and(|m| m.is_file())
        && std::fs::File::open(&tag)
            .and_then(|mut f| f.read_exact(&mut sig))
            .is_ok()
        && &sig == CACHEDIR_SIGNATURE
}

fn comps_of(dir: &str) -> Option<Vec<&str>> {
    let comps: Vec<&str> = dir.split('/').filter(|c| !c.is_empty()).collect();
    (!comps.iter().any(|c| matches!(*c, "." | ".." | ".git"))).then_some(comps)
}

/// A build directory is not hidden by .gitignore but not listed either: it is one closed row. Where git
/// answered, a directory of that name that is tracked is an ordinary directory. The label can come from a
/// `CACHEDIR.TAG` the repository itself holds: it only collapses the row, nothing is run or trusted by it.
fn is_generated(h: &Here, listing: &Listing, path: &str, name: &str) -> bool {
    (GENERATED.contains(&name) || has_cachedir_tag(h, name))
        && (!listing.git
            || listing.ignored.contains(path)
            || listing.status.get(path) == Some(&'?'))
}

/// Reads the directory `dir` (relative, `/`-separated, "" is `root`) NOW from the disk, so a closed or huge
/// directory costs nothing until it is opened. `None`: it is not a directory of the project (a link on the way
/// is not entered, it could lead out of the folder or round in a loop). Entries `git` ignores are left out
/// except build directories; links are listed as links; a name that is not printable text cannot be opened
/// again by the name shown, so it is left out and the directory is `partial`.
pub fn read_children(root: &Path, dir: &str, listing: &Listing) -> Option<Kids> {
    let here = open_here(root, &comps_of(dir)?)?;
    let (mut names, cut, mut odd) = read_names(&here)?;
    names.retain(|n| n.0 != ".git");
    let all = names.len();
    names.retain(|n| clean_line(&n.0) == n.0);
    odd += all - names.len();
    let prefix = if dir.is_empty() {
        String::new()
    } else {
        format!("{dir}/")
    };
    // files git reports as deleted are not on the disk but are still part of the project
    let on_disk: HashSet<&str> = names.iter().map(|k| k.0.as_str()).collect();
    let gone: Vec<(String, bool, bool)> = listing
        .status
        .iter()
        .filter(|(_, c)| **c == 'D')
        .filter_map(|(p, _)| p.strip_prefix(&prefix))
        .filter(|n| !n.contains('/') && !on_disk.contains(n))
        .map(|n| (n.to_string(), false, false))
        .collect();
    names.extend(gone);
    names.sort();
    names.retain(|(n, d, _)| {
        let p = format!("{prefix}{n}");
        !listing.ignored.contains(&p) || (*d && is_generated(&here, listing, &p, n))
    });
    Some(Kids {
        dir: dir.to_string(),
        names,
        partial: cut || odd > 0,
    })
}

impl Kids {
    /// Whether this is the reading of `dir`.
    pub fn is_dir(&self, dir: &str) -> bool {
        self.dir == dir
    }

    /// The rows from entry `skip`, at most `limit`. `None`: the directory is no longer there.
    pub fn page(
        &self,
        root: &Path,
        listing: &Listing,
        skip: usize,
        limit: usize,
    ) -> Option<Children> {
        let comps = comps_of(&self.dir)?;
        let here = open_here(root, &comps)?;
        let depth = comps.len() as u16;
        let prefix = if self.dir.is_empty() {
            String::new()
        } else {
            format!("{}/", self.dir)
        };
        let rows = self
            .names
            .iter()
            .skip(skip)
            .take(limit)
            .map(|(n, d, l)| {
                let p = format!("{prefix}{n}");
                TreeRow {
                    depth,
                    name: n.clone(),
                    dir: *d,
                    status: listing.status.get(&p).copied(),
                    link: *l,
                    generated: *d && is_generated(&here, listing, &p, n),
                    ..Default::default()
                }
            })
            .collect();
        Some(Children {
            rows,
            total: self.names.len(),
            partial: self.partial,
        })
    }
}

/// [`read_children`] and one page of it.
pub fn list_children(
    root: &Path,
    dir: &str,
    listing: &Listing,
    skip: usize,
    limit: usize,
) -> Option<Children> {
    read_children(root, dir, listing)?.page(root, listing, skip, limit)
}

/// The "... N more" row that ends a directory cut after its first entries.
pub fn more_row(depth: u16, left: usize) -> TreeRow {
    TreeRow {
        depth,
        name: format!("\u{2026} {left} more"),
        more: left,
        ..Default::default()
    }
}

/// A line of text in the tree that is not an entry (a folder that could not be read, entries left out).
pub fn notice_row(depth: u16, text: &str) -> TreeRow {
    TreeRow {
        depth,
        name: format!("\u{2026} {text}"),
        notice: true,
        ..Default::default()
    }
}

/// What ends a page of a directory: the "... more" row when entries are left, else the note that some entries
/// can never be shown.
pub fn page_end(rows: &mut Vec<TreeRow>, depth: u16, left: usize, partial: bool) {
    if left > 0 {
        rows.push(more_row(depth, left));
    } else if partial {
        rows.push(notice_row(
            depth,
            "some entries are not shown (too many, or names that are not text)",
        ));
    }
}

/// [`list_children`] from `skip` for the window: one [`DIR_ROWS`] page, with the "... more" row when cut.
pub fn children_rows(
    root: &Path,
    dir: &str,
    listing: &Listing,
    skip: usize,
) -> Option<Vec<TreeRow>> {
    let c = list_children(root, dir, listing, skip, DIR_ROWS)?;
    let left = c.total.saturating_sub(skip + c.rows.len());
    let depth = dir.split('/').filter(|c| !c.is_empty()).count() as u16;
    let mut rows = c.rows;
    page_end(&mut rows, depth, left, c.partial);
    Some(rows)
}

/// Paths of the rows (`dir/sub/file`), in row order.
pub fn row_paths(d: &ProjectData) -> Vec<String> {
    let mut stack: Vec<&str> = vec![];
    d.rows
        .iter()
        .map(|r| {
            stack.truncate(usize::from(r.depth));
            let p = stack
                .iter()
                .copied()
                .chain([r.name.as_str()])
                .collect::<Vec<_>>()
                .join("/");
            if r.dir {
                stack.push(&r.name);
            }
            p
        })
        .collect()
}

impl ProjectData {
    fn dir_at(&self, dir: &str) -> Option<usize> {
        let paths = row_paths(self);
        (0..self.rows.len()).find(|i| self.rows[*i].dir && paths[*i] == dir)
    }

    /// Entries of `dir` ("" is the project) that are already listed (the "... more" row not counted): where the
    /// next page starts.
    pub fn loaded(&self, dir: &str) -> usize {
        let Some((start, depth)) = self.span(dir) else {
            return 0;
        };
        self.rows[start..]
            .iter()
            .take_while(|r| r.depth >= depth)
            .filter(|r| r.depth == depth && r.more == 0 && !r.notice)
            .count()
    }

    /// Where the children of `dir` start in `rows` and their depth.
    fn span(&self, dir: &str) -> Option<(usize, u16)> {
        if dir.is_empty() {
            return Some((0, 0));
        }
        let at = self.dir_at(dir)?;
        Some((at + 1, self.rows[at].depth + 1))
    }

    /// Lists `kids` as the children of `dir`: instead of what was listed (an opened directory, Refresh), or after
    /// it (`append`: the page the "... more" row asked for). A directory that is not in the tree changes nothing.
    pub fn put_children(&mut self, dir: &str, mut kids: Vec<TreeRow>, append: bool) {
        let Some((start, depth)) = self.span(dir) else {
            return;
        };
        // an engine that answers with a whole subtree (older than this one) gives deeper rows: not children
        kids.retain(|r| r.depth == depth);
        let end = start
            + self.rows[start..]
                .iter()
                .take_while(|r| r.depth >= depth)
                .count();
        let mut from = if append { end } else { start };
        // the page asked for replaces the "... more" row and the notes that closed the last one
        while append
            && from > start
            && self.rows[from - 1].depth == depth
            && (self.rows[from - 1].more > 0 || self.rows[from - 1].notice)
        {
            from -= 1;
        }
        self.rows.splice(from..end, kids);
    }

    /// Lists again the directories in `open` (shallow ones first, so a deeper one finds its parent), each with
    /// `list`; one that is no longer in the tree is skipped.
    pub fn load_open(
        &mut self,
        open: &[String],
        mut list: impl FnMut(&str) -> Option<Vec<TreeRow>>,
    ) {
        let mut open: Vec<&String> = open.iter().collect();
        open.sort_by_key(|d| (d.matches('/').count(), d.as_str()));
        for d in open.into_iter().take(MAX_OPEN) {
            if self.dir_at(d).is_some() {
                let kids = list(d).unwrap_or_else(|| {
                    vec![notice_row(
                        d.matches('/').count() as u16 + 1,
                        "could not read this folder",
                    )]
                });
                self.put_children(d, kids, false);
            }
        }
    }
}

/// The tree of the file explorer, LAZY: the first level of the folder and the directories in `open` (`None`:
/// the parents of the changed files, for the first look) are listed; every other directory is read when the
/// person opens it ([`children_rows`]), so a huge `target/` or `node_modules/` never uses up the listing.
/// `trusted` runs the hardened git for the status letters and the ignored paths; otherwise nothing but the disk
/// is read (a folder that is not trusted can name programs in its `.git/config`).
pub fn scan_lazy(cwd: &Path, env: &Env, trusted: bool, open: Option<&[String]>) -> ProjectData {
    let mut data = ProjectData {
        loaded: true,
        ..Default::default()
    };
    let git = Git::new(env);
    if trusted
        && git
            .run(cwd, &["rev-parse", "--is-inside-work-tree"], 0)
            .is_some()
    {
        let guards = match git.filter_guards(cwd) {
            Ok(g) => g,
            Err(why) => {
                data.note = Some(format!("{why}; git was not run"));
                return data;
            }
        };
        let args = [
            "status",
            "--porcelain=v1",
            "-z",
            "--ignored",
            "--ignore-submodules=all",
        ];
        if let Some((st, cut)) = git.run_with(cwd, &guards, &args, 0) {
            data.listing.git = true;
            data.truncated = cut;
            for (p, c) in parse_status(&st) {
                let p = p.trim_end_matches('/').to_string();
                if c == '!' {
                    data.listing.ignored.insert(p);
                } else {
                    data.listing.status.insert(p, c);
                }
            }
        }
    }
    let listing = data.listing.clone();
    data.rows = children_rows(cwd, "", &listing, 0).unwrap_or_default();
    let open: Vec<String> = match open {
        Some(o) => o.to_vec(),
        None => {
            // the parents of every changed path, so the first look shows what changed
            let all: BTreeSet<&str> = listing
                .status
                .keys()
                .flat_map(|p| p.match_indices('/').map(|(i, _)| &p[..i]))
                .collect();
            all.into_iter().map(String::from).collect()
        }
    };
    data.load_open(&open, |d| children_rows(cwd, d, &listing, 0));
    data
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
        let letter = if x == '!' {
            '!' // ignored (only with `--ignored`)
        } else if x == '?' {
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
        // not a repository (or no git): the folder is still listed, by the walk that runs nothing
        return scan_plain(cwd);
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
        ..Default::default()
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
    fn a_folder_without_git_lists_its_files_not_an_empty_tree() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("a.txt"), "a").unwrap();
        let p = scan(d.path(), &env());
        assert!(p.loaded && p.note.is_none());
        assert_eq!(
            p.rows.iter().map(|r| r.name.as_str()).collect::<Vec<_>>(),
            ["a.txt"]
        );
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

    const SIG: &str = "Signature: 8a477f597d28d172789f06886806bc55\n# cargo\n";

    /// crates/x/src/*.rs, a `target/` of 20 000 files with a CACHEDIR.TAG, and a README.
    fn rust_workspace(w: &Path) {
        std::fs::create_dir_all(w.join("crates/x/src")).unwrap();
        for f in ["lib.rs", "a.rs"] {
            std::fs::write(w.join("crates/x/src").join(f), "x").unwrap();
        }
        std::fs::write(w.join("README.md"), "x").unwrap();
        let deps = w.join("target/debug/deps");
        std::fs::create_dir_all(&deps).unwrap();
        std::fs::write(w.join("target/CACHEDIR.TAG"), SIG).unwrap();
        for i in 0..20_000 {
            std::fs::write(deps.join(format!("o{i}.o")), "").unwrap();
        }
    }

    fn names(rows: &[TreeRow]) -> Vec<String> {
        rows.iter().map(|r| r.name.clone()).collect()
    }

    fn lazy_checks(w: &Path, trusted: bool) {
        let t = Instant::now();
        let mut d = scan_lazy(w, &env(), trusted, Some(&[]));
        assert!(
            t.elapsed() < Duration::from_secs(5),
            "the first scan does not read target: {:?}",
            t.elapsed()
        );
        assert_eq!(names(&d.rows), ["README.md", "crates", "target"]);
        let target = d.rows.iter().find(|r| r.name == "target").unwrap();
        assert!(target.dir && target.generated, "{target:?}");
        assert!(d.rows.len() < 10, "{:?}", names(&d.rows));
        // opening the directories one after the other shows the files
        let l = d.listing.clone();
        for dir in ["crates", "crates/x", "crates/x/src"] {
            let kids = children_rows(w, dir, &l, 0).unwrap();
            d.put_children(dir, kids, false);
        }
        let paths = row_paths(&d);
        assert!(
            paths.contains(&"crates/x/src/lib.rs".to_string()),
            "{paths:?}"
        );
        assert!(paths.contains(&"crates/x/src/a.rs".to_string()));
        // Refresh: the same directories listed again by one scan
        let open: Vec<String> = ["crates", "crates/x", "crates/x/src"]
            .map(String::from)
            .into();
        let d2 = scan_lazy(w, &env(), trusted, Some(&open));
        assert_eq!(row_paths(&d2), paths);
    }

    #[test]
    fn a_huge_untracked_target_does_not_use_up_the_listing_without_git() {
        let d = tempfile::tempdir().unwrap();
        rust_workspace(d.path());
        lazy_checks(d.path(), false);
        lazy_checks(d.path(), true); // trusted, but not a repository: the same
    }

    #[test]
    fn a_huge_untracked_target_does_not_use_up_the_listing_in_a_repository() {
        let d = tempfile::tempdir().unwrap();
        rust_workspace(d.path());
        run(d.path(), &["init", "-q"]);
        lazy_checks(d.path(), true);
        let g = scan_lazy(d.path(), &env(), true, Some(&[]));
        assert!(g.listing.git);
        assert_eq!(
            g.rows.iter().find(|r| r.name == "target").unwrap().status,
            Some('?')
        );
    }

    #[test]
    fn ignored_entries_are_hidden_but_an_ignored_build_directory_is_one_closed_row() {
        let d = tempfile::tempdir().unwrap();
        let w = d.path();
        run(w, &["init", "-q"]);
        std::fs::write(w.join(".gitignore"), "target/\n*.log\nout/\n").unwrap();
        for dir in ["target", "out", "build", "src"] {
            std::fs::create_dir(w.join(dir)).unwrap();
            std::fs::write(w.join(dir).join("f"), "x").unwrap();
        }
        std::fs::write(w.join("a.log"), "x").unwrap();
        std::fs::write(w.join("build/tracked.txt"), "x").unwrap();
        run(w, &["add", "build", "src", ".gitignore"]);
        let d = scan_lazy(w, &env(), true, Some(&[]));
        let row = |n: &str| d.rows.iter().find(|r| r.name == n);
        assert!(
            row("a.log").is_none() && row("out").is_none(),
            "{:?}",
            names(&d.rows)
        );
        assert!(row("target").unwrap().generated);
        assert!(
            !row("build").unwrap().generated,
            "a tracked build/ is ordinary"
        );
        assert!(!row("src").unwrap().generated);
        assert_eq!(row(".gitignore").unwrap().status, Some('A'));
    }

    #[test]
    fn a_directory_of_thousands_of_entries_is_paged_by_a_more_row() {
        let d = tempfile::tempdir().unwrap();
        let w = d.path();
        std::fs::create_dir_all(w.join("big")).unwrap();
        for i in 0..6000 {
            std::fs::write(w.join(format!("big/f{i:05}")), "").unwrap();
        }
        let mut t = scan_lazy(w, &env(), false, Some(&[]));
        let first = children_rows(w, "big", &t.listing, 0).unwrap();
        t.put_children("big", first, false);
        assert_eq!(t.rows.len(), 1 + DIR_ROWS + 1);
        let last = t.rows.last().unwrap();
        assert_eq!((last.more, last.depth), (6000 - DIR_ROWS, 1), "{last:?}");
        assert!(last.name.contains("more"));
        // the click: the next page goes after the loaded entries and replaces the more row
        while t.rows.last().is_some_and(|r| r.more > 0) {
            let kids = children_rows(w, "big", &t.listing, t.loaded("big")).unwrap();
            t.put_children("big", kids, true);
        }
        assert_eq!(t.rows.len(), 1 + 6000);
        assert_eq!(t.loaded("big"), 6000);
        assert_eq!(t.rows[6000].name, "f05999");
    }

    #[cfg(unix)]
    #[test]
    fn links_are_listed_as_links_and_never_entered() {
        let d = tempfile::tempdir().unwrap();
        let w = d.path().join("w");
        let out = d.path().join("out");
        std::fs::create_dir_all(w.join("sub")).unwrap();
        std::fs::create_dir_all(&out).unwrap();
        std::fs::write(out.join("secret.txt"), "x").unwrap();
        std::fs::write(w.join("sub/a.rs"), "x").unwrap();
        std::os::unix::fs::symlink(&out, w.join("escape")).unwrap();
        std::os::unix::fs::symlink(&w, w.join("sub/loop")).unwrap();
        std::os::unix::fs::symlink(w.join("sub"), w.join("alias")).unwrap();
        let t = scan_lazy(&w, &env(), false, Some(&[]));
        for n in ["escape", "alias"] {
            let r = t.rows.iter().find(|r| r.name == n).unwrap();
            assert!(r.link && !r.dir, "{r:?}");
        }
        for dir in ["escape", "alias", "escape/x"] {
            assert!(children_rows(&w, dir, &t.listing, 0).is_none(), "{dir}");
        }
        let sub = children_rows(&w, "sub", &t.listing, 0).unwrap();
        assert_eq!(names(&sub), ["a.rs", "loop"]);
        assert!(sub[1].link && !sub[1].dir);
        assert!(children_rows(&w, "sub/loop", &t.listing, 0).is_none());
        for bad in ["..", "sub/../..", ".git"] {
            assert!(children_rows(&w, bad, &t.listing, 0).is_none(), "{bad}");
        }
    }

    #[test]
    fn a_deleted_tracked_file_and_a_changed_one_are_listed_and_their_parents_open() {
        let d = tempfile::tempdir().unwrap();
        let w = d.path();
        run(w, &["init", "-q"]);
        std::fs::create_dir_all(w.join("src/deep")).unwrap();
        std::fs::write(w.join("src/a.rs"), "one\n").unwrap();
        std::fs::write(w.join("src/gone.rs"), "x\n").unwrap();
        std::fs::write(w.join("src/deep/b.rs"), "x\n").unwrap();
        std::fs::write(w.join("top.txt"), "x\n").unwrap();
        run(w, &["add", "."]);
        run(w, &["commit", "-qm", "x"]);
        std::fs::write(w.join("src/deep/b.rs"), "two\n").unwrap();
        std::fs::remove_file(w.join("src/gone.rs")).unwrap();
        let t = scan_lazy(w, &env(), true, None);
        let seen: Vec<_> = t.rows.iter().map(|r| (r.name.as_str(), r.status)).collect();
        assert_eq!(
            seen,
            [
                ("src", None),
                ("a.rs", None),
                ("deep", None),
                ("b.rs", Some('M')),
                ("gone.rs", Some('D')),
                ("top.txt", None)
            ]
        );
    }

    #[test]
    fn fifty_thousand_deleted_files_in_one_directory_are_listed_in_linear_time() {
        let d = tempfile::tempdir().unwrap();
        let w = d.path();
        std::fs::create_dir_all(w.join("d")).unwrap();
        for i in 0..2000 {
            std::fs::write(w.join(format!("d/live{i}")), "").unwrap();
        }
        let mut l = Listing::default();
        for i in 0..50_000 {
            l.status.insert(format!("d/gone{i}"), 'D');
        }
        // a deleted file that is still on the disk is listed once
        l.status.insert("d/live7".into(), 'D');
        let t = Instant::now();
        let c = list_children(w, "d", &l, 0, DIR_ROWS).unwrap();
        assert!(t.elapsed() < Duration::from_secs(1), "{:?}", t.elapsed());
        assert_eq!(c.total, 52_000);
    }

    // Windows cannot create a file name with a control character
    #[cfg(unix)]
    #[test]
    fn a_name_that_cannot_be_shown_as_it_is_leaves_the_directory_partial_and_says_so() {
        let d = tempfile::tempdir().unwrap();
        let w = d.path();
        std::fs::write(w.join("ok.txt"), "").unwrap();
        std::fs::write(w.join("bad\u{1b}[31mname"), "").unwrap();
        let rows = children_rows(w, "", &Listing::default(), 0).unwrap();
        assert_eq!(names(&rows[..1]), ["ok.txt"]);
        assert!(
            rows[1].notice && rows[1].name.contains("not shown"),
            "{rows:?}"
        );
        assert_eq!(rows.len(), 2);
        // a page after it replaces the closing note
        let mut t = ProjectData {
            rows: rows.clone(),
            ..Default::default()
        };
        t.put_children(
            "",
            vec![TreeRow {
                name: "z".into(),
                ..Default::default()
            }],
            true,
        );
        assert_eq!(names(&t.rows), ["ok.txt", "z"]);
    }

    #[test]
    fn a_failed_listing_of_an_open_directory_leaves_a_row_that_says_so() {
        let mut d = ProjectData {
            rows: vec![TreeRow {
                name: "gone".into(),
                dir: true,
                ..Default::default()
            }],
            ..Default::default()
        };
        d.load_open(&["gone".to_string()], |_| None);
        assert_eq!(d.rows.len(), 2);
        assert!(
            d.rows[1].notice && d.rows[1].depth == 1 && d.rows[1].name.contains("could not read")
        );
    }

    #[test]
    fn rows_of_an_older_engine_that_sent_a_whole_subtree_keep_only_the_children() {
        let r = |depth, name: &str, dir| TreeRow {
            depth,
            name: name.into(),
            dir,
            ..Default::default()
        };
        let mut d = ProjectData {
            rows: vec![r(0, "src", true), r(0, "z", false)],
            ..Default::default()
        };
        d.put_children(
            "src",
            vec![r(1, "a.rs", false), r(1, "deep", true), r(2, "b.rs", false)],
            false,
        );
        assert_eq!(names(&d.rows), ["src", "a.rs", "deep", "z"]);
    }

    #[cfg(unix)]
    #[test]
    fn a_cachedir_tag_that_is_a_pipe_or_a_link_is_not_read() {
        let d = tempfile::tempdir().unwrap();
        let w = d.path();
        for n in ["pipe", "link", "real"] {
            std::fs::create_dir(w.join(n)).unwrap();
        }
        std::fs::write(w.join("real/CACHEDIR.TAG"), SIG).unwrap();
        std::os::unix::fs::symlink(w.join("real/CACHEDIR.TAG"), w.join("link/CACHEDIR.TAG"))
            .unwrap();
        assert!(
            Command::new("mkfifo")
                .arg(w.join("pipe/CACHEDIR.TAG"))
                .status()
                .unwrap()
                .success()
        );
        let rows = children_rows(w, "", &Listing::default(), 0).unwrap();
        let g = |n: &str| rows.iter().find(|r| r.name == n).unwrap().generated;
        assert!(g("real") && !g("link") && !g("pipe"));
    }
}
