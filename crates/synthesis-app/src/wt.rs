//! `--worktree`: an isolated git worktree and branch `synthesis/<session>` for the run, left in place
//! for review.

use std::path::{Path, PathBuf};

use synthesis_config::Env;
use synthesis_core::WorkspaceContext;
use synthesis_workspace::{GitInfo, WorktreeManager, WorktreeOptions};

pub struct Worktree {
    mgr: WorktreeManager,
    pub info: GitInfo,
}

/// Creates the worktree under `<synthesis_home>/worktrees`. A non-git directory or a repository
/// without commits is an error (the caller exits 2).
pub fn create(
    env: &Env,
    synthesis_home: &Path,
    source: &Path,
    sid: &str,
    artifact_root: PathBuf,
) -> Result<(Worktree, WorkspaceContext), String> {
    let mut opts = WorktreeOptions::new(synthesis_home.to_path_buf());
    opts.worktrees_dir = Some(synthesis_home.join("worktrees"));
    let mgr = WorktreeManager::new(env, opts).map_err(|e| format!("--worktree: {e}"))?;
    let a = mgr
        .create(source, sid, artifact_root, false)
        .map_err(|e| format!("--worktree: {e}"))?;
    match a.git {
        Some(info) if !a.not_a_git_repo => Ok((Worktree { mgr, info }, a.context)),
        _ => Err(format!(
            "--worktree needs a git repository; {} is not inside one",
            source.display()
        )),
    }
}

/// `(files, added, removed)` of a unified diff.
pub fn stat(diff: &str) -> (usize, usize, usize) {
    let (mut f, mut a, mut r) = (0, 0, 0);
    for l in diff.lines() {
        if l.starts_with("diff --git ") {
            f += 1;
        } else if l.starts_with('+') && !l.starts_with("+++") {
            a += 1;
        } else if l.starts_with('-') && !l.starts_with("---") {
            r += 1;
        }
    }
    (f, a, r)
}

impl Worktree {
    /// One-line report for the `task.ended` notice; removes the worktree when asked and clean.
    pub fn finish(&self, cleanup: bool) -> String {
        let i = &self.info;
        let head = format!("worktree {} on branch {}", i.worktree.display(), i.branch);
        let (changes, diff) = match (self.mgr.changes(i), self.mgr.diff(i)) {
            (Ok(c), Ok(d)) => (c, d),
            (Err(e), _) | (_, Err(e)) => return format!("{head}; could not read its changes: {e}"),
        };
        let (f, a, r) = stat(&diff);
        let mut s = format!("{head}: {f} file(s) changed, +{a} -{r}");
        if !changes.outside.is_empty() {
            s += &format!(
                "; WARNING {} file(s) of the original tree changed during the run",
                changes.outside.len()
            );
        }
        if cleanup {
            if !changes.agent.is_empty() {
                s += "; kept for review (--cleanup-worktree only removes a clean worktree)";
            } else {
                match self.mgr.cleanup(i, false) {
                    Ok(()) => s += "; clean, removed",
                    Err(e) => s += &format!("; not removed: {e}"),
                }
            }
        } else {
            s += "; left in place for review";
        }
        s
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn counts_files_and_lines_not_headers() {
        let d = "diff --git a/x b/x\n--- a/x\n+++ b/x\n@@ -1 +1,2 @@\n-old\n+new\n+more\n";
        assert_eq!(super::stat(d), (1, 2, 1));
    }
}
