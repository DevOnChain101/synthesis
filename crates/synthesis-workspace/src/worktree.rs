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
    /// Injected state dir (e.g. `$SYNT_HOME`); hosts the empty hooks dir.
    pub state_dir: PathBuf,
    /// Where worktrees live; default `<repo>/../.synt-worktrees`.
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
    /// The repository's common git directory (shared by all its worktrees), canonical.
    fn common_dir(&self, repo_root: &Path) -> Result<PathBuf> {
        let c = self
            .git
            .out_str(repo_root, &["rev-parse", "--git-common-dir"])?;
        Ok(synthesis_core::canonicalize(repo_root.join(c))?)
    }

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
                .join(".synt-worktrees"),
        };
        fs::create_dir_all(&dir)?;
        let wt = synthesis_core::canonicalize(&dir)?.join(session_id);
        if wt.exists() {
            return Err(Error::Refused(format!("{} already exists", wt.display())));
        }
        let branch = format!("synthesis/{session_id}");
        let wt_s = wt.to_string_lossy().into_owned();
        // `git worktree add` lists every worktree of the repository: two at once read each
        // other's half-written `.git/worktrees/<id>` ("failed to read .../commondir")
        let common = self.common_dir(&repo_root)?;
        with_repo_lock(&common, || {
            g.out(
                &repo_root,
                &[
                    "worktree", "add", "--quiet", "-b", &branch, &wt_s, &main_head,
                ],
            )
        })??;
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
                "--ignore-submodules=all",
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
                "--ignore-submodules=all",
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

    /// `git add <flag>` of everything EXCEPT the submodules (gitlinks of the index). `git add`
    /// otherwise descends into a checked-out submodule, whose git directory the agent can replace
    /// with one that has a filter in its config, and the host would run that filter.
    fn add_all(&self, g: &Git, dir: &Path, flag: &str) -> Result<()> {
        let staged = g.out(dir, &["ls-files", "--stage", "-z"])?;
        let links: Vec<String> = nul_split(&staged)
            .filter_map(|e| {
                let (meta, path) = e.split_once('\t')?;
                meta.starts_with("160000 ")
                    .then(|| format!(":(exclude,literal){path}"))
            })
            .collect();
        let mut args = vec!["add", flag, "--", "."];
        args.extend(links.iter().map(String::as_str));
        g.out(dir, &args).map(|_| ())
    }

    /// Unified diff of the agent's work vs the baseline (untracked files included via
    /// intent-to-add in the agent's own worktree).
    pub fn diff(&self, info: &GitInfo) -> Result<String> {
        let g = self.pinned(info)?;
        self.add_all(&g, &info.worktree, "--intent-to-add")?;
        let d = g.out(
            &info.worktree,
            &[
                "diff",
                "--binary",
                "--no-ext-diff",
                "--no-textconv",
                "--ignore-submodules=all",
                "--no-color",
                &info.baseline,
            ],
        )?;
        Ok(String::from_utf8_lossy(&d).into_owned())
    }

    /// Commits everything in the worktree; returns the new tip, or `None` if nothing changed.
    pub fn commit_all(&self, info: &GitInfo, message: &str) -> Result<Option<String>> {
        let g = self.pinned(info)?;
        self.add_all(&g, &info.worktree, "--all")?;
        if g.run(
            &info.worktree,
            &["diff", "--cached", "--quiet", "--ignore-submodules=all"],
            None,
        )?
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
        // The read-only checks run under the lock too: on Windows a `git worktree add/remove` of
        // another thread rewrites `.git/config`, and a concurrent reader gets "Permission denied".
        let common = self.common_dir(&info.repo_root)?;
        with_repo_lock(&common, || -> Result<()> {
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
        })?
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

/// How long to wait for another process that is adding or removing a worktree of the same
/// repository.
const REPO_LOCK_WAIT: std::time::Duration = std::time::Duration::from_secs(30);

/// The lock file, only when it is (or can become) a plain file: a symlink planted at that path
/// would make this create or lock a file somewhere else, and a FIFO would block the open forever.
/// Neither is followed or opened: the caller then relies on the in-process mutex alone. On unix
/// the open itself also refuses a symlink (`O_NOFOLLOW`) and cannot block (`O_NONBLOCK`), and the
/// opened file is checked to be a regular file (the path may have changed since the check).
fn open_lock_file(path: &Path) -> Option<fs::File> {
    match fs::symlink_metadata(path) {
        Ok(m) if m.file_type().is_file() => {}
        Ok(_) => return None,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(_) => return None,
    }
    let mut o = fs::OpenOptions::new();
    o.create(true).truncate(false).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        let flags = rustix::fs::OFlags::NOFOLLOW | rustix::fs::OFlags::NONBLOCK;
        o.custom_flags(flags.bits() as i32);
    }
    let f = o.open(path).ok()?;
    f.metadata().ok()?.is_file().then_some(f)
}

/// Runs `f` while holding the repository's worktree lock: a process-wide mutex per common git
/// directory (threads of this process) plus an advisory lock file inside it (other processes).
/// The wait for the file is bounded; a file system without locks falls back to the mutex alone.
fn with_repo_lock<T>(common: &Path, f: impl FnOnce() -> T) -> Result<T> {
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex, OnceLock};
    static LOCKS: OnceLock<Mutex<HashMap<PathBuf, Arc<Mutex<()>>>>> = OnceLock::new();
    let m = LOCKS
        .get_or_init(Mutex::default)
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .entry(common.to_path_buf())
        .or_default()
        .clone();
    let _in_process = m.lock().unwrap_or_else(|e| e.into_inner());
    let file = open_lock_file(&common.join("synt-worktree.lock"));
    if let Some(file) = &file {
        let start = std::time::Instant::now();
        loop {
            match file.try_lock() {
                Ok(()) => break,
                Err(fs::TryLockError::WouldBlock) => {
                    if start.elapsed() >= REPO_LOCK_WAIT {
                        return Err(Error::Refused(format!(
                            "another process has been changing the worktrees of {} for {} s; try again",
                            common.display(),
                            REPO_LOCK_WAIT.as_secs()
                        )));
                    }
                    std::thread::sleep(std::time::Duration::from_millis(20));
                }
                Err(_) => break, // no locks on this file system: the in-process mutex only
            }
        }
    }
    let r = f();
    drop(file); // closing the file releases the advisory lock
    Ok(r)
}

/// What a submodule URL may be. `.gitmodules` comes from the repository, so it is untrusted:
/// only `https://`, `ssh://` and scp-like `[user@]host:path` URLs are cloned, with a host that is
/// a plain name or address (never starting with `-`), no credentials in the URL, and `ssh` with at
/// most a user name. `file:`, `ext::` and other transport helpers, `git://`, `http://`, relative
/// paths and option-like values are refused.
#[derive(Debug, Clone, Copy, Default)]
pub struct SubmodulePolicy {
    /// Tests only: a local fixture repository needs the `file` transport.
    #[cfg(test)]
    allow_file: bool,
}

/// Most submodules (all levels together) and deepest nesting one init will clone.
const MAX_SUBMODULES: usize = 50;
const MAX_DEPTH: usize = 3;
/// Total time for all clones; the git process group is killed after it.
const SUBMODULE_DEADLINE: std::time::Duration = std::time::Duration::from_secs(300);
/// Releases that carry the fixes for the submodule path-handling CVEs (CVE-2024-32002 and
/// CVE-2025-48384, which let a crafted `.gitmodules` write or run code outside the checkout):
/// 2.50.1 and later, or the last patched release of each older branch. Anything else is refused
/// (this also covers `GIT_CONFIG_GLOBAL`, which needs 2.32, and the other flags used here).
const PATCHED_GIT: [(u32, u32, u32); 7] = [
    (2, 43, 7),
    (2, 44, 4),
    (2, 45, 4),
    (2, 46, 4),
    (2, 47, 3),
    (2, 48, 2),
    (2, 49, 1),
];

fn git_version_ok(v: (u32, u32, u32)) -> bool {
    v >= (2, 50, 1)
        || PATCHED_GIT
            .iter()
            .any(|&(a, b, c)| v.0 == a && v.1 == b && v.2 >= c)
}

/// `url` without credentials (`user:pass@`), for messages: anything up to the last `@` of the
/// first path segment is dropped.
pub(crate) fn strip_userinfo(url: &str) -> String {
    let start = url.find("://").map_or(0, |i| i + 3);
    let end = url[start..].find('/').map_or(url.len(), |i| start + i);
    match url[start..end].rfind('@') {
        Some(at) => format!("{}{}", &url[..start], &url[start + at + 1..]),
        None => url.to_string(),
    }
}

/// Text from git (an error message) with every URL-looking word stripped of credentials.
fn scrub(text: &str) -> String {
    text.split_whitespace()
        .map(|w| {
            let t = w.trim_matches(['\'', '"', ':', ',']);
            if t.contains('@') {
                strip_userinfo(t)
            } else {
                w.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn host_ok(h: &str) -> bool {
    let h = h
        .strip_prefix('[')
        .and_then(|r| r.strip_suffix(']'))
        .unwrap_or(h);
    !h.is_empty()
        && !h.starts_with('-')
        && h.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'_' | b':'))
}

/// A host that is not a literal private, loopback, link-local or otherwise internal address, and
/// not `localhost`. Ambiguous numeric forms (`2130706433`, `0x7f.1`) are refused. A NAME that
/// resolves to an internal address cannot be seen here (no DNS lookup): that is the network
/// policy's job, see docs/UNATTENDED.md.
fn host_public(h: &str) -> bool {
    let h = h.to_ascii_lowercase();
    if h == "localhost"
        || h.ends_with(".localhost")
        || h.ends_with(".local")
        || h.ends_with(".internal")
    {
        return false;
    }
    let numeric = h.bytes().all(|b| b.is_ascii_digit() || b == b'.') || h.contains("0x");
    if !numeric {
        return true;
    }
    match h.parse::<std::net::Ipv4Addr>() {
        Ok(a) => {
            let o = a.octets();
            !(a.is_private()
                || a.is_loopback()
                || a.is_link_local()
                || a.is_unspecified()
                || a.is_broadcast()
                || a.is_documentation()
                || (o[0] == 100 && (64..128).contains(&o[1])) // carrier-grade NAT
                || o[0] == 0
                || o[0] >= 224)
        }
        Err(_) => false,
    }
}

fn user_ok(u: &str) -> bool {
    !u.is_empty()
        && !u.starts_with('-')
        && u.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'_' | b'+'))
}

impl SubmodulePolicy {
    fn file_allowed(self) -> bool {
        #[cfg(test)]
        return self.allow_file;
        #[cfg(not(test))]
        false
    }

    /// `Err` is the reason; the URL appears only as a short excerpt without credentials.
    fn check(self, url: &str) -> std::result::Result<(), String> {
        let excerpt = || {
            strip_userinfo(url)
                .chars()
                .take(80)
                .collect::<String>()
                .escape_debug()
                .to_string()
        };
        let bad = |why: &str| Err(format!("{why} \"{}\"", excerpt()));
        if url.is_empty()
            || url.starts_with('-')
            || url.chars().any(|c| c.is_control() || c == ' ' || c == '\\')
        {
            return bad("empty or malformed URL");
        }
        let lower = url.to_ascii_lowercase();
        if lower.contains("::") {
            return bad("transport helper");
        }
        if let Some((scheme, rest)) = lower
            .strip_prefix("https://")
            .map(|r| ("https", r))
            .or_else(|| lower.strip_prefix("ssh://").map(|r| ("ssh", r)))
        {
            let authority = rest.split('/').next().unwrap_or("");
            // credentials in a URL are never accepted; ssh may name a user (no password)
            let (user, hostport) = match authority.rsplit_once('@') {
                Some((u, h)) => (Some(u), h),
                None => (None, authority),
            };
            if scheme == "https" && user.is_some() {
                return bad("credentials in the URL");
            }
            if user.is_some_and(|u| !user_ok(u)) {
                return bad("bad user in the URL");
            }
            let host = match hostport.rsplit_once(':') {
                // `host:port`, but not the colons of a bracketed IPv6 address
                Some((h, port)) if !hostport.ends_with(']') && !port.contains(']') => {
                    if port.is_empty() || !port.bytes().all(|b| b.is_ascii_digit()) {
                        return bad("bad port");
                    }
                    h
                }
                _ => hostport,
            };
            return if host_ok(host) && host_public(host) && rest.len() > authority.len() {
                Ok(())
            } else {
                bad("bad host or no path")
            };
        }
        if self.file_allowed() && (lower.starts_with("file://") || Path::new(url).is_absolute()) {
            return Ok(());
        }
        // scp-like `[user@]host:path`: a `:` before any `/`, no scheme
        if let Some((hostpart, path)) = url.split_once(':')
            && !hostpart.contains('/')
            && !path.is_empty()
            && !path.starts_with(':')
            && !path.starts_with("//")
            && hostpart.len() > 1
        {
            let (user, host) = match hostpart.rsplit_once('@') {
                Some((u, h)) => (Some(u), h),
                None => (None, hostpart),
            };
            if user.is_none_or(user_ok) && host_ok(host) && host_public(host) && !host.contains(':')
            {
                return Ok(());
            }
            return bad("bad user or host");
        }
        bad("scheme not allowed (https, ssh only)")
    }
}

/// `(major, minor, patch)` from `git version 2.39.2 (Apple Git-143)`.
fn parse_git_version(s: &str) -> Option<(u32, u32, u32)> {
    let v = s.split_whitespace().nth(2)?;
    let mut p = v.split('.');
    Some((
        p.next()?.parse().ok()?,
        p.next()?.parse().ok()?,
        p.next().and_then(|x| x.parse().ok()).unwrap_or(0),
    ))
}

impl WorktreeManager {
    /// `(name, url)` of the submodules `.gitmodules` of the worktree declares (empty without one).
    pub fn submodules(&self, info: &GitInfo) -> Result<Vec<(String, String)>> {
        Ok(self
            .gitmodules(&self.git, &info.worktree, "url")?
            .into_iter()
            .collect())
    }

    /// `(name, value)` of every `submodule.<name>.<key>` entry of `<dir>/.gitmodules`.
    fn gitmodules(&self, g: &Git, dir: &Path, key: &str) -> Result<Vec<(String, String)>> {
        let file = dir.join(".gitmodules");
        // a symlink would let the repository make git read a file outside the worktree
        match fs::symlink_metadata(&file) {
            Ok(m) if m.is_file() => {}
            Ok(_) => return Err(Error::Refused(".gitmodules is not a regular file".into())),
            Err(_) => return Ok(vec![]),
        }
        let pattern = format!(r"^submodule\..*\.{key}$");
        let o = g.probe(
            dir,
            &[
                "config",
                "--file",
                ".gitmodules",
                "-z",
                "--get-regexp",
                &pattern,
            ],
        )?;
        if o.status.code() == Some(1) {
            return Ok(vec![]); // no matching key
        }
        let out = g.check(&["config", "--file", ".gitmodules"], o)?;
        let suffix = format!(".{key}");
        Ok(nul_split(&out)
            .filter_map(|e| {
                let (k, v) = e.split_once('\n')?;
                let name = k
                    .strip_prefix("submodule")?
                    .strip_prefix('.')?
                    .strip_suffix(suffix.as_str())?;
                Some((name.to_string(), v.to_string()))
            })
            .collect())
    }

    /// Checks out the submodules recursively (`git worktree add` does not), one level at a time so
    /// the URL policy also runs on the `.gitmodules` of every submodule BEFORE it is cloned. All or
    /// nothing at each gate: a refused URL ends the step (what is already cloned stays). Only https
    /// and ssh are enabled as transports; at most `MAX_SUBMODULES` submodules in `MAX_DEPTH`
    /// levels, one `SUBMODULE_DEADLINE` for everything (the git process group is killed after
    /// it), git >= 2.30. An `update = !command` entry or a path that leaves its directory is
    /// refused. Returns the names (nested ones as `parent/path`).
    pub fn init_submodules(&self, info: &GitInfo, policy: SubmodulePolicy) -> Result<Vec<String>> {
        let top = self.submodules(info)?;
        if top.is_empty() {
            return Ok(vec![]);
        }
        let v =
            String::from_utf8_lossy(&self.git.out(&info.worktree, &["--version"])?).into_owned();
        if !policy.file_allowed() && !parse_git_version(&v).is_some_and(git_version_ok) {
            return Err(Error::Refused(format!(
                "a git with the submodule security fixes is needed (2.50.1 or later, or 2.49.1, 2.48.2, 2.47.3, 2.46.4, 2.45.4, 2.44.4, 2.43.7); found: {}",
                v.trim()
            )));
        }
        let started = std::time::Instant::now();
        let root = self.pinned(info)?;
        let mut done = Vec::new();
        // (git to use, directory, name prefix)
        let mut level = vec![(root, info.worktree.clone(), String::new())];
        for depth in 0..MAX_DEPTH {
            let mut next = Vec::new();
            for (g, dir, prefix) in &level {
                let urls = self.gitmodules(g, dir, "url")?;
                if urls.is_empty() {
                    continue;
                }
                let paths: BTreeMap<String, String> =
                    self.gitmodules(g, dir, "path")?.into_iter().collect();
                let updates = self.gitmodules(g, dir, "update")?;
                if done.len() + urls.len() > MAX_SUBMODULES {
                    return Err(Error::Refused(format!(
                        "more than {MAX_SUBMODULES} submodules; nothing more was cloned"
                    )));
                }
                let mut bad: Vec<String> = Vec::new();
                for (n, u) in &urls {
                    if n.chars().any(|c| c.is_control() || c == '=') {
                        bad.push(format!("{}: bad name", n.escape_debug()));
                    }
                    if let Err(e) = policy.check(u) {
                        bad.push(format!("{}: {e}", n.escape_debug()));
                    }
                    match paths.get(n) {
                        Some(p) if safe_rel(p) => {}
                        _ => bad.push(format!(
                            "{}: path missing or leaves its directory",
                            n.escape_debug()
                        )),
                    }
                }
                for (n, u) in &updates {
                    if u.starts_with('!') {
                        bad.push(format!("{}: update runs a command", n.escape_debug()));
                    }
                }
                if !bad.is_empty() {
                    return Err(Error::Refused(format!(
                        "submodule refused at level {depth} ({}); nothing was cloned there",
                        bad.join("; ")
                    )));
                }
                let left = SUBMODULE_DEADLINE
                    .checked_sub(started.elapsed())
                    .ok_or_else(|| Error::Refused("submodule checkout deadline passed".into()))?;
                // every name gets its update mode pinned to a plain checkout
                // url/active come from -c (already checked above), so nothing is written to the
                // shared `.git/config` and no repository lock is needed for the (long) clone
                let guards: Vec<String> = urls
                    .iter()
                    .flat_map(|(n, u)| {
                        [
                            "-c".to_string(),
                            format!("submodule.{n}.update=checkout"),
                            "-c".to_string(),
                            format!("submodule.{n}.url={u}"),
                            "-c".to_string(),
                            format!("submodule.{n}.active=true"),
                        ]
                    })
                    .collect();
                let mut args: Vec<&str> = vec![
                    "-c",
                    "protocol.https.allow=always",
                    "-c",
                    "protocol.ssh.allow=always",
                    "-c",
                    "core.sshCommand=ssh -o BatchMode=yes -o ServerAliveInterval=15 -o ServerAliveCountMax=2",
                    "-c",
                    "http.lowSpeedLimit=1000",
                    "-c",
                    "http.lowSpeedTime=30",
                    "-c",
                    "submodule.recurse=false",
                ];
                if policy.file_allowed() {
                    args.extend(["-c", "protocol.file.allow=always"]);
                }
                args.extend(guards.iter().map(String::as_str));
                args.extend(["submodule", "update", "--"]);
                let o = g.run_deadline(dir, &args, left)?;
                if !o.status.success() {
                    return Err(Error::Refused(format!(
                        "git submodule update failed (exit {}): {}",
                        o.status.code().map_or("none".into(), |c| c.to_string()),
                        scrub(
                            String::from_utf8_lossy(&o.stderr)
                                .lines()
                                .next()
                                .unwrap_or("")
                        )
                    )));
                }
                for (n, _) in &urls {
                    let rel = &paths[n];
                    let name = format!("{prefix}{rel}");
                    let sub = dir.join(rel);
                    if sub.is_dir() {
                        next.push((self.git.clone(), sub, format!("{name}/")));
                    }
                    done.push(name);
                }
            }
            if next.is_empty() {
                return Ok(done);
            }
            level = next;
        }
        // deeper levels than MAX_DEPTH are not cloned
        if level
            .iter()
            .any(|(g, d, _)| self.gitmodules(g, d, "url").is_ok_and(|u| !u.is_empty()))
        {
            return Err(Error::Refused(format!(
                "submodules nested deeper than {MAX_DEPTH} levels were not cloned"
            )));
        }
        Ok(done)
    }
}

/// A relative path made of plain components (no `..`, no root).
fn safe_rel(p: &str) -> bool {
    !p.is_empty()
        && !p.chars().any(char::is_control) // NUL, CR, LF, TAB: they confuse git's config and path parsing
        && Path::new(p)
            .components()
            .all(|c| matches!(c, std::path::Component::Normal(_)))
}

#[cfg(test)]
mod submodule_tests {
    use super::*;

    /// Plain git for fixtures: a clean env that keeps what Windows needs to run anything.
    fn git(dir: &Path, args: &[&str]) {
        let null = if cfg!(windows) { "NUL" } else { "/dev/null" };
        let mut c = std::process::Command::new("git");
        c.env_clear()
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", null);
        for k in [
            "PATH",
            "SYSTEMROOT",
            "SystemRoot",
            "HOME",
            "USERPROFILE",
            "TEMP",
            "TMP",
            "TMPDIR",
        ] {
            if let Some(v) = std::env::var_os(k) {
                c.env(k, v);
            }
        }
        let o = c
            .current_dir(dir)
            .args([
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "-c",
                "init.defaultBranch=main",
            ])
            .args(["-c", "protocol.file.allow=always"])
            .args(args)
            .output()
            .unwrap();
        assert!(
            o.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&o.stderr)
        );
    }

    /// A path as a submodule URL: forward slashes, so Windows paths survive `.gitmodules`.
    fn url_of(p: &Path) -> String {
        p.to_str().unwrap().replace('\\', "/")
    }

    #[test]
    fn urls_other_than_plain_https_and_ssh_are_refused() {
        let p = SubmodulePolicy::default();
        for ok in [
            "https://h.example/r.git",
            "https://h.example:8443/r.git",
            "https://93.184.216.34/r.git",
            "ssh://git@h/r.git",
            "ssh://h:22/r.git",
            "git@h.example:o/r.git",
            "h.example:o/r.git",
        ] {
            assert!(p.check(ok).is_ok(), "{ok}");
        }
        for bad in [
            "file:///etc",
            "/abs/path",
            "../rel",
            "./rel",
            "ext::sh -c id",
            "git://h/r",
            "http://h/r",
            "-uHelper",
            "https:///x",
            "",
            "https://h/r\n--x",
            "FILE:///x",
            "C:\\x",
            "h:",
            "a b:c",
            // credentials, option-like hosts and users
            "https://user:pass@h.example/r.git",
            "https://token@h.example/r.git",
            "ssh://user:pw@h/r.git",
            "ssh://-oProxyCommand=id/r.git",
            "ssh://u@-oFoo/r.git",
            "u@-oFoo:p",
            "-u@h:p",
            "ssh://h:/r.git",
            "ssh://h:2x/r.git",
            "https://h.example",
            "https://h.example@evil/r.git",
            "u:p@h:path",
            "https://127.0.0.1/r.git",
            "https://10.0.0.5/r.git",
            "ssh://169.254.169.254/r.git",
            "git@192.168.1.1:o/r.git",
            "https://localhost/r.git",
            "https://2130706433/r.git",
            "https://0x7f.1/r.git",
            "https://100.64.0.1/r.git",
            "https://[::1]/r.git", // "::" is a transport-helper marker; IPv6 literals are not supported
        ] {
            assert!(p.check(bad).is_err(), "{bad}");
        }
        // the test-only policy is the only way to get a file URL through
        assert!(
            SubmodulePolicy { allow_file: true }
                .check("/abs/path")
                .is_ok()
        );
    }

    #[test]
    fn credentials_never_reach_messages() {
        assert_eq!(
            strip_userinfo("https://user:pw@h.example/r.git"),
            "https://h.example/r.git"
        );
        assert_eq!(strip_userinfo("u@h:p/q"), "h:p/q");
        assert_eq!(strip_userinfo("https://h/a@b"), "https://h/a@b");
        let e = SubmodulePolicy::default()
            .check("https://user:s3cret@h.example/r.git")
            .unwrap_err();
        assert!(!e.contains("s3cret") && !e.contains("user:"), "{e}");
        assert!(
            !scrub("fatal: unable to access 'https://user:s3cret@h/r.git/': x").contains("s3cret")
        );
    }

    #[test]
    fn git_versions_parse() {
        assert_eq!(
            parse_git_version("git version 2.39.2 (Apple Git-143)"),
            Some((2, 39, 2))
        );
        assert_eq!(parse_git_version("git version 2.50"), Some((2, 50, 0)));
        assert_eq!(parse_git_version("nonsense"), None);
        for ok in [
            (2, 50, 1),
            (2, 51, 0),
            (3, 0, 0),
            (2, 45, 4),
            (2, 49, 1),
            (2, 43, 9),
        ] {
            assert!(git_version_ok(ok), "{ok:?}");
        }
        for bad in [
            (2, 50, 0),
            (2, 45, 3),
            (2, 39, 5),
            (2, 30, 0),
            (2, 49, 0),
            (2, 42, 9),
            (1, 99, 9),
        ] {
            assert!(!git_version_ok(bad), "{bad:?}");
        }
        assert!(
            safe_rel("vendor/sub")
                && !safe_rel("a\tb")
                && !safe_rel("a\nb")
                && !safe_rel("a\rb")
                && !safe_rel("../x")
        );
    }

    fn repo_with_submodule(base: &Path, extra_gitmodules: Option<&str>) -> (PathBuf, PathBuf) {
        let (sub, repo) = (base.join("sub"), base.join("repo"));
        for d in [&sub, &repo] {
            fs::create_dir_all(d).unwrap();
            git(d, &["init", "-q"]);
        }
        fs::write(sub.join("lib.txt"), "lib\n").unwrap();
        git(&sub, &["add", "."]);
        git(&sub, &["commit", "-qm", "sub"]);
        fs::write(repo.join("a.txt"), "a\n").unwrap();
        git(&repo, &["add", "."]);
        git(&repo, &["commit", "-qm", "init"]);
        git(
            &repo,
            &["submodule", "add", "-q", &url_of(&sub), "vendor/sub"],
        );
        if let Some(x) = extra_gitmodules {
            let mut t = fs::read_to_string(repo.join(".gitmodules")).unwrap();
            t.push_str(x);
            fs::write(repo.join(".gitmodules"), t).unwrap();
            git(&repo, &["add", ".gitmodules"]);
        }
        git(&repo, &["commit", "-qm", "add submodule"]);
        (sub, repo)
    }

    fn manager(base: &Path) -> WorktreeManager {
        WorktreeManager::new(
            &Env::from_process(),
            WorktreeOptions::new(base.join("state")),
        )
        .unwrap()
    }

    #[test]
    fn submodules_are_checked_out_only_through_the_policy() {
        if std::process::Command::new("git")
            .arg("--version")
            .output()
            .is_err()
        {
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let base = synthesis_core::canonicalize(tmp.path()).unwrap();
        let (_sub, repo) = repo_with_submodule(&base, None);
        let mgr = manager(&base);
        let a = mgr.create(&repo, "s1", base.join("art"), false).unwrap();
        let info = a.git.unwrap();
        let wt_file = info.worktree.join("vendor/sub/lib.txt");
        let before = fs::read_to_string(repo.join(".git/config")).unwrap();
        assert!(
            !wt_file.exists(),
            "git worktree add does not populate submodules"
        );
        assert_eq!(mgr.submodules(&info).unwrap().len(), 1);
        // the default policy refuses the file URL: nothing is cloned
        let e = mgr
            .init_submodules(&info, SubmodulePolicy::default())
            .unwrap_err();
        assert!(e.to_string().contains("nothing was cloned"), "{e}");
        assert!(!wt_file.exists());
        let names = mgr
            .init_submodules(&info, SubmodulePolicy { allow_file: true })
            .unwrap();
        assert_eq!(names, ["vendor/sub"]);
        assert_eq!(fs::read_to_string(wt_file).unwrap(), "lib\n");
        // nothing was written to the shared config of the repository
        assert_eq!(
            fs::read_to_string(repo.join(".git/config")).unwrap(),
            before,
            "the shared config of the repository was written"
        );
    }

    #[test]
    fn an_update_command_entry_is_refused() {
        if std::process::Command::new("git")
            .arg("--version")
            .output()
            .is_err()
        {
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let base = synthesis_core::canonicalize(tmp.path()).unwrap();
        let (_sub, repo) = repo_with_submodule(&base, Some("\tupdate = !touch /tmp/pwned\n"));
        let mgr = manager(&base);
        // current git already refuses such a value when it reads the repository; either way
        // nothing is cloned and nothing runs
        match mgr.create(&repo, "s2", base.join("art"), false) {
            Err(e) => assert!(e.to_string().contains("invalid value"), "{e}"),
            Ok(a) => {
                let info = a.git.unwrap();
                let e = mgr
                    .init_submodules(&info, SubmodulePolicy { allow_file: true })
                    .unwrap_err()
                    .to_string();
                assert!(e.contains("update runs a command"), "{e}");
                assert!(!info.worktree.join("vendor/sub/lib.txt").exists());
            }
        }
        assert!(!Path::new("/tmp/pwned").exists());
    }

    /// A helper that left the process group and keeps the pipes open must not hold the call.
    #[cfg(unix)]
    #[test]
    fn an_escaped_grandchild_holding_the_pipes_does_not_hang_the_deadline() {
        if std::process::Command::new("git")
            .arg("--version")
            .output()
            .is_err()
            || std::process::Command::new("python3")
                .arg("--version")
                .output()
                .is_err()
        {
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let base = synthesis_core::canonicalize(tmp.path()).unwrap();
        git(&base, &["init", "-q"]);
        let mgr = manager(&base);
        let alias = "alias.slow=!python3 -c \"import subprocess,time; subprocess.Popen(['sleep','30'], start_new_session=True); time.sleep(30)\"";
        let t0 = std::time::Instant::now();
        let e = mgr
            .git
            .run_deadline(
                &base,
                &["-c", alias, "slow"],
                std::time::Duration::from_millis(300),
            )
            .unwrap_err()
            .to_string();
        assert!(e.contains("was killed"), "{e}");
        assert!(
            t0.elapsed() < std::time::Duration::from_secs(8),
            "took {:?}",
            t0.elapsed()
        );
    }

    #[test]
    fn a_stuck_git_is_killed_at_the_deadline() {
        if std::process::Command::new("git")
            .arg("--version")
            .output()
            .is_err()
        {
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let base = synthesis_core::canonicalize(tmp.path()).unwrap();
        git(&base, &["init", "-q"]);
        let mgr = manager(&base);
        // an alias that waits stands in for a stalled transfer (on Windows a tree: sh -> ping, which
        // taskkill /T must end)
        let slow = if cfg!(windows) {
            "alias.slow=!ping -n 60 127.0.0.1 > /dev/null"
        } else {
            "alias.slow=!sleep 30"
        };
        let t0 = std::time::Instant::now();
        let e = mgr
            .git
            .run_deadline(
                &base,
                &["-c", slow, "slow"],
                std::time::Duration::from_millis(300),
            )
            .unwrap_err()
            .to_string();
        assert!(
            e.contains("was killed") && t0.elapsed() < std::time::Duration::from_secs(10),
            "{e}"
        );
    }

    #[test]
    fn nested_submodules_are_checked_out_level_by_level() {
        if std::process::Command::new("git")
            .arg("--version")
            .output()
            .is_err()
        {
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let base = synthesis_core::canonicalize(tmp.path()).unwrap();
        let mk = |n: &str| {
            let d = base.join(n);
            fs::create_dir_all(&d).unwrap();
            git(&d, &["init", "-q"]);
            fs::write(d.join("f.txt"), n).unwrap();
            git(&d, &["add", "."]);
            git(&d, &["commit", "-qm", n]);
            d
        };
        let (inner, sub, repo) = (mk("inner"), mk("sub"), mk("repo"));
        git(&sub, &["submodule", "add", "-q", &url_of(&inner), "deep"]);
        git(&sub, &["commit", "-qm", "nest"]);
        git(
            &repo,
            &["submodule", "add", "-q", &url_of(&sub), "vendor/sub"],
        );
        git(&repo, &["commit", "-qm", "add"]);
        let mgr = manager(&base);
        let info = mgr
            .create(&repo, "s3", base.join("art"), false)
            .unwrap()
            .git
            .unwrap();
        let names = mgr
            .init_submodules(&info, SubmodulePolicy { allow_file: true })
            .unwrap();
        assert_eq!(names, ["vendor/sub", "vendor/sub/deep"]);
        assert_eq!(
            fs::read_to_string(info.worktree.join("vendor/sub/deep/f.txt")).unwrap(),
            "inner"
        );
        // without the test policy even the first level is refused before any clone
        let info2 = mgr
            .create(&repo, "s4", base.join("art"), false)
            .unwrap()
            .git
            .unwrap();
        assert!(
            mgr.init_submodules(&info2, SubmodulePolicy::default())
                .is_err()
        );
    }

    /// PoC: the agent can rewrite the `.git` file of a checked-out submodule to point at a git
    /// directory it controls, with a clean filter and a `.gitattributes` using it. The host's own
    /// git calls on the superproject (changes, diff, commit) must never descend into it.
    #[cfg(unix)]
    #[test]
    fn a_filter_planted_in_a_submodule_gitdir_never_runs_on_the_host() {
        if std::process::Command::new("git")
            .arg("--version")
            .output()
            .is_err()
        {
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let base = synthesis_core::canonicalize(tmp.path()).unwrap();
        let (_sub, repo) = repo_with_submodule(&base, None);
        let mgr = manager(&base);
        let info = mgr
            .create(&repo, "s5", base.join("art"), false)
            .unwrap()
            .git
            .unwrap();
        mgr.init_submodules(&info, SubmodulePolicy { allow_file: true })
            .unwrap();
        let subdir = info.worktree.join("vendor/sub");
        // the agent's move: a gitdir of its own, a filter in it, an attribute that selects it
        let gitfile = fs::read_to_string(subdir.join(".git")).unwrap();
        let real = subdir.join(gitfile.trim().strip_prefix("gitdir: ").unwrap());
        let real = synthesis_core::canonicalize(real).unwrap();
        let evil = base.join("evil-gitdir");
        let cp = std::process::Command::new("cp")
            .arg("-R")
            .arg(&real)
            .arg(&evil)
            .status()
            .unwrap();
        assert!(cp.success());
        let marker = base.join("PWNED");
        let mut cfg = fs::read_to_string(evil.join("config")).unwrap();
        cfg.push_str(&format!(
            "[core]\n\tworktree = {w}\n[filter \"x\"]\n\tclean = touch {m}; cat\n\tsmudge = cat\n\trequired = false\n",
            w = subdir.display(),
            m = marker.display()
        ));
        fs::write(evil.join("config"), cfg).unwrap();
        fs::write(subdir.join(".git"), format!("gitdir: {}\n", evil.display())).unwrap();
        fs::write(subdir.join(".gitattributes"), "* filter=x\n").unwrap();
        // same size as before: git must hash the file (and so run the filter) to see the change
        fs::write(subdir.join("lib.txt"), "LIB\n").unwrap();
        fs::write(subdir.join("new.txt"), "new\n").unwrap();
        let _ = mgr.changes(&info);
        assert!(!marker.exists(), "changes() ran a submodule filter");
        let _ = mgr.diff(&info);
        assert!(!marker.exists(), "diff() ran a submodule filter");
        let _ = mgr.commit_all(&info, "agent work");
        assert!(
            !marker.exists(),
            "a filter of a submodule's git directory ran on the host"
        );
    }
}
