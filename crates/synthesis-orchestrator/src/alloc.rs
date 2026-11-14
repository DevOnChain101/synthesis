//! Attempt allocation (ADR-018 items 1, 2, 4, 5).

use std::path::{Path, PathBuf};

use serde_json::{Value, json};
use synthesis_core::outcome::WorkItemId;
use synthesis_core::{ParentRef, PermissionMode, WorkspaceContext};
use synthesis_session::outcome::{
    AttemptRecord, Effects, NewOutbox, OutboxEffect, OutboxKind, OutboxRecord, OutboxState,
    OutcomeWriter,
};
use synthesis_session::{Error as StoreError, NewSession, Store};
use synthesis_workspace::{Error as WsError, GitInfo, Origin, WorktreeManager};
use time::OffsetDateTime;

/// Where the attempts run. Writable attempts need an OS sandbox: macOS, Linux and WSL2 have one,
/// native Windows has none (ADR-018 item 5, plan D3), so it is refused rather than run unconfined.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Platform {
    pub native_windows: bool,
}

impl Platform {
    pub fn current() -> Self {
        // a Linux build under WSL2 is `cfg!(windows) == false`: the engine runs inside the distro
        Self {
            native_windows: cfg!(windows),
        }
    }
}

#[derive(Debug)]
pub enum AllocError {
    /// Native Windows: writable attempts are not run (names WSL2).
    NativeWindows,
    /// The outcome's folder is not a git repository: attempts need worktrees.
    NotGit(PathBuf),
    Workspace(WsError),
    Store(StoreError),
    /// The effect did not complete (the reason is the effect's receipt).
    Effect(String),
}

impl std::fmt::Display for AllocError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NativeWindows => write!(
                f,
                "outcome attempts write files and need an OS sandbox: they run on macOS, Linux and WSL2, not on native Windows; open the folder in WSL2"
            ),
            Self::NotGit(p) => write!(
                f,
                "{} is not a git repository: an attempt needs its own worktree, so the outcome folder must be a git repository with at least one commit",
                p.display()
            ),
            Self::Workspace(e) => write!(f, "{e}"),
            Self::Store(e) => write!(f, "{e}"),
            Self::Effect(e) => write!(f, "worktree allocation failed: {e}"),
        }
    }
}
impl std::error::Error for AllocError {}
impl From<WsError> for AllocError {
    fn from(e: WsError) -> Self {
        Self::Workspace(e)
    }
}
impl From<StoreError> for AllocError {
    fn from(e: StoreError) -> Self {
        Self::Store(e)
    }
}

pub struct AllocSpec<'a> {
    pub item: &'a WorkItemId,
    pub ordinal: u32,
    /// The outcome's folder (its recorded `repo_root`): where the worktree is made from.
    pub source: &'a Path,
    /// The outcome's recorded base commit (full id); every attempt starts here.
    pub base_commit: &'a str,
    /// `local-inproc` or `local-engine`.
    pub worker_kind: &'a str,
}

/// Everything the attempt's runner is handed. `context.cwd` is the attempt's explicit working
/// directory; nothing else decides it.
#[derive(Debug, Clone)]
pub struct AttemptEnv {
    pub attempt: AttemptRecord,
    pub session_id: String,
    pub context: WorkspaceContext,
    pub git: GitInfo,
    /// The worktree already existed (a replay of an interrupted allocation).
    pub adopted: bool,
}

pub struct Allocator<'a> {
    pub store: &'a Store,
    pub worktrees: &'a WorktreeManager,
    pub platform: Platform,
    /// Session artifacts of attempts live under `<artifact_root>/<session_id>`.
    pub artifact_root: PathBuf,
}

fn new_session_id() -> String {
    // lowercase hex with dashes: no case-folding collision between two attempts on any volume
    uuid::Uuid::new_v4().to_string()
}

impl Allocator<'_> {
    /// Allocates attempt `ordinal` of `spec.item`: records the intent (outbox `worktree.create`
    /// with the session id chosen up front), creates or adopts the worktree and its session, then
    /// inserts the attempt row (ordinal, fence and liveness rules of the store). Idempotent: a
    /// replay after a crash at any step reuses the session id, finds the worktree, and does not
    /// create a second one. An attempt that already left `starting` is returned as stored and its
    /// worktree is not touched.
    pub fn allocate(
        &self,
        w: &mut OutcomeWriter,
        spec: &AllocSpec<'_>,
        now: OffsetDateTime,
    ) -> Result<AttemptEnv, AllocError> {
        if self.platform.native_windows {
            return Err(AllocError::NativeWindows);
        }
        let source = spec
            .source
            .to_str()
            .ok_or_else(|| AllocError::Effect("the outcome folder is not valid UTF-8".into()))?;
        let key = format!("worktree.create:{}:{}", spec.item, spec.ordinal);
        // the session id of a replayed allocation is the one the first call recorded
        let stored = w
            .list_outbox(w.id())?
            .into_iter()
            .find(|r| r.idempotency_key == key);
        let session_id = match &stored {
            Some(r) => payload_session(r)
                .ok_or_else(|| AllocError::Effect("malformed outbox row".into()))?,
            None => new_session_id(),
        };
        let stored_attempt = w
            .list_attempts(spec.item)?
            .into_iter()
            .find(|a| a.ordinal == spec.ordinal);
        if let Some(existing) = stored_attempt
            && existing.state != synthesis_core::outcome::AttemptState::Starting
        {
            return self.env_of(existing, spec.source);
        }
        let outbox = NewOutbox {
            idempotency_key: key.clone(),
            kind: OutboxKind::WorktreeCreate,
            payload: json!({
                "session_id": session_id,
                "work_item": spec.item,
                "ordinal": spec.ordinal,
                "source": source,
                "base_commit": spec.base_commit,
            }),
        };
        w.atomically(|tx| tx.enqueue(&outbox))?;
        let mut effect = WorktreeEffect {
            store: self.store,
            worktrees: self.worktrees,
            control_session: w.control_session_id().to_string(),
            artifact_root: self.artifact_root.clone(),
        };
        w.run_outbox(now, &mut effect)?;
        let row = w
            .list_outbox(w.id())?
            .into_iter()
            .find(|r| r.idempotency_key == key)
            .ok_or_else(|| AllocError::Effect("the outbox row vanished".into()))?;
        if row.state != OutboxState::Done {
            return Err(AllocError::Effect(
                row.receipt
                    .unwrap_or_else(|| "not completed (retry later)".into()),
            ));
        }
        // the effect made (or found) the worktree and the session; now the attempt row
        let attempt = match w.new_attempt(
            spec.item,
            spec.ordinal,
            &session_id,
            spec.worker_kind,
            &Effects::default(),
        ) {
            Ok((a, _)) => a,
            Err(StoreError::AlreadyExists(_)) => w
                .list_attempts(spec.item)?
                .into_iter()
                .find(|a| a.ordinal == spec.ordinal && a.session_id == session_id)
                .ok_or_else(|| {
                    AllocError::Effect(
                        "an attempt of that number exists with another session".into(),
                    )
                })?,
            Err(e) => return Err(e.into()),
        };
        self.env_of(attempt, spec.source)
    }

    /// The environment of a stored attempt, rebuilt from its session row without touching files.
    fn env_of(&self, attempt: AttemptRecord, source: &Path) -> Result<AttemptEnv, AllocError> {
        let rec = self.store.reader()?.get_session(&attempt.session_id)?;
        let git: GitInfo = rec
            .git_info
            .as_deref()
            .and_then(|j| serde_json::from_str(j).ok())
            .ok_or_else(|| {
                AllocError::Effect("the attempt's session has no worktree recorded".into())
            })?;
        let cwd = Path::new(&rec.cwd).to_path_buf();
        let context = WorkspaceContext::open(
            rec.workspace_id.clone(),
            &git.worktree,
            &cwd,
            &[],
            self.artifact_root.join(&attempt.session_id),
            &attempt.session_id,
        )
        .map_err(|e| AllocError::Workspace(WsError::Invalid(e.to_string())))?;
        let _ = source;
        let mut context = context;
        context.git_worktree = Some(context.root.clone());
        Ok(AttemptEnv {
            session_id: attempt.session_id.clone(),
            attempt,
            context,
            git,
            adopted: true,
        })
    }
}

fn payload_session(r: &OutboxRecord) -> Option<String> {
    r.payload
        .get("session_id")
        .and_then(Value::as_str)
        .filter(|s| is_lower_uuid(s))
        .map(str::to_string)
}

/// The ids this crate makes: lowercase, so two attempts never differ only by case.
fn is_lower_uuid(s: &str) -> bool {
    s.len() == 36
        && s.bytes().enumerate().all(|(i, b)| match i {
            8 | 13 | 18 | 23 => b == b'-',
            _ => b.is_ascii_digit() || (b'a'..=b'f').contains(&b),
        })
}

/// The `worktree.create` effect: worktree, then session row with its git info. The session row is
/// written last, so its presence with a recorded worktree is the probe.
struct WorktreeEffect<'a> {
    store: &'a Store,
    worktrees: &'a WorktreeManager,
    control_session: String,
    artifact_root: PathBuf,
}

impl OutboxEffect for WorktreeEffect<'_> {
    fn probe(&mut self, row: &OutboxRecord) -> synthesis_session::Result<Option<String>> {
        if row.kind != OutboxKind::WorktreeCreate {
            return Ok(None);
        }
        let Some(sid) = payload_session(row) else {
            return Ok(None);
        };
        let Ok(rec) = self.store.reader()?.get_session(&sid) else {
            return Ok(None);
        };
        let done = rec
            .git_info
            .as_deref()
            .is_some_and(|j| serde_json::from_str::<GitInfo>(j).is_ok_and(|g| g.worktree.is_dir()));
        Ok(done.then(|| json!({"session_id": sid}).to_string()))
    }

    fn run(&mut self, row: &OutboxRecord) -> Result<String, String> {
        if row.kind != OutboxKind::WorktreeCreate {
            return Err("not a worktree effect".into());
        }
        let sid = payload_session(row).ok_or("malformed payload: session id")?;
        let text = |k: &str| {
            row.payload
                .get(k)
                .and_then(Value::as_str)
                .map(str::to_string)
        };
        let (source, base) = (
            text("source").ok_or("malformed payload: source")?,
            text("base_commit").ok_or("malformed payload: base_commit")?,
        );
        let (alloc, origin) = self
            .worktrees
            .allocate_unstarted(
                Path::new(&source),
                &sid,
                self.artifact_root.join(&sid),
                &base,
            )
            .map_err(|e| e.to_string())?;
        let Some(git) = alloc.git.clone() else {
            return Err(AllocError::NotGit(PathBuf::from(&source)).to_string());
        };
        let new = NewSession {
            id: Some(sid.clone()),
            workspace: &alloc.context,
            model_config_id: "attempt".into(),
            permission_mode: PermissionMode::Default,
            permission_policy_id: "orchestrator".into(),
            budget: synthesis_core::Budget::default(),
            parent: Some(ParentRef {
                session_id: self.control_session.clone(),
                tool_call_id: String::new(),
            }),
            title: None,
        };
        match self.store.create_session(new) {
            Ok(w) => drop(w), // release the session lock: the runner takes it later
            Err(StoreError::AlreadyExists(_)) => {}
            Err(e) => return Err(e.to_string()),
        }
        let info = serde_json::to_string(&git).map_err(|e| e.to_string())?;
        self.store
            .set_git_info_if_unset(&sid, &info)
            .map_err(|e| e.to_string())?;
        Ok(json!({"session_id": sid, "adopted": origin == Origin::Adopted}).to_string())
    }
}
