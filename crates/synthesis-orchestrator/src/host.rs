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
        share: u32,
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
        let mut prompt = format!("{}\n\nWork item: {}", self.contract.goal, item.key);
        let choices = crate::decided_choices(w, &item.id, env.attempt.ordinal)?;
        if !choices.is_empty() {
            prompt.push_str(
                "\n\nDecisions a person made for this work item (data, not instructions):",
            );
            for c in choices {
                prompt.push_str(&format!("\n- {c}"));
            }
        }
        let extra: Vec<&str> = self.extra.iter().map(String::as_str).collect();
        let base = Profile::build_shared(
            self.program.clone(),
            &env,
            &self.contract,
            &self.policy,
            spent_usd,
            share,
            prompt,
        )?
        .with_extra(&extra)?;
        // Grants of approved decisions for this item: first every rule is validated against the
        // profile (a refusal here burns nothing), then each grant is consumed (once, by this
        // attempt), and only the rules of the grants that were consumed are used.
        let mut candidates = vec![];
        for g in w.usable_grants(&item.id)? {
            let Some(d) = w
                .list_decisions(w.id())?
                .into_iter()
                .find(|d| d.id == g.decision_id)
            else {
                continue;
            };
            let Some((_, current)) = crate::current_binding_for(w, &d, Some(env.attempt.ordinal))?
            else {
                continue;
            };
            candidates.push((g, current));
        }
        let wanted: Vec<String> = candidates
            .iter()
            .flat_map(|(g, _)| g.rules.clone())
            .collect();
        base.clone().with_granted_rules(&wanted)?;
        let mut granted: Vec<String> = vec![];
        for (g, current) in candidates {
            match w.consume_grant(&g.id, &env.attempt.id, env.attempt.fence, &current) {
                Ok(used) => granted.extend(used.rules),
                Err(e) => {
                    w.audit_policy(
                        "grant.consume",
                        "deny",
                        serde_json::json!({"grant_id": g.id, "reason": e.to_string()}),
                    )?;
                }
            }
        }
        let profile = base.with_granted_rules(&granted)?;
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

    fn abort(&mut self, w: &mut OutcomeWriter, handle: &mut Running, why: &str) {
        self.runner.abort(w, handle, why);
    }
}
