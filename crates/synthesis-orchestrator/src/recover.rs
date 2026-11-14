//! Startup recovery of attempts left `starting` or `running` by a dead orchestrator
//! (ADR-017). Nothing is rerun here: a lost attempt only frees its work item, and the scheduler
//! allocates a NEW attempt (new session, new worktree), so no effect of the lost one is repeated.

use std::time::Duration;

use synthesis_core::SessionStatus;
use synthesis_core::outcome::{AttemptId, AttemptState, WorkItemState};
use synthesis_session::outcome::{AttemptEnd, AttemptRecord, Effects, OutcomeWriter};
use synthesis_session::{ProcessProbe, Store};
use synthesis_workspace::{GitInfo, WorktreeManager};
use time::OffsetDateTime;

use crate::Platform;
use crate::runner::{RunError, Runner, WorkerEnd};

#[derive(Debug, Default, PartialEq, Eq)]
pub struct RecoveryReport {
    /// Workers still alive with a fresh lease: left alone.
    pub alive: Vec<AttemptId>,
    /// Attempts whose worker is gone and never finished: `lost`, their work item freed.
    pub lost: Vec<AttemptId>,
    /// Attempts whose worker finished but the owner died before recording it: now recorded.
    pub settled: Vec<AttemptId>,
    /// Workers whose lease expired while the process lived (killed, then lost).
    pub expired: Vec<AttemptId>,
}

fn age(at: &Option<String>, now: OffsetDateTime) -> Option<Duration> {
    let t = crate::retention::parse_store_time(at.as_deref()?)?;
    (now - t).try_into().ok()
}

/// Reconciles every live attempt of the outcome. `max_attempts` is the attempt budget per work
/// item (contract under settings); `lease` how long a worker may be silent.
#[allow(clippy::too_many_arguments)]
pub fn recover_attempts(
    store: &Store,
    worktrees: &WorktreeManager,
    probe: &dyn ProcessProbe,
    w: &mut OutcomeWriter,
    now: OffsetDateTime,
    lease: Duration,
    max_attempts: u32,
    platform: Platform,
) -> Result<RecoveryReport, RunError> {
    let mut report = RecoveryReport::default();
    let items: Vec<_> = w.list_work_items(w.id())?;
    for item in items {
        for a in w.list_attempts(&item.id)? {
            if !matches!(a.state, AttemptState::Starting | AttemptState::Running) {
                continue;
            }
            let alive = a
                .worker_pid
                .zip(a.worker_start.as_deref())
                .is_some_and(|(pid, start)| probe.start_time(pid).as_deref() == Some(start));
            if alive {
                let silent = age(&a.heartbeat_at, now).is_none_or(|d| d > lease);
                if !silent {
                    report.alive.push(a.id.clone());
                    continue;
                }
                if let Some(pid) = a.worker_pid {
                    crate::proc::kill_tree(probe, pid);
                }
                report.expired.push(a.id.clone());
            }
            // the worker is gone: did it finish before the owner died?
            let rec = store.reader()?.get_session(&a.session_id)?;
            let end = match rec.status {
                SessionStatus::Completed => Some(WorkerEnd::Completed),
                SessionStatus::Unverified => Some(WorkerEnd::Unverified),
                SessionStatus::Blocked => Some(WorkerEnd::Blocked),
                SessionStatus::Failed => Some(WorkerEnd::Failed),
                SessionStatus::Cancelled => Some(WorkerEnd::Cancelled),
                _ => None,
            };
            let git: Option<GitInfo> = rec
                .git_info
                .as_deref()
                .and_then(|j| serde_json::from_str(j).ok());
            if let (Some(end), Some(git), true) =
                (end, git.clone(), a.state == AttemptState::Running)
            {
                let runner = never_spawns(store, worktrees, probe);
                runner.settle(w, &a, &git, end)?;
                report.settled.push(a.id.clone());
                continue;
            }
            lose(
                store,
                w,
                &a,
                &item.id,
                item.state,
                &rec.status,
                max_attempts,
            )?;
            report.lost.push(a.id.clone());
        }
    }
    let _ = platform;
    Ok(report)
}

struct NoSpawn;
impl crate::runner::Spawn for NoSpawn {
    fn spawn(&self, _: &crate::Profile) -> std::io::Result<std::process::Child> {
        Err(std::io::Error::other("recovery never starts a worker"))
    }
}

fn never_spawns<'a>(
    store: &'a Store,
    worktrees: &'a WorktreeManager,
    probe: &'a dyn ProcessProbe,
) -> Runner<'a> {
    static NO: NoSpawn = NoSpawn;
    Runner {
        store,
        worktrees,
        spawn: &NO,
        probe,
        poll: Duration::from_secs(1),
        grace: Duration::from_secs(1),
    }
}

/// Marks the attempt `lost` and frees its work item: `ready` while the attempt budget allows
/// another attempt, else `terminal_failure`. The session of a worker that died mid-task is
/// marked interrupted; the worktree is kept as it is.
fn lose(
    store: &Store,
    w: &mut OutcomeWriter,
    a: &AttemptRecord,
    item: &synthesis_core::outcome::WorkItemId,
    item_state: WorkItemState,
    session: &SessionStatus,
    max_attempts: u32,
) -> Result<(), RunError> {
    if matches!(
        session,
        SessionStatus::Running | SessionStatus::WaitingApproval | SessionStatus::Verifying
    ) {
        // best effort: the dead worker's lock is free; a failure leaves the status for `recover`
        if let Ok(mut sw) = store.open_attempt_session(w, &a.session_id) {
            let _ = sw.update_status(
                SessionStatus::Interrupted,
                Some("its worker died; found by outcome recovery"),
            );
        }
    }
    let rev = w
        .list_attempts(item)?
        .into_iter()
        .find(|x| x.id == a.id)
        .map(|x| x.revision)
        .unwrap_or(a.revision);
    w.transition_attempt(
        &a.id,
        rev,
        AttemptState::Lost,
        &AttemptEnd {
            retained_reason: Some("lost: its worker died; the worktree is kept".into()),
            ..AttemptEnd::default()
        },
        &Effects::default(),
    )?;
    // Leased/Running -> Recovering -> Ready | TerminalFailure
    let step = |w: &mut OutcomeWriter, to| -> Result<(), RunError> {
        let r = w
            .list_work_items(w.id())?
            .into_iter()
            .find(|i| i.id == *item)
            .map(|i| i.revision)
            .unwrap_or(0);
        w.transition_work_item(item, r, to, &Effects::default())?;
        Ok(())
    };
    if matches!(item_state, WorkItemState::Leased | WorkItemState::Running) {
        step(w, WorkItemState::Recovering)?;
        let to = if a.ordinal < max_attempts {
            WorkItemState::Ready
        } else {
            WorkItemState::TerminalFailure
        };
        step(w, to)?;
    }
    Ok(())
}
