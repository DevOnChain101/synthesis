//! Status aggregation (verification.md §3), completion gate (§4) and bounded
//! self-correction (§5).

use crate::baseline::{Baseline, Drift};
use crate::plan::{
    detect_checks, is_noop_check, plan_checks, plan_checks_from, referenced_scripts,
};
use crate::runner::{CheckAuthorizer, VerifyRunner};
use crate::types::*;
use crate::weaken::{FileEdit, removed_prerequisite_guards, weakened_tests};
use std::collections::{BTreeSet, HashMap};
use std::path::Path;
use std::sync::Arc;
use synthesis_core::WorkspaceContext;
use synthesis_events::TaskStatus;
use tokio_util::sync::CancellationToken;

/// Open question in verification.md §3 rule 4 / doc §13.7.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum InferredOnlyPolicy {
    /// Inferred checks alone never give `passed`; the report caps at `unverified`.
    CapUnverified,
    /// verification.md §3 rule 4: no failed check and at least one passed -> `passed`.
    #[default]
    AllowPassed,
}

#[derive(Debug, Clone)]
pub struct GateConfig {
    pub max_verify_attempts: u32,
    pub inferred_only: InferredOnlyPolicy,
}

impl Default for GateConfig {
    fn default() -> Self {
        Self {
            max_verify_attempts: synthesis_events::Budget::default().max_verify_attempts,
            inferred_only: InferredOnlyPolicy::default(),
        }
    }
}

/// What the harness knows about the task when the model ends its turn.
#[derive(Debug, Clone, Default)]
pub struct TaskState {
    pub task_id: String,
    pub session_id: String,
    pub criteria: Vec<Criterion>,
    /// Checks the model registered through `verify_run`.
    pub model_checks: Vec<Check>,
    /// Any Write/Execute side effect happened in the task.
    pub side_effects: bool,
    pub changed_files: Vec<String>,
    /// Line diffs of the files the agent edited (for [`weakened_tests`]).
    pub edits: Vec<FileEdit>,
    /// Check command -> skipped + todo tests it already had before the agent's edits. Empty when
    /// no baseline run exists, which makes every skip count against the check.
    pub baseline_skipped: HashMap<String, u64>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum GateDecision {
    /// `None` = the gate did not apply (no side effects, no criteria): `verification_report_id: null`.
    Complete(Option<Box<VerificationReport>>),
    /// Append `feedback` as a user message and go back to `running`.
    Continue {
        feedback: String,
        report: Box<VerificationReport>,
    },
    Stop {
        status: TaskStatus,
        reason: String,
        report: Option<Box<VerificationReport>>,
    },
}

/// Computes criterion statuses in place and returns (report status, limitations).
pub fn aggregate(
    criteria: &mut [Criterion],
    checks: &[CheckResult],
    policy: InferredOnlyPolicy,
) -> (VerificationStatus, Vec<String>) {
    use VerificationStatus::*;
    for c in criteria.iter_mut() {
        let linked: Vec<_> = checks
            .iter()
            .filter(|k| k.criteria_ids.contains(&c.id))
            .collect();
        c.evidence = linked.iter().map(|k| k.id.clone()).collect();
        let worst = linked.iter().map(|k| k.status).max_by_key(|s| s.severity());
        c.status = match worst {
            None => Unverified,
            Some(Failed) => Failed,
            _ if linked.iter().any(|k| k.status == Passed) => Passed,
            Some(w) => w,
        };
    }
    let mut lim = vec![];
    for k in checks
        .iter()
        .filter(|k| k.status != Passed && k.status != Failed)
    {
        if let Some(t) = k.tests.as_ref().filter(|t| t.skipped + t.todo > 0) {
            lim.push(format!(
                "{SKIPPED_TESTS}: check {} ({}) ran {} passed, {} failed but {} skipped and {} todo{}",
                k.id,
                k.command.as_deref().unwrap_or("no command"),
                t.passed,
                t.failed,
                t.skipped,
                t.todo,
                if t.skipped_names.is_empty() {
                    String::new()
                } else {
                    format!(": {}", t.skipped_names.join(" | "))
                }
            ));
            continue;
        }
        lim.push(format!(
            "check {} ({}) was {}: {}",
            k.id,
            k.command.as_deref().unwrap_or("no command"),
            k.status.as_str(),
            k.blocked_reason.as_deref().unwrap_or("it proves nothing")
        ));
    }
    // A planned (harness or user) test check must pass: lint or build evidence never verifies a
    // task that has tests, and a blocked or not-run test is not "somewhere else passed".
    let test_kind = |k: &&CheckResult| {
        matches!(
            k.kind,
            CheckKind::Unit | CheckKind::Integration | CheckKind::BrowserE2e
        ) && k.declared_by != Declarer::Model
    };
    let tests_pending = checks
        .iter()
        .filter(test_kind)
        .any(|k| k.status != Passed && k.status != Failed);
    for k in checks
        .iter()
        .filter(test_kind)
        .filter(|k| k.status != Passed && k.status != Failed)
    {
        lim.push(format!(
            "{PLANNED_TEST}: {} ({}) was {}; lint or build results do not verify a task with tests planned",
            k.id,
            k.command.as_deref().unwrap_or("no command"),
            k.status.as_str()
        ));
    }
    // Model-registered checks add evidence but never stand in for a planned check.
    let planned_pass = checks
        .iter()
        .any(|k| k.status == Passed && k.declared_by != Declarer::Model);
    if !planned_pass && checks.iter().any(|k| k.status == Passed) {
        lim.push(format!(
            "{MODEL_ONLY} passed: they add evidence but never replace a planned check"
        ));
    }
    let user: Vec<_> = criteria
        .iter()
        .filter(|c| c.source == CriterionSource::User)
        .collect();
    let status = if checks.iter().any(|k| k.status == Failed) {
        Failed
    } else if user.iter().any(|c| c.status == Blocked) {
        Blocked
    } else if user.iter().any(|c| c.status != Passed) || tests_pending {
        Unverified
    } else if user.is_empty() {
        // `Failed` is excluded above; with no planned test check pending, a planned pass is enough.
        if !checks.is_empty() {
            lim.push("no explicit acceptance criteria; inferred checks only".into());
        }
        if planned_pass && policy == InferredOnlyPolicy::AllowPassed {
            Passed
        } else {
            Unverified
        }
    } else {
        Passed
    };
    if checks.is_empty() {
        lim.push("no checks could be planned or run".into());
    }
    lim.push("a pass covers only what the listed checks exercise, not all behavior".into());
    (status, lim)
}

/// Whether the failure output names a file the agent changed (a real regression, not noise):
/// the full workspace-relative path, with either separator.
fn mentions_changed(summary: &str, changed: &[String]) -> bool {
    changed
        .iter()
        .any(|f| summary.contains(f.as_str()) || summary.contains(&f.replace('/', "\\")))
}

/// A passed check that skipped tests (beyond its baseline) is not a clean pass.
fn downgrade_skips(checks: &mut [CheckResult], baseline: &HashMap<String, u64>) {
    for k in checks
        .iter_mut()
        .filter(|k| k.status == VerificationStatus::Passed)
    {
        let Some(t) = &k.tests else { continue };
        let was = k.command.as_ref().and_then(|c| baseline.get(c)).copied();
        if t.skipped + t.todo > was.unwrap_or(0) {
            k.status = VerificationStatus::Unverified;
        }
    }
}

/// The planned inferred checks and the workspace as they were when the session's first task
/// started (verification.md §3b). Taking it reads the whole workspace: run it on a blocking thread.
pub struct TaskStart {
    baseline: Baseline,
    frozen: Vec<Check>,
}

impl TaskStart {
    pub fn take(ws: &WorkspaceContext) -> Self {
        let frozen = detect_checks(&ws.cwd);
        let prefix = ws.cwd.strip_prefix(&ws.root).unwrap_or(Path::new(""));
        let extra = referenced_scripts(&ws.cwd, &frozen)
            .into_iter()
            .map(|s| prefix.join(s).to_string_lossy().replace('\\', "/"))
            .collect();
        Self {
            baseline: Baseline::take(&ws.root, extra),
            frozen,
        }
    }
}

/// Per-task gate: create one per task, call `evaluate` each time the model ends its turn.
pub struct CompletionGate {
    runner: VerifyRunner,
    cfg: GateConfig,
    attempt: u32,
    unverified_round_used: bool,
    prev: Option<Prev>,
    /// Frozen checks and the workspace snapshot ([`begin`](Self::begin)).
    start: Option<Arc<TaskStart>>,
    /// Commands that failed and then passed on a re-run in this task.
    flaky: BTreeSet<String>,
}

struct Prev {
    fingerprints: BTreeSet<String>,
    changed: Vec<String>,
    by_command: HashMap<String, VerificationStatus>,
}

impl CompletionGate {
    pub fn new(runner: VerifyRunner, cfg: GateConfig) -> Self {
        Self {
            runner,
            cfg,
            attempt: 0,
            unverified_round_used: false,
            prev: None,
            start: None,
            flaky: BTreeSet::new(),
        }
    }

    /// Call when the task starts, before the agent changes anything, with the [`TaskStart`] of
    /// the session (taken once, reused by later tasks until one ends verified, so weakening
    /// carries over from "continue" prompts). Without it the gate re-reads the project at
    /// evaluation time and cannot see shell-made changes.
    pub fn begin(&mut self, start: Arc<TaskStart>) {
        self.start = Some(start);
    }

    /// [`begin`](Self::begin) with a fresh snapshot of this gate's workspace.
    pub fn begin_now(&mut self) -> Arc<TaskStart> {
        let s = Arc::new(TaskStart::take(self.runner.workspace()));
        self.start = Some(s.clone());
        s
    }

    /// `claimed_done == false` means the harness forces a final evaluation (budget exhausted):
    /// it never returns `Continue`, and the status comes from the report alone.
    pub async fn evaluate(
        &mut self,
        st: &TaskState,
        claimed_done: bool,
        cancel: &CancellationToken,
    ) -> GateDecision {
        self.evaluate_with(st, claimed_done, cancel, None).await
    }

    /// [`evaluate`](Self::evaluate) where each check command is first authorised by `auth`; a
    /// refused check is reported `blocked` (never failed, never skipped silently).
    pub async fn evaluate_with(
        &mut self,
        st: &TaskState,
        claimed_done: bool,
        cancel: &CancellationToken,
        auth: Option<&dyn CheckAuthorizer>,
    ) -> GateDecision {
        let has_user = st
            .criteria
            .iter()
            .any(|c| c.source == CriterionSource::User);
        // What changed, from the baseline snapshot as well as from the tools' own records: a
        // shell command that looked read-only can still have changed files.
        let drift = match self.start.clone() {
            Some(t) => Some(
                tokio::task::spawn_blocking(move || t.baseline.drift())
                    .await
                    .unwrap_or_else(|_| Drift {
                        incomplete: true,
                        ..Default::default()
                    }),
            ),
            None => None,
        };
        let drifted = drift.as_ref().is_some_and(|d| !d.changed.is_empty());
        if !has_user && !st.side_effects && !drifted {
            return GateDecision::Complete(None);
        }
        self.attempt += 1;
        let started_at = now();
        let mut changed_files = st.changed_files.clone();
        if let Some(d) = &drift {
            for f in &d.changed {
                if !changed_files.contains(f) {
                    changed_files.push(f.clone());
                }
            }
            changed_files.sort();
        }
        let cwd = self.runner.workspace().cwd.clone();
        let plan = match &self.start {
            Some(t) => plan_checks_from(
                &st.criteria,
                &st.model_checks,
                &cwd,
                t.frozen.clone(),
                &changed_files,
            ),
            None => plan_checks(&st.criteria, &st.model_checks, &cwd, &changed_files),
        };
        let mut checks = self.runner.run_with(&plan, cancel, auth).await;
        let mut limitations = vec![];
        for k in checks.iter_mut() {
            if k.status != VerificationStatus::Passed {
                continue;
            }
            let cmd = k.command.clone().unwrap_or_default();
            // A planned test check must have counted at least one passing test.
            let counted = k.tests.as_ref().is_some_and(|t| t.passed > 0);
            let why = if is_noop_check(&cwd, &cmd) {
                Some("runs nothing that can fail")
            } else if !counted
                && k.declared_by != Declarer::Model
                && matches!(
                    k.kind,
                    CheckKind::Unit | CheckKind::Integration | CheckKind::BrowserE2e
                )
            {
                Some("exited 0 but its output counts no passing test")
            } else {
                None
            };
            if let Some(why) = why {
                k.status = VerificationStatus::Unverified;
                k.blocked_reason = Some(format!("proves nothing: {why}"));
                limitations.push(format!("{NOOP_CHECK}: {} ({cmd}) {why}", k.id));
            }
        }
        downgrade_skips(&mut checks, &st.baseline_skipped);
        // A harness or model check failing without mentioning what the agent changed gets one
        // re-run. A pass on the re-run is flaky: the check stays `failed` (never silently
        // upgraded or downgraded) and says so.
        for i in 0..checks.len() {
            let k = &checks[i];
            // A user-declared check is about the change itself: its failure is real.
            if k.status != VerificationStatus::Failed
                || k.declared_by == Declarer::User
                || k.timed_out
                || mentions_changed(&k.summary, &changed_files)
            {
                continue;
            }
            let again = self.runner.run_one_with(&plan[i], cancel, auth).await;
            if again.status == VerificationStatus::Passed {
                let cmd = again.command.clone().unwrap_or_default();
                limitations.push(format!(
                    "{FLAKY}: check {} ({cmd}) failed, then passed on an identical re-run; reported as failed",
                    again.id
                ));
                self.flaky.insert(cmd);
            }
        }
        // A command that flaked earlier in this task passes only if it passes 3 runs in a row.
        for i in 0..checks.len() {
            let flaky_before = checks[i].status == VerificationStatus::Passed
                && checks[i]
                    .command
                    .as_ref()
                    .is_some_and(|c| self.flaky.contains(c));
            if !flaky_before {
                continue;
            }
            for _ in 0..2 {
                let again = self.runner.run_one_with(&plan[i], cancel, auth).await;
                if again.status != VerificationStatus::Passed {
                    limitations.push(format!(
                        "{FLAKY}: check {} passed once but failed on a confirmation run; reported as failed",
                        again.id
                    ));
                    checks[i] = again;
                    break;
                }
            }
            if checks[i].status == VerificationStatus::Passed {
                limitations.push(format!(
                    "{FLAKY}: check {} flaked earlier in this task; it now passed 3 runs in a row",
                    checks[i].id
                ));
            }
        }
        let mut criteria = st.criteria.clone();
        let (mut status, lim) = aggregate(&mut criteria, &checks, self.cfg.inferred_only);
        limitations.extend(lim);
        // Editing what the checks assert or run voids a pass: it is reported, never `passed`.
        let mut weak: Vec<String> = vec![];
        // Tool diffs and snapshot diffs together, one edit per path (the snapshot's wins).
        let mut edits: Vec<FileEdit> = drift.as_ref().map_or(vec![], |d| d.edits.clone());
        let seen: BTreeSet<String> = edits.iter().map(|e| e.path.clone()).collect();
        edits.extend(st.edits.iter().filter(|e| !seen.contains(&e.path)).cloned());
        weak.extend(weakened_tests(&edits));
        weak.extend(removed_prerequisite_guards(&edits));
        if let Some(d) = &drift {
            if !d.weakened.is_empty() {
                weak.push(format!("{TEST_FILES_MODIFIED}: {}", d.weakened.join(", ")));
            }
            if !d.defs.is_empty() {
                weak.push(format!(
                    "{DEFS_CHANGED} during the task: {}; human review required",
                    d.defs.join(", ")
                ));
            }
            if d.incomplete {
                weak.push(format!(
                    "{TEST_FILES_MODIFIED}: workspace too large to snapshot completely; changes may be missed"
                ));
            }
        }
        for l in weak {
            if status == VerificationStatus::Passed {
                status = VerificationStatus::Unverified;
            }
            limitations.push(l);
        }

        if let Some(p) = &self.prev {
            for k in checks
                .iter()
                .filter(|k| k.status == VerificationStatus::Failed)
            {
                let was = k.command.as_ref().and_then(|c| p.by_command.get(c));
                if was == Some(&VerificationStatus::Passed) {
                    limitations.push(format!(
                        "regression: check {} ({}) passed in the previous attempt and now fails",
                        k.id,
                        k.command.as_deref().unwrap_or("")
                    ));
                }
            }
        }
        let fingerprints: BTreeSet<String> = checks
            .iter()
            .filter_map(|k| k.failure_fingerprint.clone())
            .collect();
        let no_progress = status == VerificationStatus::Failed
            && self
                .prev
                .as_ref()
                .is_some_and(|p| p.fingerprints == fingerprints && p.changed == changed_files);
        self.prev = Some(Prev {
            fingerprints,
            changed: changed_files.clone(),
            by_command: checks
                .iter()
                .filter_map(|k| Some((k.command.clone()?, k.status)))
                .collect(),
        });

        let report = Box::new(VerificationReport {
            id: format!("vr-{}-{}", st.task_id, self.attempt),
            task_id: st.task_id.clone(),
            session_id: st.session_id.clone(),
            attempt: self.attempt,
            status,
            criteria,
            checks,
            changed_files,
            limitations,
            started_at,
            finished_at: now(),
        });
        let stop = |status, reason: &str, report| GateDecision::Stop {
            status,
            reason: reason.into(),
            report: Some(report),
        };
        if cancel.is_cancelled() {
            return stop(TaskStatus::Cancelled, "cancelled", report);
        }
        let room = claimed_done && self.attempt < self.cfg.max_verify_attempts;
        match status {
            VerificationStatus::Passed => GateDecision::Complete(Some(report)),
            VerificationStatus::Blocked => {
                stop(TaskStatus::Blocked, "verification_blocked", report)
            }
            VerificationStatus::Failed if no_progress && claimed_done => {
                stop(TaskStatus::Failed, "no_progress", report)
            }
            VerificationStatus::Failed if room => {
                let feedback = self.feedback(&report);
                GateDecision::Continue { feedback, report }
            }
            VerificationStatus::Failed => stop(
                TaskStatus::Failed,
                if claimed_done {
                    "verify_budget_exhausted"
                } else {
                    "verification_failed"
                },
                report,
            ),
            VerificationStatus::Unverified if room && !self.unverified_round_used => {
                self.unverified_round_used = true;
                let feedback = self.feedback(&report);
                GateDecision::Continue { feedback, report }
            }
            VerificationStatus::Unverified => stop(TaskStatus::Unverified, "unverified", report),
        }
    }

    /// Structured user-role message for the model; never claims success.
    fn feedback(&self, r: &VerificationReport) -> String {
        let mut m = format!(
            "Verification attempt {} of {} is {}. The task is NOT verified as complete.\n",
            r.attempt,
            self.cfg.max_verify_attempts,
            r.status.as_str()
        );
        for k in r
            .checks
            .iter()
            .filter(|k| k.status == VerificationStatus::Failed)
        {
            m += &format!(
                "\nFAILED {} `{}` ({}):\n",
                k.id,
                k.command.as_deref().unwrap_or(""),
                if k.timed_out {
                    "timeout".into()
                } else {
                    format!("exit {:?}", k.exit_code)
                }
            );
            match k.artifacts.first() {
                Some(a) => {
                    m += &format!(
                        "{}\n(full log: artifact {})\n",
                        self.runner.read_tail(a, 4096),
                        a.artifact_id
                    )
                }
                None => m += &k.summary,
            }
        }
        m += "\nNot verified:\n";
        for c in r
            .criteria
            .iter()
            .filter(|c| c.status != VerificationStatus::Passed)
        {
            m += &format!("- criterion {} ({}): {}\n", c.id, c.text, c.status.as_str());
        }
        for l in &r.limitations {
            m += &format!("- {l}\n");
        }
        m += if r.status == VerificationStatus::Failed {
            "\nFix the cause of the failure, then finish again; checks will be re-run."
        } else {
            "\nMake the planned checks pass, or add a focused check for each unverified criterion (verify_run adds evidence but never replaces a planned check), then finish again."
        };
        m
    }
}
