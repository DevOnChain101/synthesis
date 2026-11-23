//! The branch of a folder's git work tree, read from `.git/HEAD` directly: no `git` process runs, so a repository's
//! config (fsmonitor, hooks) is never executed. Callers run it off the UI thread.

use std::path::{Path, PathBuf};

const MAX_HEAD: u64 = 4096;
const MAX_NAME: usize = 40;

/// `Some(branch)` (`@a1b2c3d` for a detached HEAD) when `dir` is inside a git work tree, else `None`.
pub fn branch_of(dir: &Path) -> Option<String> {
    let head = dir.ancestors().find_map(|d| git_dir(&d.join(".git")))?;
    let head = read_small(&head.join("HEAD"))?;
    let head = head.trim();
    let name = match head.strip_prefix("ref:") {
        Some(r) => {
            let r = r.trim();
            r.strip_prefix("refs/heads/").unwrap_or(r).to_string()
        }
        None if head.len() >= 7 && head.bytes().all(|b| b.is_ascii_hexdigit()) => {
            format!("@{}", &head[..7])
        }
        None => return None,
    };
    let name = crate::term::sanitize_title(&name);
    if name.is_empty() {
        return None;
    }
    Some(match name.char_indices().nth(MAX_NAME) {
        Some((i, _)) => format!("{}\u{2026}", &name[..i]),
        None => name,
    })
}

/// The git directory a `.git` entry stands for: the directory itself, or the one a `gitdir: path` file names
/// (worktrees, submodules).
fn git_dir(dot_git: &Path) -> Option<PathBuf> {
    let meta = std::fs::symlink_metadata(dot_git).ok()?;
    if meta.is_dir() {
        return Some(dot_git.to_path_buf());
    }
    if !meta.is_file() {
        return None;
    }
    let text = read_small(dot_git)?;
    let target = Path::new(text.lines().next()?.strip_prefix("gitdir:")?.trim());
    let dir = dot_git.parent()?.join(target);
    dir.join("HEAD").is_file().then_some(dir)
}

fn read_small(p: &Path) -> Option<String> {
    use std::io::Read;
    let f = std::fs::File::open(p).ok()?;
    if !f.metadata().ok()?.is_file() {
        return None;
    }
    let mut s = String::new();
    f.take(MAX_HEAD).read_to_string(&mut s).ok()?;
    Some(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repo(head: &str) -> tempfile::TempDir {
        let t = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(t.path().join(".git")).unwrap();
        std::fs::write(t.path().join(".git/HEAD"), head).unwrap();
        t
    }

    #[test]
    fn branch_detached_nested_and_none() {
        let t = repo("ref: refs/heads/feature/x\n");
        std::fs::create_dir_all(t.path().join("a/b")).unwrap();
        assert_eq!(
            branch_of(&t.path().join("a/b")).as_deref(),
            Some("feature/x")
        );
        let d = repo("0123456789abcdef0123456789abcdef01234567\n");
        assert_eq!(branch_of(d.path()).as_deref(), Some("@0123456"));
        let plain = tempfile::tempdir().unwrap();
        assert_eq!(branch_of(plain.path()), None);
    }

    #[test]
    fn worktree_gitdir_file_is_followed() {
        let t = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(t.path().join("real")).unwrap();
        std::fs::write(t.path().join("real/HEAD"), "ref: refs/heads/wt\n").unwrap();
        std::fs::create_dir_all(t.path().join("w")).unwrap();
        std::fs::write(t.path().join("w/.git"), "gitdir: ../real\n").unwrap();
        assert_eq!(branch_of(&t.path().join("w")).as_deref(), Some("wt"));
    }

    #[test]
    fn untrusted_names_are_cleaned_and_capped() {
        let t = repo("ref: refs/heads/a\u{202e}b\u{1b}[31m\u{200b}c\n");
        assert_eq!(branch_of(t.path()).as_deref(), Some("ab[31mc"));
        let long = repo(&format!("ref: refs/heads/{}\n", "x".repeat(100)));
        let b = branch_of(long.path()).unwrap();
        assert_eq!(b.chars().count(), MAX_NAME + 1);
        assert!(b.ends_with('\u{2026}'));
    }
}
