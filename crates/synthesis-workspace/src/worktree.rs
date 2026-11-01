//! `WorktreeManager`: one git worktree + branch per session, never touching the user's tree.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use synthesis_config::Env;
use synthesis_core::WorkspaceContext;

use crate::git::Git;
use crate::restore::hash_bytes;
use crate::{Error, Result};

pub struct WorktreeOptions {
    /// Injected state dir (e.g. `$SYNTHESIS_HOME`); hosts the empty hooks dir.
    pub state_dir: PathBuf,
    /// Where worktrees live; default `<repo>/../.synthesis-worktrees`.
    pub worktrees_dir: Option<PathBuf>,
    /// Read the user's global git config (off by default; it can carry filters and aliases).
    pub allow_global_config: bool,
    /// Author / committer for commits we create.
    pub identity: (String, String),
}

impl WorktreeOptions {
    pub fn new(state_dir: PathBuf) -> Self {
        Self {
            state_dir,
            worktrees_dir: None,
            allow_global_config: false,
            identity: ("Synthesis Agent".into(), "synthesis@localhost".into()),
        }
    }
}

/// Persist this next to the session; every post-create call takes it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct GitInfo {
    pub repo_root: PathBuf,
    pub worktree: PathBuf,
    /// Absolute `<repo>/.git/worktrees/<id>`, recorded at creation (before the agent runs).
    /// Every later git call in the worktree is pinned to it; the agent-writable `.git` pointer
    /// file is only compared against it, never trusted.
    pub git_dir: PathBuf,
    pub branch: String,
    /// Commit the agent's work is measured against.
    pub baseline: String,
    /// HEAD of the original tree at allocation time.
    pub main_head: String,
    /// Original-tree files that were already dirty at allocation: path -> content hash.
    pub main_snapshot: BTreeMap<String, Option<String>>,
    pub include_uncommitted: bool,
}

#[derive(Debug, Clone)]
pub struct Allocation {
    pub context: WorkspaceContext,
    /// `None` for a plain directory.
    pub git: Option<GitInfo>,
    /// Capability flag: no worktree, no diff / merge, no isolation.
    pub not_a_git_repo: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Change {
    pub path: String,
    /// git status letter (`M`, `A`, `D`, ...) or `?` for untracked.
    pub status: char,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Changes {
    /// Worktree vs baseline: the agent's edits.
    pub agent: Vec<Change>,
    /// Original tree files that changed since allocation (user edits, or the agent escaping).
    pub outside: Vec<Change>,
    /// Original HEAD moved since allocation: `(old, new)`.
    pub main_head_moved: Option<(String, String)>,
    /// Paths in both lists.
    pub overlap: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MergePlan {
    pub branch: String,
    pub target_head: String,
    pub tip: String,
    /// `baseline..tip`, oldest first (for an explicit cherry-pick).
    pub commits: Vec<String>,
    pub conflicts: Vec<String>,
    pub uncommitted_in_worktree: bool,
    /// Agent-changed paths that are also dirty in the original tree.
    pub overlap: Vec<String>,
    pub clean: bool,
}

pub struct WorktreeManager {
    git: Git,
    opts: WorktreeOptions,
}

fn check_id(id: &str) -> Result<()> {
    let ok = !id.is_empty()
        && id.len() <= 100
        && !id.starts_with(['-', '.'])
        && !id.contains("..")
        && !id.ends_with(".lock")
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'));
    if ok {
        Ok(())
    } else {
        Err(Error::Invalid(format!("bad session id {id:?}")))
    }
}

fn hash_path(p: &Path) -> Option<String> {
    let m = fs::symlink_metadata(p).ok()?;
    if m.is_symlink() {
        Some(hash_bytes(
            fs::read_link(p).ok()?.to_string_lossy().as_bytes(),
        ))
    } else if m.is_file() {
        Some(hash_bytes(&fs::read(p).ok()?))
    } else {
        None
    }
}

fn nul_split(b: &[u8]) -> impl Iterator<Item = String> + '_ {
    b.split(|c| *c == 0)
        .filter(|s| !s.is_empty())
        .map(|s| String::from_utf8_lossy(s).into_owned())
}

impl WorktreeManager {
    pub fn new(env: &Env, opts: WorktreeOptions) -> Result<Self> {
        Ok(Self {
            git: Git::new(env, &opts.state_dir, opts.allow_global_config)?,
            opts,
        })
    }

    fn ident(&self) -> [String; 4] {
        let (n, e) = &self.opts.identity;
        [
            "-c".into(),
            format!("user.name={n}"),
            "-c".into(),
            format!("user.email={e}"),
        ]
    }

    /// Runs git with the commit identity prepended.
    fn id_out(&self, g: &Git, dir: &Path, args: &[&str]) -> Result<Vec<u8>> {
        let id = self.ident();
        let all: Vec<&str> = id
            .iter()
            .map(String::as_str)
            .chain(args.iter().copied())
            .collect();
        g.out(dir, &all)
    }

    /// Git bound to the recorded worktree repo. Refuses when the worktree's `.git` pointer or the
    /// admin dir's `gitdir` / `commondir` files no longer match the recorded values. Those are
    /// read directly, so no git process ever follows an agent-writable pointer; the probe is
    /// pinned too, and pinning covers the check-to-use gap.
    fn pinned(&self, info: &GitInfo) -> Result<Git> {
        let refuse = |what: &str| {
            Err(Error::Refused(format!(
                "{} no longer matches the recorded {}; refusing to trust it",
                what,
                info.git_dir.display()
            )))
        };
        let canon = |p: &Path| synthesis_core::canonicalize(p).ok();
        let want_dir = canon(&info.git_dir);
        let dot_git = info.worktree.join(".git");
        let pointer = match fs::symlink_metadata(&dot_git) {
            Ok(m) if m.is_file() => fs::read_to_string(&dot_git).ok(),
            _ => None,
        };
        let target = pointer
            .as_deref()
            .and_then(|t| t.lines().next()?.strip_prefix("gitdir:"))
            .map(|t| info.worktree.join(t.trim())); // join keeps an absolute target as is
        if want_dir.is_none() || target.as_deref().and_then(canon) != want_dir {
            return refuse(&format!("{}", dot_git.display()));
        }
        let read = |name: &str| fs::read_to_string(info.git_dir.join(name)).ok();
        let back = read("gitdir").map(|t| PathBuf::from(t.trim()));
        if back.as_deref().and_then(canon) != canon(&dot_git) {
            return refuse(&format!("{}/gitdir", info.git_dir.display()));
        }
        let common = read("commondir").map(|t| info.git_dir.join(t.trim()));
        let recorded_common = info.git_dir.parent().and_then(Path::parent).and_then(canon);
        if recorded_common.is_none() || common.as_deref().and_then(canon) != recorded_common {
            return refuse(&format!("{}/commondir", info.git_dir.display()));
        }
        let pinned = self.git.pinned(&info.git_dir, &info.worktree);
        let o = pinned.probe(&info.worktree, &["rev-parse", "--absolute-git-dir"])?;
        let found = o
            .status
            .success()
            .then(|| String::from_utf8_lossy(&o.stdout).trim_end().to_string())
            .and_then(|s| synthesis_core::canonicalize(s).ok());
        if found.is_none() || found != want_dir {
            return refuse(&format!("{}/.git", info.worktree.display()));
        }
        Ok(pinned)
    }

    /// Allocates a worktree for `source` (any dir inside a git repo) or, for a non-git dir, a
    /// plain workspace. The original tree is only ever read. With `include_uncommitted`,
    /// tracked changes and untracked files of the original tree are copied into the worktree
    /// and committed as the baseline, so they are not reported as agent edits.
    pub fn create(
        &self,
        source: &Path,
        session_id: &str,
        artifact_root: PathBuf,
        include_uncommitted: bool,
    ) -> Result<Allocation> {
        check_id(session_id)?;
        let source = synthesis_core::canonicalize(source)?;
        let ws_id = uuid::Uuid::new_v4().to_string();
        let probe = self.git.probe(&source, &["rev-parse", "--show-toplevel"])?;
        if !probe.status.success() {
            if !String::from_utf8_lossy(&probe.stderr).contains("not a git repository") {
                self.git.check(&["rev-parse"], probe)?;
                unreachable!();
            }
            let context =
                WorkspaceContext::open(ws_id, &source, &source, &[], artifact_root, session_id)?;
            return Ok(Allocation {
                context,
                git: None,
                not_a_git_repo: true,
            });
        }
        let top = String::from_utf8_lossy(&probe.stdout)
            .trim_end()
            .to_string();
        let repo_root = synthesis_core::canonicalize(top)?;
        let g = &self.git;
        let main_head = g
            .out_str(&repo_root, &["rev-parse", "--verify", "HEAD^{commit}"])
            .map_err(|_| Error::Refused("repository has no commits".into()))?;
        let main_snapshot = self.dirty(g, &repo_root, &main_head)?;

        let dir = match &self.opts.worktrees_dir {
            Some(d) => d.clone(),
            None => repo_root
                .parent()
                .ok_or_else(|| Error::Invalid("repo at filesystem root".into()))?
                .join(".synthesis-worktrees"),
        };
        fs::create_dir_all(&dir)?;
        let wt = synthesis_core::canonicalize(&dir)?.join(session_id);
        if wt.exists() {
            return Err(Error::Refused(format!("{} already exists", wt.display())));
        }
        let branch = format!("synthesis/{session_id}");
        let wt_s = wt.to_string_lossy().into_owned();
        g.out(
            &repo_root,
            &[
                "worktree", "add", "--quiet", "-b", &branch, &wt_s, &main_head,
            ],
        )?;
        let wt = synthesis_core::canonicalize(&wt)?;
        let git_dir =
            synthesis_core::canonicalize(g.out_str(&wt, &["rev-parse", "--absolute-git-dir"])?)?;

        let mut baseline = main_head.clone();
        if include_uncommitted && !main_snapshot.is_empty() {
            self.copy_uncommitted(&repo_root, &wt, &main_head)?;
            g.out(&wt, &["add", "-A"])?;
            self.id_out(
                g,
                &wt,
                &[
                    "commit",
                    "--quiet",
                    "--no-verify",
                    "--no-gpg-sign",
                    "-m",
                    "synthesis: baseline with uncommitted changes",
                ],
            )?;
            baseline = g.out_str(&wt, &["rev-parse", "HEAD"])?;
        }
        // a source inside a subdirectory of the repo maps to the same subdirectory of the worktree
        let cwd = source
            .strip_prefix(&repo_root)
            .map(|rel| wt.join(rel))
            .ok()
            .filter(|d| d.is_dir())
            .unwrap_or_else(|| wt.clone());
        let mut context = WorkspaceContext::open(ws_id, &wt, &cwd, &[], artifact_root, session_id)?;
        context.git_worktree = Some(context.root.clone());
        let info = GitInfo {
            repo_root,
            worktree: context.root.clone(),
            git_dir,
            branch,
            baseline,
            main_head,
            main_snapshot,
            include_uncommitted,
        };
        Ok(Allocation {
            context,
            git: Some(info),
            not_a_git_repo: false,
        })
    }

    /// Tracked changes via a binary diff, untracked files by copy (regular files only).
    fn copy_uncommitted(&self, repo: &Path, wt: &Path, head: &str) -> Result<()> {
        let diff = self.git.out(
            repo,
            &[
                "diff",
                "--binary",
                "--no-ext-diff",
                "--no-textconv",
                "--no-color",
                head,
            ],
        )?;
        if !diff.is_empty() {
            let o = self.git.run(
                wt,
                &["apply", "--binary", "--whitespace=nowarn"],
                Some(&diff),
            )?;
            self.git.check(&["apply"], o)?;
        }
        for rel in nul_split(
            &self
                .git
                .out(repo, &["ls-files", "--others", "--exclude-standard", "-z"])?,
        ) {
            let from = repo.join(&rel);
            if fs::symlink_metadata(&from)?.is_file() {
                let to = wt.join(&rel);
                if let Some(p) = to.parent() {
                    fs::create_dir_all(p)?;
                }
                fs::copy(&from, &to)?;
            }
        }
        Ok(())
    }

    /// Dirty files of `root` vs `base` (tracked diff + untracked) with content hashes.
    fn dirty(&self, g: &Git, root: &Path, base: &str) -> Result<BTreeMap<String, Option<String>>> {
        Ok(self
            .changes_vs(g, root, base)?
            .into_iter()
            .map(|c| {
                let h = hash_path(&root.join(&c.path));
                (c.path, h)
            })
            .collect())
    }

    fn changes_vs(&self, g: &Git, dir: &Path, base: &str) -> Result<Vec<Change>> {
        let tracked = g.out(
            dir,
            &[
                "diff",
                "--name-status",
                "-z",
                "--no-renames",
                "--no-ext-diff",
                "--no-textconv",
                base,
            ],
        )?;
        let mut parts = nul_split(&tracked);
        let mut v = Vec::new();
        while let (Some(s), Some(path)) = (parts.next(), parts.next()) {
            v.push(Change {
                path,
                status: s.chars().next().unwrap_or('M'),
            });
        }
        for path in nul_split(&g.out(dir, &["ls-files", "--others", "--exclude-standard", "-z"])?) {
            v.push(Change { path, status: '?' });
        }
        Ok(v)
    }

    /// Agent edits vs user / outside edits (spec: separate them; never reset either).
    pub fn changes(&self, info: &GitInfo) -> Result<Changes> {
        let wg = self.pinned(info)?;
        let agent = self.changes_vs(&wg, &info.worktree, &info.baseline)?;
        let now = self.dirty(&self.git, &info.repo_root, &info.main_head)?;
        let mut outside = Vec::new();
        for path in now
            .keys()
            .chain(info.main_snapshot.keys())
            .collect::<std::collections::BTreeSet<_>>()
        {
            if now.get(path) != info.main_snapshot.get(path) {
                let status = if now.contains_key(path) { 'M' } else { 'R' };
                outside.push(Change {
                    path: path.clone(),
                    status,
                });
            }
        }
        let head = self.git.out_str(&info.repo_root, &["rev-parse", "HEAD"])?;
        let overlap = agent
            .iter()
            .filter(|c| outside.iter().any(|o| o.path == c.path))
            .map(|c| c.path.clone())
            .collect();
        Ok(Changes {
            agent,
            outside,
            main_head_moved: (head != info.main_head).then(|| (info.main_head.clone(), head)),
            overlap,
        })
    }

    /// Unified diff of the agent's work vs the baseline (untracked files included via
    /// intent-to-add in the agent's own worktree).
    pub fn diff(&self, info: &GitInfo) -> Result<String> {
        let g = self.pinned(info)?;
        g.out(&info.worktree, &["add", "--intent-to-add", "--all"])?;
        let d = g.out(
            &info.worktree,
            &[
                "diff",
                "--binary",
                "--no-ext-diff",
                "--no-textconv",
                "--no-color",
                &info.baseline,
            ],
        )?;
        Ok(String::from_utf8_lossy(&d).into_owned())
    }

    /// Commits everything in the worktree; returns the new tip, or `None` if nothing changed.
    pub fn commit_all(&self, info: &GitInfo, message: &str) -> Result<Option<String>> {
        let g = self.pinned(info)?;
        g.out(&info.worktree, &["add", "-A"])?;
        if g.run(&info.worktree, &["diff", "--cached", "--quiet"], None)?
            .status
            .success()
        {
            return Ok(None);
        }
        self.id_out(
            &g,
            &info.worktree,
            &[
                "commit",
                "--quiet",
                "--no-verify",
                "--no-gpg-sign",
                "-m",
                message,
            ],
        )?;
        Ok(Some(g.out_str(&info.worktree, &["rev-parse", "HEAD"])?))
    }

    /// Read-only: computes whether the session branch merges into the original HEAD cleanly
    /// (in-memory `merge-tree`; nothing is checked out or written to a working tree).
    pub fn plan_merge(&self, info: &GitInfo) -> Result<MergePlan> {
        let g = &self.git;
        let target_head = g.out_str(&info.repo_root, &["rev-parse", "HEAD"])?;
        let tip = self
            .pinned(info)?
            .out_str(&info.worktree, &["rev-parse", "HEAD"])?;
        let range = format!("{}..{tip}", info.baseline);
        let commits = g
            .out_str(&info.repo_root, &["rev-list", "--reverse", &range])?
            .lines()
            .map(str::to_string)
            .collect();
        let o = g.run(
            &info.repo_root,
            &[
                "merge-tree",
                "--write-tree",
                "--name-only",
                "-z",
                &target_head,
                &tip,
            ],
            None,
        )?;
        let conflicts = match o.status.code() {
            Some(0) => Vec::new(),
            Some(1) => nul_split_until_blank(&o.stdout),
            _ => {
                g.check(&["merge-tree"], o)?;
                unreachable!()
            }
        };
        let ch = self.changes(info)?;
        let wg = self.pinned(info)?;
        let uncommitted = !self.changes_vs(&wg, &info.worktree, "HEAD")?.is_empty();
        let main_dirty = self.dirty(g, &info.repo_root, &target_head)?;
        let overlap: Vec<String> = ch
            .agent
            .iter()
            .filter(|c| main_dirty.contains_key(&c.path))
            .map(|c| c.path.clone())
            .collect();
        let clean = conflicts.is_empty() && overlap.is_empty() && !uncommitted;
        Ok(MergePlan {
            branch: info.branch.clone(),
            target_head,
            tip,
            commits,
            conflicts,
            uncommitted_in_worktree: uncommitted,
            overlap,
            clean,
        })
    }

    /// Explicit `merge --no-ff` of the session tip into the original tree's current branch.
    /// Refuses a stale or unclean plan; never resets, never forces. Returns the new HEAD.
    pub fn execute_merge(&self, info: &GitInfo, plan: &MergePlan) -> Result<String> {
        if !plan.clean {
            return Err(Error::Refused("merge plan is not clean".into()));
        }
        if info.include_uncommitted && info.baseline != info.main_head {
            return Err(Error::Refused("baseline snapshot contains uncommitted user changes; cherry-pick plan.commits instead".into()));
        }
        let head = self.git.out_str(&info.repo_root, &["rev-parse", "HEAD"])?;
        let tip = self
            .pinned(info)?
            .out_str(&info.worktree, &["rev-parse", "HEAD"])?;
        if head != plan.target_head || tip != plan.tip {
            return Err(Error::Refused("stale merge plan".into()));
        }
        let msg = format!("Merge {}", info.branch);
        if let Err(e) = self.id_out(
            &self.git,
            &info.repo_root,
            &[
                "merge",
                "--no-ff",
                "--no-verify",
                "--no-gpg-sign",
                "-m",
                &msg,
                &tip,
            ],
        ) {
            let merging = self
                .git
                .run(
                    &info.repo_root,
                    &["rev-parse", "-q", "--verify", "MERGE_HEAD"],
                    None,
                )?
                .status
                .success();
            if merging {
                self.git.out(&info.repo_root, &["merge", "--abort"])?;
            }
            return Err(e);
        }
        self.git.out_str(&info.repo_root, &["rev-parse", "HEAD"])
    }

    /// Removes worktree and branch. Without `force` refuses uncommitted work or commits not
    /// yet merged into the original HEAD.
    pub fn cleanup(&self, info: &GitInfo, force: bool) -> Result<()> {
        let g = &self.git;
        if !force {
            let wg = self.pinned(info)?;
            let dirty = self.changes_vs(&wg, &info.worktree, "HEAD")?;
            if !dirty.is_empty() {
                return Err(Error::Refused(format!(
                    "{} uncommitted change(s) in worktree",
                    dirty.len()
                )));
            }
            // `git worktree remove` deletes gitignored files (.env, build output) without --force
            let ignored = nul_split(&wg.out(
                &info.worktree,
                &[
                    "ls-files",
                    "--others",
                    "--ignored",
                    "--exclude-standard",
                    "-z",
                ],
            )?)
            .count();
            if ignored > 0 {
                return Err(Error::Refused(format!(
                    "{ignored} gitignored file(s) in worktree (they would be deleted)"
                )));
            }
            let tip = wg.out_str(&info.worktree, &["rev-parse", "HEAD"])?;
            let merged = tip == info.baseline
                || g.run(
                    &info.repo_root,
                    &["merge-base", "--is-ancestor", &tip, "HEAD"],
                    None,
                )?
                .status
                .success();
            if !merged {
                return Err(Error::Refused("session branch has unmerged commits".into()));
            }
        }
        let wt = info.worktree.to_string_lossy().into_owned();
        let mut rm = vec!["worktree", "remove"];
        if force {
            rm.push("--force");
        }
        rm.push(&wt);
        g.out(&info.repo_root, &rm)?;
        g.out(&info.repo_root, &["branch", "-D", &info.branch])?;
        Ok(())
    }
}

/// `merge-tree -z --name-only` output: `<tree>\0<path>\0...\0\0<messages>`.
fn nul_split_until_blank(b: &[u8]) -> Vec<String> {
    b.split(|c| *c == 0)
        .skip(1)
        .take_while(|s| !s.is_empty())
        .map(|s| String::from_utf8_lossy(s).into_owned())
        .collect()
}
