//! One file's diff against HEAD for the file explorer, and the checks that go with it. Same hardened [`Git`] as
//! the rest of `project` (no fsmonitor, hooks, pager, external diff, textconv or filter drivers; closed
//! environment; bounded output; timeout with a process-group kill). The repository is untrusted data:
//! the path is checked before git sees it, a secret file and a diff that holds secret-shaped text are withheld,
//! and a `.git` that leads out of the folder is not a repository to run git in.

use std::io::Read as _;
use std::path::Path;

use super::{Git, MAX_BYTES, clean_line};
use synthesis_config::Env;

/// Lines of one diff the explorer keeps.
pub const DIFF_LINES: usize = 2000;
const HIDDEN_SECRET: &str = "hidden: secret file";
const HIDDEN_SHAPED: &str = "hidden: the diff holds what looks like a secret";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileDiff {
    pub lines: Vec<String>,
    /// Only the start is shown (the byte or line cap).
    pub cut: bool,
}

/// A project-relative path git output or a click may name: no absolute path or drive, no `..`/`.`/`.git`
/// component, no NUL, backslash or control character.
pub fn safe_rel(rel: &str) -> Result<(), String> {
    let bad = rel.is_empty()
        || rel.starts_with(['/', '\\'])
        || rel.as_bytes().get(1) == Some(&b':')
        || rel.chars().any(|c| c.is_control() || c == '\\')
        || rel
            .split('/')
            .any(|c| matches!(c, "" | "." | ".." | ".git"));
    if bad {
        return Err("not a path inside the project".into());
    }
    Ok(())
}

/// The folder's `.git` must be a directory, or a `gitdir:` file naming a directory inside the folder or a linked
/// work tree's own (whose `gitdir` file points back at this `.git` file, as `git worktree add` writes it). A
/// link, or a file leading elsewhere, is refused: the git directory would be somebody else's.
pub fn gitdir_ok(cwd: &Path) -> Result<(), String> {
    let no = |why: &str| Err(format!("git was not run: {why}"));
    let Some(base) = cwd
        .ancestors()
        .find(|d| d.join(".git").symlink_metadata().is_ok())
    else {
        return Ok(()); // not a repository: git will say so
    };
    let dot = base.join(".git");
    let meta = dot.symlink_metadata().map_err(|e| e.to_string())?;
    if meta.is_dir() {
        return Ok(());
    }
    if !meta.is_file() {
        return no(".git is a link");
    }
    let mut text = String::new();
    std::fs::File::open(&dot)
        .and_then(|f| f.take(4096).read_to_string(&mut text))
        .map_err(|e| e.to_string())?;
    let Some(target) = text.lines().next().and_then(|l| l.strip_prefix("gitdir:")) else {
        return no(".git is not a gitdir file");
    };
    let (Ok(target), Ok(root), Ok(dot)) = (
        std::fs::canonicalize(base.join(target.trim())),
        std::fs::canonicalize(base),
        std::fs::canonicalize(&dot),
    ) else {
        return no(".git names a folder that is not there");
    };
    let back = std::fs::read_to_string(target.join("gitdir"))
        .ok()
        .and_then(|b| std::fs::canonicalize(b.lines().next()?.trim()).ok());
    if target.starts_with(&root) || back.as_deref() == Some(dot.as_path()) {
        Ok(())
    } else {
        no(".git points outside this folder")
    }
}

/// `lines` unless one of them looks like a secret (the shapes `toolview::redact_text` masks): then nothing.
fn unless_secret(lines: Vec<String>) -> Result<Vec<String>, String> {
    if lines.iter().any(|l| crate::toolview::redact_text(l) != *l) {
        return Err(HIDDEN_SHAPED.into());
    }
    Ok(lines)
}

/// A new file shown as one added hunk (`lines` are the file's text, already read safely).
pub fn all_added(rel: &str, lines: &[String]) -> Result<FileDiff, String> {
    safe_rel(rel)?;
    if synthesis_core::secrets::is_secret_shaped(Path::new(rel)) {
        return Err(HIDDEN_SECRET.into());
    }
    let n = lines.len().min(DIFF_LINES);
    let mut v = vec![
        "--- /dev/null".to_string(),
        format!("+++ b/{}", clean_line(rel)),
        format!("@@ -0,0 +1,{n} @@"),
    ];
    v.extend(lines.iter().take(n).map(|l| format!("+{l}")));
    Ok(FileDiff {
        lines: unless_secret(v)?,
        cut: lines.len() > n,
    })
}

/// `git diff HEAD -- rel` (the index when there is no commit yet) of a tracked file. Never the content of a
/// secret file, and never a diff with secret-shaped text in it.
pub fn file_diff(cwd: &Path, env: &Env, trusted: bool, rel: &str) -> Result<FileDiff, String> {
    safe_rel(rel)?;
    if synthesis_core::secrets::is_secret_shaped(Path::new(rel)) {
        return Err(HIDDEN_SECRET.into());
    }
    if !trusted {
        return Err("the folder is not trusted: git is not run here".into());
    }
    gitdir_ok(cwd)?;
    let git = Git::new(env);
    if git
        .run(cwd, &["rev-parse", "--is-inside-work-tree"], 0)
        .is_none()
    {
        return Err("not a git repository".into());
    }
    let guards = git
        .filter_guards(cwd)
        .map_err(|why| format!("{why}; git was not run"))?;
    let diff = |base: &'static str| {
        let mut a = vec!["--literal-pathspecs", "diff"];
        a.extend([
            "--no-color",
            "--no-ext-diff",
            "--no-textconv",
            "--ignore-submodules=all",
            "--no-renames",
        ]);
        a.extend(base.split_whitespace());
        a.extend(["--", rel]);
        git.run_with(cwd, &guards, &a, b'\n')
    };
    let (raw, cut) = diff("HEAD")
        .or_else(|| diff("--cached"))
        .ok_or("git could not produce the diff")?;
    let mut lines: Vec<String> = String::from_utf8_lossy(&raw)
        .lines()
        .map(clean_line)
        .collect();
    let cut = cut || lines.len() > DIFF_LINES || raw.len() >= MAX_BYTES;
    lines.truncate(DIFF_LINES);
    Ok(FileDiff {
        lines: unless_secret(lines)?,
        cut,
    })
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

    fn repo() -> tempfile::TempDir {
        let d = tempfile::tempdir().unwrap();
        git(d.path(), &["init", "-q"]);
        std::fs::write(d.path().join("a.txt"), "one\ntwo\n").unwrap();
        std::fs::write(d.path().join(".env"), "A=1\n").unwrap();
        git(d.path(), &["add", "-f", "."]);
        git(d.path(), &["commit", "-q", "-m", "x"]);
        d
    }

    #[test]
    fn paths_that_could_leave_the_project_are_refused() {
        for bad in [
            "",
            "/etc/passwd",
            "\\x",
            "C:/x",
            "a/../b",
            "../a",
            "./a",
            "a//b",
            ".git/config",
            "a/.git/x",
            "a\0b",
            "a\nb",
            "a\u{1b}b",
            "a\\b",
        ] {
            assert!(safe_rel(bad).is_err(), "{bad:?}");
        }
        assert!(safe_rel("src/a b.rs").is_ok());
    }

    #[test]
    fn a_changed_file_has_its_diff_against_head() {
        let d = repo();
        std::fs::write(d.path().join("a.txt"), "one\nthree\n").unwrap();
        let f = file_diff(d.path(), &env(), true, "a.txt").unwrap();
        assert!(f.lines.iter().any(|l| l == "-two"), "{:?}", f.lines);
        assert!(f.lines.iter().any(|l| l == "+three"));
        assert!(!f.cut);
        assert!(file_diff(d.path(), &env(), false, "a.txt").is_err());
        assert!(file_diff(d.path(), &env(), true, "../a.txt").is_err());
    }

    #[test]
    fn a_secret_file_and_a_secret_shaped_diff_are_withheld() {
        let d = repo();
        std::fs::write(d.path().join(".env"), "A=2\n").unwrap();
        assert_eq!(
            file_diff(d.path(), &env(), true, ".env").unwrap_err(),
            HIDDEN_SECRET
        );
        assert_eq!(
            all_added(".env", &["A=2".into()]).unwrap_err(),
            HIDDEN_SECRET
        );
        std::fs::write(d.path().join("a.txt"), "API_KEY=abcdef123456\n").unwrap();
        assert_eq!(
            file_diff(d.path(), &env(), true, "a.txt").unwrap_err(),
            HIDDEN_SHAPED
        );
        assert!(
            all_added(
                "n.txt",
                &["Authorization: Bearer abcdefghijklmnop1234".into()]
            )
            .is_err()
        );
        let ok = all_added("n.txt", &["hello".into()]).unwrap();
        assert_eq!(ok.lines.last().unwrap(), "+hello");
    }

    #[test]
    fn the_diff_is_capped() {
        let d = repo();
        let big: String = (0..DIFF_LINES + 50)
            .map(|i| format!("line {i}\n"))
            .collect();
        std::fs::write(d.path().join("a.txt"), big).unwrap();
        let f = file_diff(d.path(), &env(), true, "a.txt").unwrap();
        assert!(f.cut);
        assert_eq!(f.lines.len(), DIFF_LINES);
        let added = all_added("n.txt", &vec!["x".to_string(); DIFF_LINES + 5]).unwrap();
        assert!(added.cut && added.lines.len() == DIFF_LINES + 3);
    }

    #[test]
    fn a_git_file_pointing_outside_is_refused_and_a_linked_worktree_is_not() {
        let t = tempfile::tempdir().unwrap();
        let out = t.path().join("elsewhere");
        std::fs::create_dir_all(&out).unwrap();
        let w = t.path().join("w");
        std::fs::create_dir_all(&w).unwrap();
        std::fs::write(w.join(".git"), format!("gitdir: {}\n", out.display())).unwrap();
        assert!(gitdir_ok(&w).unwrap_err().contains("outside"));
        // the work tree's own git directory carries a back pointer to this `.git` file
        std::fs::write(
            out.join("gitdir"),
            format!(
                "{}\n",
                std::fs::canonicalize(w.join(".git")).unwrap().display()
            ),
        )
        .unwrap();
        assert!(gitdir_ok(&w).is_ok());
        std::fs::write(w.join(".git"), "garbage").unwrap();
        assert!(gitdir_ok(&w).is_err());
        assert!(file_diff(&w, &env(), true, "a.txt").is_err());
    }

    /// A repository whose config and attributes name programs: none runs while listing or diffing.
    #[cfg(unix)]
    #[test]
    fn a_malicious_repository_runs_nothing() {
        use std::os::unix::fs::PermissionsExt as _;
        let d = repo();
        let w = d.path();
        let marker = w.join("PWNED");
        let script = w.join("evil.sh");
        std::fs::write(
            &script,
            format!("#!/bin/sh\ntouch '{}'\ncat\n", marker.display()),
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        let s = script.display().to_string();
        let hooks = w.join("hooks");
        std::fs::create_dir_all(&hooks).unwrap();
        std::fs::copy(&script, hooks.join("post-index-change")).unwrap();
        std::fs::set_permissions(
            hooks.join("post-index-change"),
            std::fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        for (k, v) in [
            ("core.fsmonitor", s.clone()),
            ("core.pager", s.clone()),
            ("core.hooksPath", hooks.display().to_string()),
            ("diff.external", s.clone()),
            ("diff.evil.textconv", s.clone()),
            ("diff.evil.command", s.clone()),
            ("filter.evil.clean", s.clone()),
            ("filter.evil.smudge", s.clone()),
            ("alias.diff", format!("!{s}")),
        ] {
            git(w, &["config", k, &v]);
        }
        std::fs::write(w.join(".gitattributes"), "*.txt diff=evil filter=evil\n").unwrap();
        std::fs::write(w.join("a.txt"), "changed\n").unwrap();
        std::fs::write(w.join("new.txt"), "new\n").unwrap();
        let listing = crate::project::scan_lazy(w, &env(), true, None);
        let diff = file_diff(w, &env(), true, "a.txt");
        assert!(!marker.exists(), "a program of the repository ran");
        assert!(listing.listing.git, "{:?}", listing.note);
        assert!(diff.is_ok(), "{diff:?}");
    }
}
