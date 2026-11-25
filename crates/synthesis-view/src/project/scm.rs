//! Source Control of the file explorer: what is staged and what is changed, stage, unstage and commit. The only
//! place that WRITES to the user's repository, so it is narrow: every call is the hardened [`Git`] of `project`
//! (no fsmonitor, external diff, textconv, filter drivers, pager, ssh or credential programs; `core.hooksPath`
//! points at nothing, so no hook runs; no terminal prompt; `--literal-pathspecs`, paths after `--`, never a shell;
//! closed environment; process-group kill on a timeout). The repository is untrusted data:
//! - a secret-shaped file is never staged (its content is never read);
//! - a path that leaves the project (`..`, a link to somewhere else) is refused;
//! - the state is read again at the moment of the click, and only a path still in the listed state is acted on;
//! - the commit message is stripped of control characters, capped, and refused when it holds a secret shape;
//! - the author is the user's git config (`user.useConfigOnly`: git does not invent one from the host name);
//! - a repository config that names a signing program is refused (it would run a program of the repository);
//! - git's error text is shown sanitised, redacted and capped.

use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use super::filediff::{gitdir_ok, repo_ok, safe_rel};
use super::{Git, clean_line};
use synthesis_config::Env;

/// Entries of each group the list keeps.
const MAX_ENTRIES: usize = 500;
const WRITE_TIMEOUT: Duration = Duration::from_secs(30);
/// Bytes of a commit message (git's own limit is far higher; a message this long is not typed).
pub const MAX_MESSAGE: usize = 4096;
const MAX_NOTE: usize = 300;
/// Paths per `git add` (a Windows command line is 32 K characters).
const CHUNK: usize = 100;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    pub path: String,
    /// M modified, A added, D deleted, R renamed, `?` untracked, `!` conflicted.
    pub letter: char,
    /// Looks like a secret file: shown by name, never staged.
    pub secret: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Scm {
    /// `main`, `detached at 1a2b3c4` or `main (no commits yet)`.
    pub head: String,
    pub staged: Vec<Entry>,
    pub changes: Vec<Entry>,
    /// A group was cut at the display limit.
    pub cut: bool,
}

pub enum Act {
    Stage(String),
    Unstage(String),
    StageAll,
    Commit(String),
}

fn secret_in(root: &Path, rel: &str) -> bool {
    synthesis_core::secrets::is_secret_shaped_in(Path::new(rel), root)
        || synthesis_core::secrets::is_secret_target_in(&root.join(rel), root)
}

/// A git that may be run here: trusted folder, a `.git` that stays inside it, a work tree that is this folder.
/// Also the top folder of the work tree: the paths git reports (and takes back) are relative to it, so every later
/// call runs there, not in the subfolder the app may have been opened on.
fn ready(cwd: &Path, env: &Env, trusted: bool) -> Result<(Git, Vec<String>, PathBuf), String> {
    if !trusted {
        return Err("the folder is not trusted: git is not run here".into());
    }
    gitdir_ok(cwd)?;
    let git = Git::new(env);
    if !repo_ok(&git, cwd)? {
        return Err("not a git repository".into());
    }
    let guards = git
        .filter_guards(cwd)
        .map_err(|why| format!("{why}; git was not run"))?;
    let top = git
        .run(cwd, &["rev-parse", "--show-toplevel"], b'\n')
        .and_then(|(o, _)| std::fs::canonicalize(String::from_utf8_lossy(&o).trim()).ok())
        .ok_or("the work tree could not be found")?;
    Ok((git, guards, top))
}

/// The state of the repository: `status --porcelain=v1 -z -b`, every untracked file by itself.
pub fn status(cwd: &Path, env: &Env, trusted: bool) -> Result<Scm, String> {
    let (git, guards, top) = ready(cwd, env, trusted)?;
    read(&git, &guards, &top)
}

fn read(git: &Git, guards: &[String], cwd: &Path) -> Result<Scm, String> {
    let args = [
        "status",
        "--porcelain=v1",
        "-z",
        "-b",
        "--untracked-files=all",
        "--ignore-submodules=all",
    ];
    let (raw, cut) = git
        .run_with(cwd, guards, &args, 0)
        .ok_or("git could not produce the status")?;
    let mut s = parse(&raw, cwd);
    s.cut |= cut;
    if s.head == "detached" {
        let sha = git
            .run(cwd, &["rev-parse", "--short", "HEAD"], b'\n')
            .map(|(o, _)| clean_line(String::from_utf8_lossy(&o).trim()))
            .unwrap_or_default();
        s.head = format!("detached at {sha}");
    }
    Ok(s)
}

fn parse(st: &[u8], root: &Path) -> Scm {
    let mut s = Scm::default();
    // only EMPTY records are skipped here: a filter on the length would let a rename origin of 1-3 bytes
    // go unconsumed below and swallow the entry after it
    let mut it = st.split(|c| *c == 0).filter(|p| !p.is_empty());
    while let Some(e) = it.next() {
        if e.len() < 4 {
            continue; // not `XY path`
        }
        let (x, y) = (e[0] as char, e[1] as char);
        let text = clean_line(&String::from_utf8_lossy(&e[3..]));
        if x == '#' {
            s.head = head_of(&text);
            continue;
        }
        if matches!(x, 'R' | 'C') || matches!(y, 'R' | 'C') {
            it.next(); // the origin of a rename or copy is a record of its own, whatever its length
        }
        if safe_rel(&text).is_err() {
            continue; // not a path that can be acted on
        }
        let entry = |letter| Entry {
            secret: secret_in(root, &text),
            path: text.clone(),
            letter,
        };
        let conflict = x == 'U' || y == 'U' || (x == y && matches!(x, 'A' | 'D'));
        if conflict {
            s.changes.push(entry('!'));
            continue;
        }
        if x == '?' {
            s.changes.push(entry('?'));
        }
        if !matches!(x, ' ' | '?' | '!') {
            s.staged.push(entry(if x == 'C' { 'A' } else { x }));
        }
        if !matches!(y, ' ' | '?' | '!') {
            s.changes.push(entry(if y == 'C' { 'A' } else { y }));
        }
    }
    for g in [&mut s.staged, &mut s.changes] {
        if g.len() > MAX_ENTRIES {
            g.truncate(MAX_ENTRIES);
            s.cut = true;
        }
    }
    s
}

/// `## main...origin/main [ahead 1]`, `## No commits yet on main`, `## HEAD (no branch)` without the `## `.
fn head_of(t: &str) -> String {
    let t = t.strip_prefix("## ").unwrap_or(t);
    if t.starts_with("HEAD (no branch)") {
        return "detached".into();
    }
    for p in ["No commits yet on ", "Initial commit on "] {
        if let Some(b) = t.strip_prefix(p) {
            return format!(
                "{} (no commits yet)",
                b.chars().take(80).collect::<String>()
            );
        }
    }
    let b = t.split("...").next().unwrap_or(t);
    b.split(" [").next().unwrap_or(b).chars().take(80).collect()
}

/// `path` must stay inside `root`: the folder it is in (the nearest one that exists, for a deleted file) and,
/// when it is a link, where the link leads. A link that leads nowhere is refused too.
fn inside(root: &Path, rel: &str) -> Result<(), String> {
    let no = || Err(format!("{rel}: leaves the project, not staged"));
    let Ok(root) = std::fs::canonicalize(root) else {
        return no();
    };
    let full = root.join(rel);
    if full
        .symlink_metadata()
        .is_ok_and(|m| m.file_type().is_symlink())
        && !std::fs::canonicalize(&full).is_ok_and(|r| r.starts_with(&root))
    {
        return no();
    }
    match full
        .ancestors()
        .skip(1)
        .find_map(|d| std::fs::canonicalize(d).ok())
    {
        Some(d) if d.starts_with(&root) => Ok(()),
        _ => no(),
    }
}

/// The commit message as git gets it: control characters replaced by a space, trimmed; refused when empty, too
/// long, or when it holds what looks like a secret.
pub fn clean_message(m: &str) -> Result<String, String> {
    let s: String = m
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    let s = s.trim().to_string();
    if s.is_empty() {
        return Err("the commit message is empty".into());
    }
    if s.len() > MAX_MESSAGE {
        return Err(format!(
            "the commit message is longer than {} KiB",
            MAX_MESSAGE / 1024
        ));
    }
    if crate::toolview::redact_text(&s) != s {
        return Err(
            "the commit message looks like it holds a secret; nothing was committed".into(),
        );
    }
    Ok(s)
}

/// Does the repository's own config (any scope below the user's) name a program for signing? Fails closed.
fn signing_program_in_repo(git: &Git, cwd: &Path) -> Result<(), String> {
    let o = git
        .raw(
            cwd,
            &[],
            &[
                "config",
                "--show-scope",
                "--get-regexp",
                r"^gpg\..*(program|command)$",
            ],
        )
        .filter(|o| !o.timed_out && !o.truncated);
    let text =
        |o: &synthesis_cli_ui_base::imgclip::Output| String::from_utf8_lossy(&o.stdout).to_string();
    match o {
        Some(o) if o.code == Some(1) && o.stdout.is_empty() => Ok(()),
        Some(o)
            if o.code == Some(0)
                && !text(&o)
                    .lines()
                    .any(|l| l.starts_with("local") || l.starts_with("worktree")) =>
        {
            Ok(())
        }
        Some(o) if o.code == Some(0) => Err(
            "this repository's config names a signing program; it would run here, so nothing was committed"
                .into(),
        ),
        _ => Err("the signing settings could not be checked; nothing was committed".into()),
    }
}

/// The paths of `git diff --cached` / `git diff-tree` (`--name-status -z --no-renames`): every record, raw, no cap
/// and no listing limit. `Ok((secret paths that are not deletions, records that could not be checked, count))`;
/// `Err` when the answer is missing or cut: what could not be checked is never committed.
fn name_status(
    git: &Git,
    guards: &[String],
    cwd: &Path,
    args: &[&str],
) -> Result<(Vec<String>, usize, usize), String> {
    // exit 0 and a whole answer, else a partial list could pass the gate
    let o = git
        .raw(cwd, guards, args)
        .filter(|o| !o.timed_out && o.code == Some(0))
        .ok_or("git could not list the staged files; nothing was committed")?;
    if o.truncated {
        return Err("the list of staged files is too long to check; nothing was committed".into());
    }
    let raw = o.stdout;
    let mut it = raw.split(|c| *c == 0).filter(|r| !r.is_empty());
    let (mut secret, mut skipped, mut n) = (vec![], 0, 0);
    while let Some(st) = it.next() {
        n += 1;
        // `--no-renames`: a status letter, then one path; an odd record is counted and not trusted
        let Some(path) = it.next() else {
            skipped += 1;
            break;
        };
        let path = String::from_utf8_lossy(path).to_string();
        // the secret test runs on EVERY record, before any decision about the path's shape
        if st != b"D" && secret_in(cwd, &path) {
            secret.push(clean_line(&path));
        }
        if st.len() != 1 || safe_rel(&path).is_err() || path.contains('\u{fffd}') {
            skipped += 1;
        }
    }
    Ok((secret, skipped, n))
}

/// The commit gate: reads the index itself (not the capped, parsed lists) and refuses unless every staged path
/// was checked and none is secret-shaped (a deletion is exempt). `Ok(count)` of staged paths.
fn commit_gate(git: &Git, guards: &[String], cwd: &Path) -> Result<usize, String> {
    let (secret, skipped, n) = name_status(
        git,
        guards,
        cwd,
        &[
            "diff",
            "--cached",
            "--name-status",
            "-z",
            "--no-renames",
            "--no-relative",
            "--no-ext-diff",
            "--no-textconv",
        ],
    )?;
    if n == 0 {
        return Err("nothing is staged".into());
    }
    if !secret.is_empty() {
        return Err(format!(
            "a staged file looks like a secret, unstage it first: {}",
            secret.join(", ")
        ));
    }
    if skipped > 0 {
        return Err(format!(
            "{skipped} staged path(s) could not be checked (odd characters in the name); nothing was committed"
        ));
    }
    Ok(n)
}

/// After a commit: did a secret-shaped path get in anyway? The index is checked before the commit and git then
/// reads it again, so a process that changes the index in between could slip a file in (accepted residual, see
/// docs/specs/desktop.md). The commit is never undone here; the note says how.
fn committed_secrets(git: &Git, guards: &[String], cwd: &Path) -> String {
    match name_status(
        git,
        guards,
        cwd,
        &[
            "diff-tree",
            "--root",
            "--no-commit-id",
            "--name-status",
            "-r",
            "-z",
            "--no-renames",
            "HEAD",
        ],
    ) {
        Ok((secret, _, _)) if secret.is_empty() => String::new(),
        Ok((secret, _, _)) => format!(
            "committed a secret-shaped file: {}. Undo with `git reset --soft HEAD~1`",
            secret.join(", ")
        ),
        Err(_) => "the committed files could not be checked for secrets".into(),
    }
}

/// git's complaint as one short line for the UI: control characters out, secret shapes masked, capped.
fn note_of(stderr: &[u8], stdout: &[u8], code: Option<i32>) -> String {
    let both = [stderr, stdout].map(String::from_utf8_lossy);
    let t = both
        .iter()
        .flat_map(|s| s.lines())
        .map(|l| crate::toolview::redact_text(&clean_line(l)))
        .filter(|l| !l.trim().is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    if t.is_empty() {
        return format!(
            "git failed (exit {})",
            code.map_or("?".into(), |c| c.to_string())
        );
    }
    match t.char_indices().nth(MAX_NOTE) {
        Some((i, _)) => format!("{}\u{2026}", &t[..i]),
        None => t,
    }
}

impl Git {
    /// One hardened git call that changes something: `Err` carries git's own words (sanitised) when it fails.
    fn write(&self, cwd: &Path, extra: &[String], args: &[&str]) -> Result<(), String> {
        let exe = self.exe.as_ref().ok_or("git is not installed")?;
        let mut cmd = Command::new(exe);
        cmd.args(self.argv(extra, args))
            .current_dir(cwd)
            .env_clear()
            .envs(self.env.iter().map(|(k, v)| (k, v)))
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        #[cfg(unix)]
        std::os::unix::process::CommandExt::process_group(&mut cmd, 0);
        #[cfg(windows)]
        std::os::windows::process::CommandExt::creation_flags(&mut cmd, 0x0800_0000);
        let mut child = cmd
            .spawn()
            .map_err(|e| format!("git could not be started: {}", clean_line(&e.to_string())))?;
        // both pipes are drained (the rest of a long text is dropped) so git never blocks on a full one
        fn drain(mut r: impl std::io::Read + Send + 'static) -> std::thread::JoinHandle<Vec<u8>> {
            std::thread::spawn(move || {
                let mut b = Vec::new();
                let _ = (&mut r).take(16 * 1024).read_to_end(&mut b);
                let _ = std::io::copy(&mut r, &mut std::io::sink());
                b
            })
        }
        let out = drain(child.stdout.take().expect("stdout was piped"));
        let err = drain(child.stderr.take().expect("stderr was piped"));
        let deadline = Instant::now() + WRITE_TIMEOUT;
        let code = loop {
            match child.try_wait() {
                Ok(Some(s)) => break s.code(),
                Ok(None) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(10))
                }
                _ => {
                    synthesis_cli_ui_base::imgclip::kill_tree(&mut child);
                    // never deleted by the app: only the person knows no other git is running
                    return Err("git did not finish in time and was stopped; a stale .git/index.lock may need removing by hand".into());
                }
            }
        };
        let (out, err) = (
            out.join().unwrap_or_default(),
            err.join().unwrap_or_default(),
        );
        if code == Some(0) {
            Ok(())
        } else {
            Err(note_of(&err, &out, code))
        }
    }
}

/// Does one click's work (see the module docs). `Ok` carries a note to show (what was skipped), maybe empty.
pub fn apply(cwd: &Path, env: &Env, trusted: bool, act: &Act) -> Result<String, String> {
    let msg = match act {
        Act::Commit(m) => Some(clean_message(m)?),
        Act::Stage(p) | Act::Unstage(p) => {
            safe_rel(p)?;
            None
        }
        Act::StageAll => None,
    };
    let (git, guards, top) = ready(cwd, env, trusted)?;
    let cwd = top.as_path();
    let now = read(&git, &guards, cwd)?;
    let stale = || "the file changed since the list was read; the list is refreshed".to_string();
    let add = |paths: &[&str]| -> Result<(), String> {
        for c in paths.chunks(CHUNK) {
            let mut a = vec!["--literal-pathspecs", "add", "--"];
            a.extend(c);
            git.write(cwd, &guards, &a)?;
        }
        Ok(())
    };
    match act {
        Act::Stage(p) => {
            let e = now
                .changes
                .iter()
                .find(|e| e.path == *p)
                .ok_or_else(stale)?;
            if e.secret {
                return Err(format!("{p}: looks like a secret file, not staged"));
            }
            if e.letter == '!' {
                return Err(format!(
                    "{p}: has a merge conflict; resolve it outside the app"
                ));
            }
            inside(cwd, p)?;
            add(&[p])?;
            Ok(String::new())
        }
        Act::StageAll => {
            let conflicts = now.changes.iter().filter(|e| e.letter == '!').count();
            let (skip, ok): (Vec<_>, Vec<_>) = now
                .changes
                .iter()
                .filter(|e| e.letter != '!')
                .partition(|e| e.secret);
            let ok: Vec<&str> = ok
                .iter()
                .map(|e| e.path.as_str())
                .filter(|p| inside(cwd, p).is_ok())
                .collect();
            let left = now.changes.len() - conflicts - skip.len() - ok.len();
            add(&ok)?;
            let mut note = vec![];
            if !skip.is_empty() {
                note.push(format!(
                    "{} secret-looking file(s) skipped: {}",
                    skip.len(),
                    skip.iter()
                        .map(|e| e.path.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                ));
            }
            if left > 0 {
                note.push(format!("{left} path(s) leaving the project skipped"));
            }
            if conflicts > 0 {
                note.push(format!(
                    "{conflicts} conflicted file(s) skipped: resolve them outside the app"
                ));
            }
            Ok(note.join("; "))
        }
        Act::Unstage(p) => {
            now.staged.iter().find(|e| e.path == *p).ok_or_else(stale)?;
            git.write(
                cwd,
                &guards,
                &["--literal-pathspecs", "reset", "-q", "--", p],
            )?;
            Ok(String::new())
        }
        Act::Commit(_) => {
            commit_gate(&git, &guards, cwd)?;
            signing_program_in_repo(&git, cwd)?;
            let mut extra = guards.clone();
            extra.extend(["-c".to_string(), "user.useConfigOnly=true".to_string()]);
            let msg = msg.expect("a commit has a message");
            // `--no-verify` is a second lock on the hooks `core.hooksPath` already switched off
            git.write(cwd, &extra, &["commit", "-q", "--no-verify", "-m", &msg])?;
            Ok(committed_secrets(&git, &guards, cwd))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env() -> Env {
        Env::from_pairs([(
            "PATH",
            Env::from_process()
                .get("PATH")
                .unwrap_or_default()
                .to_string(),
        )])
    }

    fn git(dir: &Path, args: &[&str]) {
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

    /// A repository with one commit; `HOME` of the test points at a folder with an identity, see `home`.
    fn repo() -> tempfile::TempDir {
        let d = tempfile::tempdir().unwrap();
        git(d.path(), &["init", "-q"]);
        std::fs::write(d.path().join("a.txt"), "one\n").unwrap();
        git(d.path(), &["add", "."]);
        git(d.path(), &["commit", "-q", "-m", "x"]);
        d
    }

    /// An env whose HOME holds the identity (the hardened git reads user config only from there).
    fn env_with_identity() -> (Env, tempfile::TempDir) {
        let h = tempfile::tempdir().unwrap();
        std::fs::write(
            h.path().join(".gitconfig"),
            "[user]\n\tname = T\n\temail = t@t\n",
        )
        .unwrap();
        let e = Env::from_pairs([
            (
                "PATH",
                Env::from_process()
                    .get("PATH")
                    .unwrap_or_default()
                    .to_string(),
            ),
            ("HOME", h.path().to_string_lossy().to_string()),
            ("USERPROFILE", h.path().to_string_lossy().to_string()),
        ]);
        (e, h)
    }

    fn st(d: &Path) -> Scm {
        status(d, &env(), true).unwrap()
    }

    fn paths(g: &[Entry]) -> Vec<(&str, char)> {
        g.iter().map(|e| (e.path.as_str(), e.letter)).collect()
    }

    #[test]
    fn the_two_groups_have_a_letter_per_file() {
        let d = repo();
        std::fs::write(d.path().join("a.txt"), "two\n").unwrap();
        std::fs::write(d.path().join("new.txt"), "n\n").unwrap();
        std::fs::write(d.path().join("added.txt"), "n\n").unwrap();
        git(d.path(), &["add", "added.txt"]);
        let s = st(d.path());
        assert_eq!(paths(&s.staged), [("added.txt", 'A')]);
        assert_eq!(paths(&s.changes), [("a.txt", 'M'), ("new.txt", '?')]);
        assert!(
            !s.head.is_empty() && !s.head.contains("detached"),
            "{}",
            s.head
        );
    }

    #[test]
    fn detached_head_and_an_empty_repository() {
        let d = repo();
        git(d.path(), &["checkout", "-q", "--detach"]);
        assert!(st(d.path()).head.starts_with("detached at "));
        let e = tempfile::tempdir().unwrap();
        git(e.path(), &["init", "-q"]);
        std::fs::write(e.path().join("f"), "x").unwrap();
        let s = st(e.path());
        assert!(s.head.ends_with("(no commits yet)"), "{}", s.head);
        assert_eq!(paths(&s.changes), [("f", '?')]);
    }

    #[test]
    fn stage_commit_and_unstage_work_and_the_lists_follow() {
        let d = repo();
        let (e, _h) = env_with_identity();
        let go = |a: Act| apply(d.path(), &e, true, &a);
        std::fs::write(d.path().join("a.txt"), "two\n").unwrap();
        std::fs::write(d.path().join("b.txt"), "b\n").unwrap();
        go(Act::Stage("a.txt".into())).unwrap();
        assert_eq!(
            paths(&status(d.path(), &e, true).unwrap().staged),
            [("a.txt", 'M')]
        );
        go(Act::Unstage("a.txt".into())).unwrap();
        assert!(status(d.path(), &e, true).unwrap().staged.is_empty());
        assert!(
            go(Act::Commit("m".into()))
                .unwrap_err()
                .contains("nothing is staged")
        );
        go(Act::StageAll).unwrap();
        let s = status(d.path(), &e, true).unwrap();
        assert_eq!(s.staged.len(), 2);
        assert!(s.changes.is_empty());
        go(Act::Commit("-x two files".into())).unwrap();
        let s = status(d.path(), &e, true).unwrap();
        assert!(s.staged.is_empty() && s.changes.is_empty());
        let log = Command::new("git")
            .args(["log", "-1", "--format=%s"])
            .current_dir(d.path())
            .output()
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&log.stdout).trim(), "-x two files");
    }

    #[test]
    fn unstage_works_before_the_first_commit_and_a_deletion_stages() {
        let d = tempfile::tempdir().unwrap();
        git(d.path(), &["init", "-q"]);
        std::fs::write(d.path().join("f"), "x").unwrap();
        apply(d.path(), &env(), true, &Act::Stage("f".into())).unwrap();
        assert_eq!(paths(&st(d.path()).staged), [("f", 'A')]);
        apply(d.path(), &env(), true, &Act::Unstage("f".into())).unwrap();
        assert!(st(d.path()).staged.is_empty());
        let r = repo();
        std::fs::remove_file(r.path().join("a.txt")).unwrap();
        apply(r.path(), &env(), true, &Act::Stage("a.txt".into())).unwrap();
        assert_eq!(paths(&st(r.path()).staged), [("a.txt", 'D')]);
    }

    #[test]
    fn hooks_do_not_run_on_commit_even_when_the_config_would_allow_them() {
        let d = repo();
        let (e, _h) = env_with_identity();
        let hooks = d.path().join(".git/hooks");
        for h in [
            "pre-commit",
            "commit-msg",
            "post-commit",
            "prepare-commit-msg",
        ] {
            let f = hooks.join(h);
            std::fs::write(
                &f,
                format!("#!/bin/sh\necho ran > \"{}/ran-{h}\"\n", d.path().display()),
            )
            .unwrap();
            #[cfg(unix)]
            std::fs::set_permissions(&f, std::os::unix::fs::PermissionsExt::from_mode(0o755))
                .unwrap();
        }
        // a hooks path in the repository's config is overridden as well
        git(d.path(), &["config", "core.hooksPath", ".git/hooks"]);
        std::fs::write(d.path().join("a.txt"), "two\n").unwrap();
        apply(d.path(), &e, true, &Act::StageAll).unwrap();
        apply(d.path(), &e, true, &Act::Commit("go".into())).unwrap();
        let ran: Vec<_> = std::fs::read_dir(d.path())
            .unwrap()
            .filter_map(|x| x.ok())
            .filter(|x| x.file_name().to_string_lossy().starts_with("ran-"))
            .collect();
        assert!(ran.is_empty(), "a hook ran: {ran:?}");
    }

    #[cfg(unix)]
    #[test]
    fn a_hostile_config_runs_nothing() {
        let d = repo();
        let (e, _h) = env_with_identity();
        let mark = |n: &str| d.path().join(n);
        let prog = |n: &str| format!("sh -c 'echo x > {}'", mark(n).display());
        let cfg = format!(
            "[core]\n\tfsmonitor = {}\n\tsshCommand = {}\n\teditor = {}\n[diff]\n\texternal = {}\n[filter \"evil\"]\n\tclean = {}\n\tsmudge = {}\n\trequired = true\n",
            prog("fsm"),
            prog("ssh"),
            prog("ed"),
            prog("ext"),
            prog("clean"),
            prog("clean"),
        );
        let mut c = std::fs::read_to_string(d.path().join(".git/config")).unwrap();
        c.push_str(&cfg);
        std::fs::write(d.path().join(".git/config"), c).unwrap();
        std::fs::write(d.path().join(".gitattributes"), "*.txt filter=evil\n").unwrap();
        std::fs::write(d.path().join("a.txt"), "two\n").unwrap();
        let r = apply(d.path(), &e, true, &Act::Stage("a.txt".into()));
        // the filter is switched off (or the whole call is refused): either way nothing ran
        let _ = r;
        let _ = status(d.path(), &e, true);
        let _ = apply(d.path(), &e, true, &Act::Commit("go".into()));
        for n in ["fsm", "ssh", "ed", "ext", "clean"] {
            assert!(!mark(n).exists(), "{n} ran");
        }
    }

    #[test]
    fn a_signing_program_named_by_the_repository_is_refused_and_nothing_runs() {
        let d = repo();
        let (e, _h) = env_with_identity();
        let m = d.path().join("gpg-ran");
        git(d.path(), &["config", "commit.gpgsign", "true"]);
        git(
            d.path(),
            &[
                "config",
                "gpg.program",
                &format!("sh -c 'echo x > {}'", m.display()),
            ],
        );
        std::fs::write(d.path().join("a.txt"), "two\n").unwrap();
        apply(d.path(), &e, true, &Act::StageAll).unwrap();
        let err = apply(d.path(), &e, true, &Act::Commit("go".into())).unwrap_err();
        assert!(err.contains("signing program"), "{err}");
        assert!(!m.exists());
    }

    #[test]
    fn a_secret_file_is_listed_by_name_and_never_staged() {
        let d = repo();
        std::fs::write(d.path().join(".env"), "A=1\n").unwrap();
        std::fs::write(d.path().join("id_rsa"), "k\n").unwrap();
        std::fs::write(d.path().join("ok.txt"), "o\n").unwrap();
        let s = st(d.path());
        assert!(s.changes.iter().filter(|e| e.secret).count() == 2);
        let err = apply(d.path(), &env(), true, &Act::Stage(".env".into())).unwrap_err();
        assert!(err.contains(".env") && !err.contains("A=1"), "{err}");
        let note = apply(d.path(), &env(), true, &Act::StageAll).unwrap();
        assert!(
            note.starts_with("2 secret-looking file(s) skipped"),
            "{note}"
        );
        let s = st(d.path());
        assert_eq!(paths(&s.staged), [("ok.txt", 'A')]);
    }

    #[test]
    fn a_secret_that_was_staged_elsewhere_blocks_the_commit() {
        let d = repo();
        let (e, _h) = env_with_identity();
        std::fs::write(d.path().join(".env"), "A=1\n").unwrap();
        git(d.path(), &["add", "-f", ".env"]);
        let err = apply(d.path(), &e, true, &Act::Commit("go".into())).unwrap_err();
        assert!(err.contains(".env"), "{err}");
        apply(d.path(), &e, true, &Act::Unstage(".env".into())).unwrap();
    }

    #[test]
    fn a_rename_with_a_short_origin_does_not_hide_the_next_entry() {
        let d = repo();
        let (e, _h) = env_with_identity();
        std::fs::write(d.path().join("a"), "x\n").unwrap();
        git(d.path(), &["add", "a"]);
        git(d.path(), &["commit", "-q", "-m", "a"]);
        git(d.path(), &["mv", "a", "a-b"]);
        std::fs::write(d.path().join("id_rsa"), "k\n").unwrap();
        git(d.path(), &["add", "-f", "id_rsa"]);
        let s = status(d.path(), &e, true).unwrap();
        assert!(s.staged.iter().any(|x| x.path == "a-b"), "{:?}", s.staged);
        assert!(
            s.staged.iter().any(|x| x.path == "id_rsa" && x.secret),
            "{:?}",
            s.staged
        );
        let err = apply(d.path(), &e, true, &Act::Commit("go".into())).unwrap_err();
        assert!(err.contains("id_rsa"), "{err}");
    }

    #[test]
    fn a_secret_beyond_the_listing_cap_still_blocks_the_commit() {
        let d = repo();
        let (e, _h) = env_with_identity();
        for i in 0..=MAX_ENTRIES {
            std::fs::write(d.path().join(format!("f{i:04}")), "x").unwrap();
        }
        std::fs::create_dir(d.path().join("server")).unwrap();
        std::fs::write(d.path().join("server/id_rsa"), "k").unwrap();
        git(d.path(), &["add", "-f", "."]);
        let s = status(d.path(), &e, true).unwrap();
        assert!(
            s.cut && !s.staged.iter().any(|x| x.secret),
            "the list hid it"
        );
        let err = apply(d.path(), &e, true, &Act::Commit("go".into())).unwrap_err();
        assert!(err.contains("server/id_rsa"), "{err}");
    }

    #[test]
    fn a_staged_path_that_cannot_be_checked_is_refused_with_a_count() {
        let d = repo();
        let (e, _h) = env_with_identity();
        std::fs::write(d.path().join("ok.txt"), "x").unwrap();
        std::fs::write(d.path().join("tab\there"), "x").unwrap();
        #[cfg(unix)]
        std::fs::write(d.path().join("back\\slash"), "x").unwrap();
        std::fs::write(d.path().join("dot."), "x").unwrap();
        git(
            d.path(),
            &["add", "-f", "--", "ok.txt", "tab\there", "dot."],
        );
        #[cfg(unix)]
        git(d.path(), &["add", "-f", "--", "back\\slash"]);
        let err = apply(d.path(), &e, true, &Act::Commit("go".into())).unwrap_err();
        assert!(err.contains("could not be checked"), "{err}");
        let head = Command::new("git")
            .args(["rev-list", "--count", "HEAD"])
            .current_dir(d.path())
            .output()
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&head.stdout).trim(),
            "1",
            "nothing was committed"
        );
    }

    #[test]
    fn deleting_a_secret_from_tracking_is_not_blocked_and_a_conflict_is_not_staged() {
        let d = repo();
        let (e, _h) = env_with_identity();
        std::fs::write(d.path().join("id_rsa"), "k").unwrap();
        git(d.path(), &["add", "-f", "id_rsa"]);
        git(d.path(), &["commit", "-q", "-m", "k"]);
        git(d.path(), &["rm", "-q", "--cached", "id_rsa"]);
        assert_eq!(
            apply(d.path(), &e, true, &Act::Commit("untrack".into())).unwrap(),
            ""
        );
        // a conflicted file: refused by Stage, skipped by Stage all
        git(d.path(), &["checkout", "-q", "-b", "other"]);
        std::fs::write(d.path().join("a.txt"), "other\n").unwrap();
        git(d.path(), &["commit", "-qam", "o"]);
        git(d.path(), &["checkout", "-q", "-"]);
        std::fs::write(d.path().join("a.txt"), "main\n").unwrap();
        git(d.path(), &["commit", "-qam", "m"]);
        let _ = Command::new("git")
            .args(["merge", "other"])
            .current_dir(d.path())
            .output();
        assert!(st(d.path()).changes.iter().any(|x| x.letter == '!'));
        let err = apply(d.path(), &e, true, &Act::Stage("a.txt".into())).unwrap_err();
        assert!(err.contains("conflict"), "{err}");
        let note = apply(d.path(), &e, true, &Act::StageAll).unwrap();
        assert!(note.contains("conflicted"), "{note}");
    }

    #[test]
    fn a_secret_that_slipped_in_after_the_gate_is_reported_not_undone() {
        let d = repo();
        std::fs::write(d.path().join("id_rsa"), "k").unwrap();
        git(d.path(), &["add", "-f", "id_rsa"]);
        git(d.path(), &["commit", "-q", "-m", "k"]);
        let (g, guards, _) = ready(d.path(), &env(), true).unwrap();
        let n = committed_secrets(&g, &guards, d.path());
        assert!(
            n.contains("id_rsa") && n.contains("git reset --soft HEAD~1"),
            "{n}"
        );
        std::fs::write(d.path().join("b"), "x").unwrap();
        git(d.path(), &["add", "b"]);
        git(d.path(), &["commit", "-q", "-m", "b"]);
        assert_eq!(committed_secrets(&g, &guards, d.path()), "");
    }

    #[test]
    fn opened_on_a_subfolder_the_whole_repository_is_listed_checked_and_staged() {
        let d = repo();
        let (e, _h) = env_with_identity();
        std::fs::create_dir(d.path().join("sub")).unwrap();
        let sub = d.path().join("sub");
        git(d.path(), &["config", "diff.relative", "true"]);
        std::fs::write(d.path().join("id_rsa"), "k").unwrap();
        git(d.path(), &["add", "-f", "id_rsa"]);
        // staged at the root, the app is on `sub` and diff.relative is on: the gate still sees it
        let err = apply(&sub, &e, true, &Act::Commit("go".into())).unwrap_err();
        assert!(err.contains("id_rsa"), "{err}");
        apply(&sub, &e, true, &Act::Unstage("id_rsa".into())).unwrap();
        // a file in the parent of the folder the app is on stages (root-relative path, literal pathspec)
        std::fs::write(d.path().join("a.txt"), "two\n").unwrap();
        apply(&sub, &e, true, &Act::Stage("a.txt".into())).unwrap();
        assert_eq!(
            paths(&status(&sub, &e, true).unwrap().staged),
            [("a.txt", 'M')]
        );
        apply(&sub, &e, true, &Act::Commit("go".into())).unwrap();
    }

    #[test]
    fn paths_outside_the_project_and_stale_paths_are_refused() {
        let d = repo();
        for bad in ["../x", "/etc/passwd", "a/../../x", ".git/config"] {
            assert!(
                apply(d.path(), &env(), true, &Act::Stage(bad.into())).is_err(),
                "{bad}"
            );
            assert!(
                apply(d.path(), &env(), true, &Act::Unstage(bad.into())).is_err(),
                "{bad}"
            );
        }
        // a file that is not changed (any more) is not acted on
        let err = apply(d.path(), &env(), true, &Act::Stage("a.txt".into())).unwrap_err();
        assert!(err.contains("changed since"), "{err}");
        assert!(apply(d.path(), &env(), true, &Act::Unstage("a.txt".into())).is_err());
        assert!(apply(d.path(), &env(), false, &Act::StageAll).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn a_link_that_leads_out_of_the_project_is_refused() {
        let d = repo();
        let out = tempfile::tempdir().unwrap();
        std::fs::write(out.path().join("t"), "x").unwrap();
        std::os::unix::fs::symlink(out.path().join("t"), d.path().join("lnk")).unwrap();
        std::os::unix::fs::symlink(out.path(), d.path().join("dir")).unwrap();
        let err = apply(d.path(), &env(), true, &Act::Stage("lnk".into())).unwrap_err();
        assert!(err.contains("leaves the project"), "{err}");
        assert!(inside(d.path(), "dir/t").is_err());
        let note = apply(d.path(), &env(), true, &Act::StageAll).unwrap();
        assert!(note.contains("leaving the project"), "{note}");
        assert!(st(d.path()).staged.is_empty());
        // a link inside the project is a file like another
        std::os::unix::fs::symlink("a.txt", d.path().join("ok")).unwrap();
        apply(d.path(), &env(), true, &Act::Stage("ok".into())).unwrap();
    }

    #[test]
    fn the_message_is_cleaned_capped_and_refused_when_it_holds_a_secret() {
        assert_eq!(clean_message(" a\u{1b}[31m\nb\t ").unwrap(), "a [31m b");
        assert!(clean_message("  \n ").is_err());
        assert!(clean_message(&"x".repeat(MAX_MESSAGE + 1)).is_err());
        assert!(clean_message(&"x".repeat(MAX_MESSAGE)).is_ok());
        let e = clean_message("fix API_KEY=abcdef123456 leak").unwrap_err();
        assert!(!e.contains("abcdef123456"));
        let d = repo();
        std::fs::write(d.path().join("a.txt"), "two\n").unwrap();
        apply(d.path(), &env(), true, &Act::StageAll).unwrap();
        assert!(
            apply(
                d.path(),
                &env(),
                true,
                &Act::Commit("TOKEN=abcdef123456".into())
            )
            .is_err()
        );
        assert_eq!(st(d.path()).staged.len(), 1, "nothing was committed");
    }

    #[test]
    fn without_an_identity_git_says_so_and_does_not_invent_one() {
        let d = repo();
        let h = tempfile::tempdir().unwrap(); // a HOME with no .gitconfig
        let e = Env::from_pairs([
            (
                "PATH",
                Env::from_process()
                    .get("PATH")
                    .unwrap_or_default()
                    .to_string(),
            ),
            ("HOME", h.path().to_string_lossy().to_string()),
        ]);
        std::fs::write(d.path().join("a.txt"), "two\n").unwrap();
        apply(d.path(), &e, true, &Act::StageAll).unwrap();
        let err = apply(d.path(), &e, true, &Act::Commit("go".into())).unwrap_err();
        assert!(
            err.to_lowercase().contains("identity")
                || err.contains("user.name")
                || err.contains("email"),
            "{err}"
        );
        assert!(err.len() <= MAX_NOTE + 3);
        assert_eq!(st(d.path()).staged.len(), 1);
    }

    #[test]
    fn git_text_is_one_capped_redacted_line() {
        let n = note_of(
            b"fatal: bad\x1b[31m thing\nAPI_KEY=abcdef123456\n",
            b"",
            Some(128),
        );
        assert!(
            !n.contains('\u{1b}') && !n.contains('\n') && !n.contains("abcdef123456"),
            "{n}"
        );
        assert!(note_of(&b"x".repeat(5000), b"", Some(1)).chars().count() <= MAX_NOTE + 1);
        assert_eq!(note_of(b"", b"", Some(3)), "git failed (exit 3)");
    }
}
