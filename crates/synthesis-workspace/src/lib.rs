//! Per-session workspace isolation: git worktrees, ports / temp dirs / process registry, and
//! checkpoint file restore (design doc §5.7, §5.10; docs/specs/workspace-context.md).
//!
//! # Threat model
//! The repository (its `.git/config`, `.gitattributes`, `.git/hooks`, files) is UNTRUSTED
//! input: a cloned repo must not be able to run code just because the agent inspects it.
//! Every `git` invocation goes through one helper (`git` module, crate-private) that:
//! * resolves `git` to an absolute path from the injected `PATH` (no cwd lookup on Windows);
//! * uses `env_clear()` plus `synthesis_config::Env::child_env()` (allowlist; no ambient env, no
//!   caller `GIT_*`), then sets `GIT_CONFIG_NOSYSTEM=1`, `GIT_TERMINAL_PROMPT=0`,
//!   `GIT_CONFIG_GLOBAL=<null>` (unless `allow_global_config`), `GIT_OPTIONAL_LOCKS=0`,
//!   `GIT_ATTR_NOSYSTEM=1`, `GIT_PAGER=cat`, `GIT_EDITOR=true`, `GIT_ASKPASS=true`,
//!   `GIT_EXTERNAL_DIFF=`;
//! * passes `-c` (highest precedence, beats repo config): `core.hooksPath=<empty dir>`
//!   (hooks never run, incl. `reference-transaction`), `core.fsmonitor=false`,
//!   `core.sshCommand=false`, `protocol.allow=never`, `diff.external=`, `core.pager=cat`,
//!   `core.editor=true`, `core.askPass=`, `core.attributesFile=<null>`, `credential.helper=`,
//!   `gc.auto=0`, `maintenance.auto=false`, `commit.gpgsign=false`, `tag.gpgsign=false`;
//! * before each command lists the repo config and blanks every `filter.*.{clean,smudge,
//!   process}`, `diff.*.{textconv,command}`, `merge.*.{driver,recursivedriver}` it defines
//!   (and `filter.*.required=false`), because those names are repo-chosen and cannot be
//!   listed statically; diffs additionally use `--no-ext-diff --no-textconv`.
//! * never sets `safe.directory`: a repo owned by another user fails with a git error rather
//!   than being trusted. Never touches the network (`protocol.allow=never`).
//!
//! * worktree calls are pinned: the `<worktree>/.git` pointer file is agent-writable, so
//!   `GitInfo::git_dir` (`<main>/.git/worktrees/<id>`, recorded at creation) is exported as
//!   `GIT_DIR` with `GIT_WORK_TREE=<worktree>`, and before each call `rev-parse
//!   --absolute-git-dir` must equal it, else `Error::Refused`. `commondir` is read by git from
//!   the pinned dir. The main repo's `.git/worktrees/<id>/` is outside the worktree; the
//!   standard sandbox policy makes only `<ws>/.git/hooks` and `<ws>/.git/config` read-only
//!   (a worktree's `.git` is a file, so those do not even exist there); `<main>/.git` is
//!   protected only if it is not in a writable root. An agent that can write
//!   `<main>/.git/worktrees/<id>/` (HEAD, index, commondir) is NOT defended against.
//!
//! Not covered: `core.worktree` in the main repo config is accepted as git sees it;
//! an OS sandbox (R7) is still required for anything the agent itself executes.
//! The user's original tree is never modified, except by an explicit
//! [`WorktreeManager::execute_merge`].

mod git;
pub use git::find_git;
mod resources;
mod restore;
mod worktree;

pub use resources::{CloseReport, PortLease, ResourceManager, SessionResources};
pub use restore::{
    FileAction, Ledger, Refusal, RestoreOutcome, RestorePlan, SessionRewind, hash_bytes,
    restore_files,
};
pub use worktree::{
    Allocation, Change, Changes, GitInfo, MergePlan, WorktreeManager, WorktreeOptions,
};

use std::fmt;

#[derive(Debug)]
pub enum Error {
    GitMissing,
    Git {
        args: String,
        code: Option<i32>,
        stderr: String,
    },
    Io(std::io::Error),
    Resolve(synthesis_core::ResolveError),
    Session(synthesis_session::Error),
    Invalid(String),
    /// A safety check refused the operation; nothing was changed.
    Refused(String),
}

pub type Result<T> = std::result::Result<T, Error>;

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::GitMissing => write!(f, "git not found on PATH"),
            Self::Git { args, code, stderr } => {
                write!(f, "git {args} failed ({code:?}): {stderr}")
            }
            Self::Io(e) => write!(f, "{e}"),
            Self::Resolve(e) => write!(f, "{e}"),
            Self::Session(e) => write!(f, "{e}"),
            Self::Invalid(s) => write!(f, "invalid: {s}"),
            Self::Refused(s) => write!(f, "refused: {s}"),
        }
    }
}
impl std::error::Error for Error {}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}
impl From<synthesis_core::ResolveError> for Error {
    fn from(e: synthesis_core::ResolveError) -> Self {
        Self::Resolve(e)
    }
}
impl From<synthesis_session::Error> for Error {
    fn from(e: synthesis_session::Error) -> Self {
        Self::Session(e)
    }
}
