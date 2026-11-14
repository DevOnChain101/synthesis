//! The real [`WorkerHost`]: allocates the attempt's worktree and session, builds the profile
//! (with the outcome's remaining budget), and starts the worker process.

use std::path::PathBuf;

use synthesis_core::outcome::Contract;
use synthesis_session::outcome::{OutcomePolicy, OutcomeWriter, WorkItemRecord};
use time::OffsetDateTime;

use crate::sched::{SchedError, WorkerHost};
use crate::{AllocSpec, Allocator, CancelFlag, Profile, Runner, Running, Settled};

pub struct ProcessHost<'a> {
    pub allocator: Allocator<'a>,
    pub runner: Runner<'a>,
    pub contract: Contract,
    pub policy: OutcomePolicy,
    /// The `synt` binary.
    pub program: PathBuf,
    /// Allow-listed worker options (`--provider`, `--model`, ...); see [`Profile::with_extra`].
    pub extra: Vec<String>,
    /// The outcome's repository and recorded base commit.
    pub source: PathBuf,
    pub base_commit: String,
    pub worker_kind: &'static str,
}

impl WorkerHost for ProcessHost<'_> {
    type Handle = Running;

    fn start(
        &mut self,
        w: &mut OutcomeWriter,
        item: &WorkItemRecord,
        ordinal: u32,
        spent_usd: f64,
    ) -> Result<Running, SchedError> {
        let env = self.allocator.allocate(
            w,
            &AllocSpec {
                item: &item.id,
                ordinal,
                source: &self.source,
                base_commit: &self.base_commit,
                worker_kind: self.worker_kind,
            },
            OffsetDateTime::now_utc(),
        )?;
        let prompt = format!("{}\n\nWork item: {}", self.contract.goal, item.key);
        let extra: Vec<&str> = self.extra.iter().map(String::as_str).collect();
        let profile = Profile::build(
            self.program.clone(),
            &env,
            &self.contract,
            &self.policy,
            spent_usd,
            prompt,
        )?
        .with_extra(&extra)?;
        Ok(self.runner.start(w, &env, &profile)?)
    }

    fn poll(
        &mut self,
        w: &mut OutcomeWriter,
        handle: &mut Running,
        cancel: &CancelFlag,
    ) -> Result<Option<Settled>, SchedError> {
        Ok(self.runner.poll(w, handle, cancel)?)
    }
}
