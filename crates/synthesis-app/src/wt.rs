//! `--worktree`: an isolated git worktree and branch `synthesis/<session>` for the run, left in place
//! for review.

use std::path::{Path, PathBuf};

use synthesis_config::Env;
use synthesis_core::WorkspaceContext;
use synthesis_harness::sanitize_for_display;
use synthesis_workspace::{GitInfo, SubmodulePolicy, WorktreeManager, WorktreeOptions};

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
    include_uncommitted: bool,
) -> Result<(Worktree, WorkspaceContext), String> {
    let mut opts = WorktreeOptions::new(synthesis_home.to_path_buf());
    opts.worktrees_dir = Some(synthesis_home.join("worktrees"));
    let mgr = WorktreeManager::new(env, opts).map_err(|e| format!("--worktree: {e}"))?;
    let a = mgr
        .create(source, sid, artifact_root, include_uncommitted)
        .map_err(|e| format!("--worktree: {e}"))?;
    match a.git {
        Some(info) if !a.not_a_git_repo => Ok((Worktree { mgr, info }, a.context)),
        _ => Err(format!(
            "--worktree needs a git repository; {} is not inside one",
            source.display()
        )),
    }
}

/// Re-pins the worktree a stored session ran in (`info` is what `create` recorded). Refuses when
/// the directory is gone or its git pointers no longer match the recorded ones.
pub fn reattach(
    env: &Env,
    synthesis_home: &Path,
    info: GitInfo,
    artifact_root: PathBuf,
    workspace_id: String,
    cwd: &Path,
    sid: &str,
) -> Result<(Worktree, WorkspaceContext), String> {
    // the record comes from a database: it must name one of OUR worktrees of the recorded repository
    let ours = synthesis_core::canonicalize(synthesis_home.join("worktrees")).ok();
    let wt = synthesis_core::canonicalize(&info.worktree).ok();
    let sane = wt.as_ref().zip(ours.as_ref()).is_some_and(|(w, o)| {
        w.starts_with(o)
            && w.file_name()
                .is_some_and(|n| n == std::ffi::OsStr::new(sid))
    }) && info
        .git_dir
        .starts_with(info.repo_root.join(".git").join("worktrees"));
    if !sane {
        return Err(format!(
            "session {sid}: the recorded worktree is not one of this tool's worktrees ({}); refusing to resume in it",
            sanitize_for_display(&info.worktree.display().to_string())
        ));
    }
    if !info.worktree.is_dir() {
        return Err(format!(
            "session {sid}: its worktree {} is gone, so it cannot be resumed",
            info.worktree.display()
        ));
    }
    let mut opts = WorktreeOptions::new(synthesis_home.to_path_buf());
    opts.worktrees_dir = Some(synthesis_home.join("worktrees"));
    let mgr = WorktreeManager::new(env, opts).map_err(|e| format!("worktree: {e}"))?;
    mgr.changes(&info).map_err(|e| {
        format!("session {sid}: its worktree no longer matches what was recorded: {e}")
    })?;
    let cwd = if cwd.starts_with(&info.worktree) {
        cwd
    } else {
        &info.worktree
    };
    let mut ctx =
        WorkspaceContext::open(workspace_id, &info.worktree, cwd, &[], artifact_root, sid)
            .map_err(|e| e.to_string())?;
    ctx.git_worktree = Some(ctx.root.clone());
    Ok((Worktree { mgr, info }, ctx))
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

/// `synt.worktree.submodules` (bool) from the USER file, then the MANAGED one (managed wins). A
/// repository's own settings never enable it: its `.gitmodules` URLs are untrusted.
pub fn submodules_enabled(home: &Path, managed: &Path) -> Result<bool, String> {
    let mut on = false;
    for (path, what) in [
        (home.join("settings.json"), "user settings"),
        (managed.to_path_buf(), "managed settings"),
    ] {
        let Some(doc) = synthesis_settings::read_json(&path, what)? else {
            continue;
        };
        if let Some(v) = doc
            .get("synt")
            .and_then(|s| s.get("worktree"))
            .and_then(|w| w.get("submodules"))
        {
            on = v
                .as_bool()
                .ok_or_else(|| format!("{what}: synt.worktree.submodules must be true or false"))?;
        }
    }
    Ok(on)
}

impl Worktree {
    /// The submodule step after `create`: `git worktree add` leaves them empty. With `enabled` it
    /// runs the hardened `git submodule update --init --recursive`; either way the returned notice
    /// says what happened (`None`: no `.gitmodules`).
    pub fn submodule_notice(&self, enabled: Result<bool, String>) -> Option<String> {
        let subs = match self.mgr.submodules(&self.info) {
            Ok(s) if s.is_empty() => return None,
            Ok(s) => s,
            Err(e) => return Some(format!("submodules are NOT checked out ({e})")),
        };
        let names = subs
            .iter()
            .map(|(n, _)| sanitize_for_display(n))
            .collect::<Vec<_>>()
            .join(", ");
        match enabled {
            Ok(true) => match self
                .mgr
                .init_submodules(&self.info, SubmodulePolicy::default())
            {
                Ok(n) => Some(format!("checked out {} submodule(s): {names}", n.len())),
                Err(e) => Some(format!(
                    "submodules are NOT checked out ({}): {names}",
                    sanitize_for_display(&e.to_string())
                )),
            },
            Ok(false) => Some(format!(
                "submodules are NOT checked out in the worktree: {names}. To check them out (https and ssh URLs only) set {{\"synt\":{{\"worktree\":{{\"submodules\":true}}}}}} in your user settings"
            )),
            Err(e) => Some(format!(
                "submodules are NOT checked out ({}): {names}",
                sanitize_for_display(&e)
            )),
        }
    }

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
    fn submodules_setting_is_user_or_managed_and_defaults_off() {
        let d = tempfile::tempdir().unwrap();
        let (home, managed) = (d.path(), d.path().join("m.json"));
        let on = || super::submodules_enabled(home, &managed);
        assert_eq!(on(), Ok(false));
        std::fs::write(
            home.join("settings.json"),
            r#"{"synt":{"worktree":{"submodules":true}}}"#,
        )
        .unwrap();
        assert_eq!(on(), Ok(true));
        std::fs::write(&managed, r#"{"synt":{"worktree":{"submodules":false}}}"#).unwrap();
        assert_eq!(on(), Ok(false), "managed wins");
        std::fs::write(&managed, r#"{"synt":{"worktree":{"submodules":"yes"}}}"#).unwrap();
        assert!(on().is_err());
    }

    #[test]
    fn counts_files_and_lines_not_headers() {
        let d = "diff --git a/x b/x\n--- a/x\n+++ b/x\n@@ -1 +1,2 @@\n-old\n+new\n+more\n";
        assert_eq!(super::stat(d), (1, 2, 1));
    }
}
