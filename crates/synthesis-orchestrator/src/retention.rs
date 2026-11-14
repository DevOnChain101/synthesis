//! End of an attempt: the host commits the tree (the subject of evidence), and a finished
//! attempt's worktree is removed only when git says nothing would be lost (ADR-018 items 3, 8;
//! plan D9: 7 days, never an automatic deletion of uncommitted work).

use synthesis_core::TaskStatus;
use synthesis_core::outcome::AttemptState;
use synthesis_session::outcome::{AttemptEnd, AttemptRecord, Effects, OutcomeWriter};
use synthesis_workspace::{Error as WsError, GitInfo, WorktreeManager};
use time::OffsetDateTime;

use crate::AllocError;

/// Commits everything in the attempt's worktree (the agent never commits on its own authority)
/// and records the resulting commit as the attempt's subject. `Running -> Finished`. Returns the
/// subject commit: the new commit, or the current tip when the tree was unchanged.
pub fn finish_attempt(
    worktrees: &WorktreeManager,
    w: &mut OutcomeWriter,
    attempt: &AttemptRecord,
    git: &GitInfo,
    task_status: TaskStatus,
) -> Result<String, AllocError> {
    let committed = worktrees.commit_all(git, &format!("synthesis: attempt {}", attempt.id))?;
    let subject = match committed {
        Some(c) => c,
        None => worktrees.head(git)?,
    };
    let current = w
        .list_attempts(&attempt.work_item_id)?
        .into_iter()
        .find(|a| a.id == attempt.id)
        .ok_or_else(|| AllocError::Effect("the attempt vanished".into()))?;
    w.transition_attempt(
        &attempt.id,
        current.revision,
        AttemptState::Finished,
        &AttemptEnd {
            task_status: Some(task_status),
            subject_commit: Some(subject.clone()),
            retained_reason: None,
        },
        &Effects::default(),
    )?;
    Ok(subject)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Cleanup {
    Removed,
    /// Kept; the reason is recorded on the attempt (`retained_reason`).
    Retained(String),
}

/// Removes a finished attempt's worktree and branch if nothing would be lost; otherwise keeps
/// it and records why. Never forces. The caller makes sure the worker process has exited.
pub fn cleanup_attempt(
    worktrees: &WorktreeManager,
    w: &mut OutcomeWriter,
    attempt: &AttemptRecord,
    git: &GitInfo,
) -> Result<Cleanup, AllocError> {
    if !attempt.state.is_terminal() {
        return Err(AllocError::Effect("the attempt has not finished".into()));
    }
    match worktrees.cleanup(git, false) {
        Ok(()) => Ok(Cleanup::Removed),
        Err(WsError::Refused(why)) => {
            w.set_attempt_retained(&attempt.id, &why)?;
            Ok(Cleanup::Retained(why))
        }
        Err(e) => Err(e.into()),
    }
}

/// Is a finished attempt past its retention (`worktree_retention_days`, default 7)? `ended_at`
/// is the store's timestamp; an unparsable one is never "due".
pub fn retention_due(ended_at: &str, now: OffsetDateTime, days: u32) -> bool {
    let fmt = time::macros::format_description!(
        "[year]-[month]-[day]T[hour]:[minute]:[second].[subsecond digits:3]Z"
    );
    time::PrimitiveDateTime::parse(ended_at, &fmt)
        .map(time::PrimitiveDateTime::assume_utc)
        .is_ok_and(|t| now >= t + time::Duration::days(i64::from(days)))
}

/// Parses a store timestamp.
pub(crate) fn parse_store_time(t: &str) -> Option<OffsetDateTime> {
    let fmt = time::macros::format_description!(
        "[year]-[month]-[day]T[hour]:[minute]:[second].[subsecond digits:3]Z"
    );
    time::PrimitiveDateTime::parse(t, &fmt)
        .ok()
        .map(time::PrimitiveDateTime::assume_utc)
}
