//! The independent runner and the verifier that uses it (ADR-020 sections 3 and 5).
//!
//! After an attempt ends, the host looks at what it changed and runs the contract's frozen
//! checks, never in the attempt's worktree (the worker could still have a process there, or have
//! forged a report): a FRESH checkout of the attempt's commit is made through a scratch
//! repository (see `gitsafe`: no repository config, filters, refs or hooks), the checks run there
//! under the OS sandbox with the source read-only, a private temp directory, no network and no
//! state paths, and the results are written as `independent_check` evidence by this process alone
//! (the control-session lock holder), each row MAC'd under the chain key. The runner refuses to
//! produce evidence when the sandbox cannot confine. A check that fails to start, times out or is
//! stopped records NO row (an error is not evidence and is retried), and the item fails.
//!
//! "Independent" means: the repository's OWN checks exit 0 on a clean checkout of code the
//! attempt could write, under a sandbox. It does not mean the checks test the requirement:
//! `build.rs`, `conftest.py`, `.cargo/config.toml`, a Makefile, test configuration and toolchain
//! pins can make a check pass without testing anything. Where that can be seen (definition files,
//! weakened or removed tests, fewer tests than the base commit, zero tests) the evidence is
//! flagged, a flag fails the item and the case goes to a person.
//!
//! Two kinds of work item reach the verifier:
//! - an `agent` item: the changed files of its commit against the envelope (a write outside
//!   `allowed_paths`, a symlink or submodule link fails it), recorded as the `envelope-scope` check;
//! - a `verify` item: the contract's checks on the candidate commit, run on a background thread so
//!   the scheduler keeps ticking (heartbeats, cancel and the wall cap advance in `tick`).

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use sha2::{Digest, Sha256};
use synthesis_core::outcome::{
    AttemptState, Contract, ContractCheck, ContractKind, TrustClass, WorkItemId, WorkItemKind,
    hidden_char, outside_envelope, validate_contract,
};
use synthesis_sandbox::{
    Guarantee, NetworkPolicy, SandboxBackend, SandboxPolicy, SandboxedCommand,
};
use synthesis_session::outcome::{AuthenticEvidence, NewEvidence, OutcomeWriter, WorkItemRecord};

use crate::gitsafe::{ChangeReport, fresh_checkout_stoppable, inspect_change};
use crate::proc::{ChildState, StateFn, child_state, kill_unreaped_tree};
use crate::sched::{SchedError, Verdict, Verifier};

/// The check id of the changed-files test of an `agent` item.
pub const SCOPE_CHECK: &str = "envelope-scope";

/// Most output kept of one check (the tail), in bytes.
const OUTPUT_TAIL: usize = 64 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifyError(pub String);

impl std::fmt::Display for VerifyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "verification: {}", self.0)
    }
}
impl std::error::Error for VerifyError {}

fn sha_hex(b: &[u8]) -> String {
    Sha256::digest(b)
        .iter()
        .map(|x| format!("{x:02x}"))
        .collect()
}

fn state_err(e: impl std::fmt::Display) -> SchedError {
    SchedError::State(e.to_string())
}

// ---------------------------------------------------------------- running a check

/// What running one check produced.
#[derive(Debug, Clone)]
pub struct CheckRun {
    pub id: String,
    pub command_hash: String,
    /// `None`: it did not exit by itself (timed out, stopped, could not start).
    pub exit_code: Option<i32>,
    /// Why it has no exit code, when it has none.
    pub problem: Option<String>,
    /// The tail of its output.
    pub output: Vec<u8>,
}

impl CheckRun {
    /// `passed` only for exit 0; everything else, a check that never ran included, is not a pass.
    pub fn result(&self) -> &'static str {
        match (self.exit_code, &self.problem) {
            (Some(0), None) => "passed",
            (Some(_), None) => "failed",
            _ => "error",
        }
    }
}

/// Runs a check command in a directory. The real one is [`SandboxRunner`]; tests substitute.
pub trait CheckRunner: Send + Sync {
    /// Refuses (`Err`) when this host cannot confine the check; otherwise the environment
    /// fingerprint recorded with the evidence.
    fn fingerprint(&self) -> Result<String, VerifyError>;
    fn run(
        &self,
        dir: &Path,
        scratch: &Path,
        check: &ContractCheck,
        timeout: Duration,
        stop: &AtomicBool,
    ) -> CheckRun;
}

/// Runs the command with `sh -c` under the OS sandbox: the source is read-only (no writable root),
/// the private temp directory is the only place to write, the network is denied, Synthesis's own
/// state is inaccessible, and the environment is `PATH`, `LANG` and the temp locations only. The
/// guarantee is the backend's (`Weak` on macOS and for the bubblewrap filesystem): this is defence
/// in depth for code the attempt could write, not a boundary against a determined program.
pub struct SandboxRunner {
    pub home: PathBuf,
    pub backend: Box<dyn SandboxBackend>,
    /// Where the child stands (tests substitute it, like the attempt runner's).
    pub state: StateFn,
    /// Largest file a check may write, in MiB (`synt.orchestrator.verify.max_file_mb`, default
    /// 512). A larger one (a big debug test binary) stops the check with SIGXFSZ: it fails closed.
    pub max_file_mb: u64,
}

/// Default of [`SandboxRunner::max_file_mb`].
pub const DEFAULT_MAX_FILE_MB: u64 = 512;

impl SandboxRunner {
    pub fn detect(home: PathBuf) -> Self {
        Self {
            home,
            backend: synthesis_sandbox::detect(),
            state: child_state,
            max_file_mb: DEFAULT_MAX_FILE_MB,
        }
    }
}

impl CheckRunner for SandboxRunner {
    fn fingerprint(&self) -> Result<String, VerifyError> {
        let caps = self.backend.capabilities();
        let net = self.backend.network_guarantee(&NetworkPolicy::Deny);
        if cfg!(windows) || caps.filesystem < Guarantee::Weak || net < Guarantee::Weak {
            return Err(VerifyError(format!(
                "the {} sandbox cannot confine a check (filesystem {:?}, network {:?}): independent evidence needs an enforcing sandbox",
                self.backend.name(),
                caps.filesystem,
                net
            )));
        }
        Ok(format!(
            "{}/{} sandbox={} fs={:?} net={:?}",
            std::env::consts::OS,
            std::env::consts::ARCH,
            self.backend.name(),
            caps.filesystem,
            net
        ))
    }

    fn run(
        &self,
        dir: &Path,
        scratch: &Path,
        check: &ContractCheck,
        timeout: Duration,
        stop: &AtomicBool,
    ) -> CheckRun {
        let command_hash = sha_hex(check.command.as_bytes());
        let fail = |why: String| CheckRun {
            id: check.id.clone(),
            command_hash: command_hash.clone(),
            exit_code: None,
            problem: Some(why),
            output: vec![],
        };
        let tmp = scratch.join(format!("tmp-{}", check.id));
        if let Err(e) = std::fs::create_dir_all(&tmp) {
            return fail(format!("cannot create the private temp dir: {e}"));
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o700));
        }
        let mut policy = SandboxPolicy::standard(dir, &tmp);
        policy.writable_roots.clear(); // the checkout is read-only
        policy.readonly_paths.clear();
        policy.readable_roots.push(dir.to_path_buf());
        policy
            .deny_paths
            .extend(synthesis_sandbox::synthesis_state_paths(&self.home));
        policy.network = NetworkPolicy::Deny;
        policy.required = Guarantee::Weak;
        policy.env_passthrough = ["PATH", "LANG", "LC_ALL"].map(String::from).to_vec();
        // No single file a check writes can exceed the limit (this bounds one file, not the total
        // it writes in its temp dir, and it is the check's own process that is limited). The
        // sandbox has no registry cache and no network, so only a project whose dependencies are
        // vendored (or already built into its toolchain) can be verified.
        let command = format!(
            "ulimit -f {} 2>/dev/null; {}",
            self.max_file_mb.saturating_mul(2048),
            check.command
        );
        let wrapped = match self.backend.wrap(
            &policy,
            &SandboxedCommand {
                program: "sh".into(),
                args: vec!["-c".into(), command.into()],
                env: vec![
                    ("HOME".into(), tmp.clone().into_os_string()),
                    (
                        "CARGO_TARGET_DIR".into(),
                        tmp.join("target").into_os_string(),
                    ),
                    ("CI".into(), "1".into()),
                ],
                cwd: Some(dir.to_path_buf()),
                clear_env: true,
            },
        ) {
            Ok(w) => w,
            Err(e) => return fail(format!("the sandbox refused the check: {e}")),
        };
        let log_path = scratch.join(format!("{}.log", check.id));
        let log = match open_private(&log_path) {
            Ok(f) => f,
            Err(e) => return fail(format!("cannot open the output file: {e}")),
        };
        let mut c = Command::new(&wrapped.program);
        c.args(&wrapped.args);
        if wrapped.clear_env {
            c.env_clear();
        }
        c.envs(
            wrapped
                .env
                .iter()
                .map(|(k, v): &(OsString, OsString)| (k, v)),
        );
        if let Some(cwd) = &wrapped.cwd {
            c.current_dir(cwd);
        }
        c.stdin(Stdio::null());
        match (log.try_clone(), log.try_clone()) {
            (Ok(a), Ok(b)) => {
                c.stdout(a).stderr(b);
            }
            _ => return fail("cannot share the output file".into()),
        }
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            c.process_group(0);
        }
        let mut child = match c.spawn() {
            Ok(ch) => ch,
            Err(e) => return fail(format!("cannot start the check: {e}")),
        };
        let pid = child.id();
        let deadline = Instant::now() + timeout;
        let (code, problem) = loop {
            // The process group is killed BEFORE the leader is reaped, while its pid is still
            // ours; once the kernel reaped it behind our back the pid may be someone else's and
            // nothing is signalled.
            match (self.state)(pid) {
                ChildState::ExitedUnreaped => {
                    kill_unreaped_tree(pid);
                    break match child.wait().map(|s| s.code()) {
                        Ok(Some(c)) => (Some(c), None),
                        Ok(None) => (None, Some("the check was killed by a signal".to_owned())),
                        Err(e) => (None, Some(format!("cannot wait for the check: {e}"))),
                    };
                }
                ChildState::Gone => {
                    break (
                        None,
                        Some(
                            "the check was reaped behind our back: its exit status is lost"
                                .to_owned(),
                        ),
                    );
                }
                ChildState::Running => {}
            }
            #[cfg(not(unix))]
            if let Ok(Some(st)) = child.try_wait() {
                break (st.code(), None);
            }
            let why = if stop.load(Ordering::SeqCst) {
                Some("verification was stopped")
            } else if Instant::now() >= deadline {
                Some("the check timed out")
            } else {
                None
            };
            if let Some(why) = why {
                kill_unreaped_tree(pid);
                let _ = child.kill();
                let _ = child.wait();
                break (None, Some(why.to_owned()));
            }
            std::thread::sleep(Duration::from_millis(20));
        };
        // Whatever re-parented to init after the leader exited (a `setsid` daemon on macOS) is
        // out of reach by process group: the sandbox, not this, confines it.
        CheckRun {
            id: check.id.clone(),
            command_hash,
            exit_code: code,
            problem,
            output: tail_of(&log_path),
        }
    }
}

fn open_private(p: &Path) -> std::io::Result<std::fs::File> {
    let mut o = std::fs::OpenOptions::new();
    o.create(true).write(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        o.mode(0o600);
    }
    o.open(p)
}

fn tail_of(p: &Path) -> Vec<u8> {
    let mut buf = vec![];
    if let Ok(mut f) = std::fs::File::open(p) {
        let len = f.metadata().map_or(0, |m| m.len());
        if len > OUTPUT_TAIL as u64 {
            let _ = std::io::Seek::seek(&mut f, std::io::SeekFrom::Start(len - OUTPUT_TAIL as u64));
        }
        let _ = f.take(OUTPUT_TAIL as u64).read_to_end(&mut buf);
    }
    buf
}

/// Output as stored for display: no control, ANSI-escape or direction characters.
fn sanitize_output(b: &[u8]) -> Vec<u8> {
    String::from_utf8_lossy(b)
        .chars()
        .filter(|c| matches!(c, '\n' | '\t') || !hidden_char(*c))
        .collect::<String>()
        .into_bytes()
}

/// Total tests a runner's output says it ran. A HEURISTIC, not a control: it trusts any
/// `test result:` line in the output tail, which a `harness = false` binary can print (so can the
/// code under test). Its uses (zero tests, fewer than the base) raise flags for a person; none of
/// them proves a test ran. It reads the `test result: ok. N passed; M failed; ...` lines of
/// `cargo test`, summed over test binaries; `None` when the output is not of a known runner.
fn test_count(output: &[u8]) -> Option<u64> {
    let text = String::from_utf8_lossy(output);
    let mut total = None;
    for l in text
        .lines()
        .filter(|l| l.trim_start().starts_with("test result:"))
    {
        let mut n = 0u64;
        for part in l.split(';') {
            let part = part.trim();
            let digits: String = part
                .rsplit(' ')
                .nth(1)
                .map(str::to_owned)
                .unwrap_or_default();
            if (part.ends_with("passed") || part.ends_with("failed"))
                && let Ok(v) = digits.parse::<u64>()
            {
                n += v;
            }
        }
        total = Some(total.unwrap_or(0) + n);
    }
    total
}

// ---------------------------------------------------------------- the verifier

struct JobOutput {
    fingerprint: String,
    flags: Vec<&'static str>,
    runs: Vec<CheckRun>,
}

enum JobResult {
    /// The contract's checks that had no result yet.
    Checks(JobOutput),
    /// The changed files of an attempt's commit.
    Scope {
        report: ChangeReport,
        fingerprint: String,
    },
}

struct Job {
    commit: String,
    stop: Arc<AtomicBool>,
    handle: JoinHandle<Result<JobResult, VerifyError>>,
}

/// The scratch directory of a job is removed on every way out.
struct Cleanup(PathBuf);

impl Drop for Cleanup {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// What one evidence row says, before it is made a [`NewEvidence`].
struct EvRow<'a> {
    attempt: Option<(&'a synthesis_core::outcome::AttemptId, i64)>,
    commit: &'a str,
    check_id: &'a str,
    command_hash: Option<String>,
    result: &'a str,
    exit_code: Option<i32>,
    fingerprint: &'a str,
    artifact: Option<(String, String, String)>,
    flags: Vec<String>,
}

/// The [`Verifier`] for real outcomes: replaces `HandOff` once an outcome runs real attempts.
pub struct IndependentVerifier {
    /// The outcome's repository (the object database the commits live in).
    pub repo: PathBuf,
    /// `$SYNT_HOME`: scratch checkouts and logs live under `<home>/verify/` while a job runs.
    pub home: PathBuf,
    contract: Contract,
    runner: Arc<dyn CheckRunner>,
    /// Longest a single check may run.
    pub timeout: Duration,
    /// Masks session secrets (and secret patterns) in a report shown for review.
    pub redactor: synthesis_events::Redactor,
    jobs: BTreeMap<WorkItemId, Job>,
}

impl IndependentVerifier {
    /// For the outcome `w` owns: the contract is the APPROVED current one (its approval MAC must
    /// verify), re-validated; a verifier never runs checks of a contract nobody approved.
    pub fn for_outcome(
        w: &OutcomeWriter,
        repo: PathBuf,
        home: PathBuf,
        runner: Arc<dyn CheckRunner>,
        timeout: Duration,
    ) -> Result<Self, VerifyError> {
        let version = w
            .get_outcome(w.id())
            .map_err(|e| VerifyError(e.to_string()))?
            .contract_version;
        let (contract, _hash, approved) = w
            .get_contract(w.id(), version)
            .map_err(|e| VerifyError(e.to_string()))?;
        if !approved {
            return Err(VerifyError(format!(
                "contract v{version} is not approved: no check of it is run"
            )));
        }
        validate_contract(&contract).map_err(|e| VerifyError(e.to_string()))?;
        Ok(Self {
            repo,
            home,
            contract,
            runner,
            timeout,
            redactor: synthesis_events::Redactor::new([]),
            jobs: BTreeMap::new(),
        })
    }

    fn criteria_of(&self, check_id: &str) -> Vec<String> {
        self.contract
            .criteria
            .iter()
            .filter(|c| c.check_ids.iter().any(|i| i == check_id))
            .map(|c| c.id.clone())
            .collect()
    }

    fn evidence(&self, item: &WorkItemRecord, r: EvRow<'_>) -> NewEvidence {
        NewEvidence {
            work_item_id: item.id.clone(),
            attempt_id: r.attempt.map(|a| a.0.clone()),
            trust_class: TrustClass::IndependentCheck,
            producer: "independent_runner".into(),
            subject_commit: r.commit.into(),
            check_id: r.check_id.into(),
            command_hash: r.command_hash,
            criteria_ids: self.criteria_of(r.check_id),
            result: r.result.into(),
            exit_code: r.exit_code.map(i64::from),
            env_fingerprint: r.fingerprint.into(),
            artifact: r.artifact,
            fence: r.attempt.map_or(0, |a| a.1),
            flags: r.flags,
        }
    }

    /// A directory no other job (a stale one for the same item included) shares.
    fn scratch_dir(&self, name: &str) -> PathBuf {
        let nonce = uuid::Uuid::new_v4().simple().to_string();
        self.home
            .join("verify")
            .join(format!("{name}-{}", &nonce[..8]))
    }

    /// A job for another commit is stale: stop it and forget it.
    fn drop_stale(&mut self, item: &WorkItemId, commit: &str) {
        if self.jobs.get(item).is_some_and(|j| j.commit != commit)
            && let Some(old) = self.jobs.remove(item)
        {
            old.stop.store(true, Ordering::SeqCst);
        }
    }

    /// The rows for this item, check and commit that count (chained event and MAC).
    fn rows_for(
        &self,
        w: &OutcomeWriter,
        item: &WorkItemRecord,
        commit: &str,
        check: &str,
    ) -> Result<Vec<AuthenticEvidence>, SchedError> {
        Ok(w.authentic_evidence(w.id())?
            .into_iter()
            .filter(|e| {
                e.work_item_id == item.id
                    && e.subject_commit == commit
                    && e.check_id == check
                    && e.trust_class == TrustClass::IndependentCheck
            })
            .collect())
    }

    /// An `agent` item: its commit's changes against the envelope, inspected on a job thread (up to
    /// hundreds of git commands) so the tick never waits and cancel reaches it.
    fn verify_agent(
        &mut self,
        w: &mut OutcomeWriter,
        item: &WorkItemRecord,
    ) -> Result<Verdict, SchedError> {
        let Some(att) = w
            .list_attempts(&item.id)?
            .into_iter()
            .rfind(|a| a.state == AttemptState::Finished)
        else {
            return Ok(Verdict::Pending);
        };
        // A read-only goal's subject is a report, not a commit: no independent check can judge it.
        // Only a person's accepted `report_review` can (S1 section 2.3); until then the item waits.
        if self.contract.kind == ContractKind::ReadOnly {
            return crate::report::review_verdict(w, item, &att, &self.redactor);
        }
        let Some(commit) = att.subject_commit.clone() else {
            return Ok(Verdict::Failed("the attempt recorded no commit".into()));
        };
        self.drop_stale(&item.id, &commit);
        // already judged (a restart): decide from the authentic row of THIS attempt. A row of an
        // earlier attempt, even for the same commit (one carried over from a blocked attempt), does
        // not count: the commit is inspected again against the base commit.
        if let Some(row) = self
            .rows_for(w, item, &commit, SCOPE_CHECK)?
            .iter()
            .find(|e| e.fence_current)
        {
            return Ok(if row.result == "passed" && row.flags.is_empty() {
                Verdict::Passed
            } else {
                Verdict::Failed(format!(
                    "the attempt's changes failed the envelope check (flags: {})",
                    row.flags.join(", ")
                ))
            });
        }
        if !self.jobs.contains_key(&item.id) {
            let fingerprint = self.runner.fingerprint().map_err(state_err)?;
            let base = w.get_outcome(w.id())?.base_commit;
            let repo = self.repo.clone();
            let dir = self.scratch_dir(&format!("scope-{}", att.id.as_str()));
            let stop = Arc::new(AtomicBool::new(false));
            let (c2, s2) = (commit.clone(), stop.clone());
            let handle = std::thread::spawn(move || {
                let _clean = Cleanup(dir.clone());
                let report = inspect_change(&repo, dir, &base, &c2, &s2)?;
                Ok(JobResult::Scope {
                    report,
                    fingerprint,
                })
            });
            self.jobs.insert(
                item.id.clone(),
                Job {
                    commit,
                    stop,
                    handle,
                },
            );
            return Ok(Verdict::Pending);
        }
        if !self.jobs[&item.id].handle.is_finished() {
            return Ok(Verdict::Pending);
        }
        let done = self.jobs.remove(&item.id).expect("present");
        let (report, fp) = match done.handle.join() {
            Ok(Ok(JobResult::Scope {
                report,
                fingerprint,
            })) => (report, fingerprint),
            Ok(Ok(_)) => return Ok(Verdict::Failed("unexpected verification result".into())),
            Ok(Err(e)) => return Ok(Verdict::Failed(e.to_string())),
            Err(_) => return Ok(Verdict::Failed("the verification thread panicked".into())),
        };
        let mut outside = outside_envelope(&report.files, &self.contract.envelope);
        outside.extend(report.special.iter().cloned());
        outside.sort();
        outside.dedup();
        let mut flags: Vec<String> = report.flags.iter().map(|f| (*f).to_owned()).collect();
        if !outside.is_empty() {
            flags.push("outside_envelope".into());
        }
        let passed = outside.is_empty();
        w.record_evidence(&self.evidence(
            item,
            EvRow {
                attempt: Some((&att.id, att.fence)),
                commit: &commit,
                check_id: SCOPE_CHECK,
                command_hash: None,
                result: if passed { "passed" } else { "failed" },
                exit_code: None,
                fingerprint: &fp,
                artifact: None,
                flags,
            },
        ))?;
        if !passed {
            let shown: Vec<_> = outside.iter().take(10).cloned().collect();
            return Ok(Verdict::Failed(format!(
                "the attempt changed files outside its envelope (or added links): {}",
                crate::decision::clean(&shown.join(", "))
            )));
        }
        if !report.flags.is_empty() {
            // flagged evidence cannot make an item succeed: a change to definition files or tests
            // needs a person (a decision), not a quiet pass
            return Ok(Verdict::Failed(format!(
                "the attempt changed what the checks are made of ({}): definition files, file modes or weakened tests need review",
                report.flags.join(", ")
            )));
        }
        Ok(Verdict::Passed)
    }

    /// A `verify` item: the contract's checks on the candidate commit, on a background thread.
    /// Only the checks without an authentic row for this commit are run and recorded, so a retry
    /// after a partial failure never records a passing check twice.
    fn verify_checks(
        &mut self,
        w: &mut OutcomeWriter,
        item: &WorkItemRecord,
    ) -> Result<Verdict, SchedError> {
        let Some(commit) = w.candidate_commit()? else {
            return Ok(Verdict::Pending);
        };
        if self.contract.checks.is_empty() {
            return Ok(Verdict::Failed(
                "the contract has no checks to run independently".into(),
            ));
        }
        self.drop_stale(&item.id, &commit);
        if !self.jobs.contains_key(&item.id) {
            let mut todo = vec![];
            for c in &self.contract.checks {
                if self.rows_for(w, item, &commit, &c.id)?.is_empty() {
                    todo.push(c.clone());
                }
            }
            if todo.is_empty() {
                return self.decide(w, item, &commit, &[]);
            }
            let fp = self.runner.fingerprint().map_err(state_err)?;
            let (repo, base) = (self.repo.clone(), w.get_outcome(w.id())?.base_commit);
            let dir = self.scratch_dir(&format!(
                "{}-{}",
                item.id.as_str(),
                commit.get(..12).unwrap_or(&commit)
            ));
            let (runner, timeout) = (self.runner.clone(), self.timeout);
            let stop = Arc::new(AtomicBool::new(false));
            let (c2, s2) = (commit.clone(), stop.clone());
            let handle = std::thread::spawn(move || {
                job(&*runner, &repo, &dir, &base, &c2, &todo, timeout, &s2, fp)
                    .map(JobResult::Checks)
            });
            self.jobs.insert(
                item.id.clone(),
                Job {
                    commit,
                    stop,
                    handle,
                },
            );
            return Ok(Verdict::Pending);
        }
        if !self.jobs[&item.id].handle.is_finished() {
            return Ok(Verdict::Pending);
        }
        let done = self.jobs.remove(&item.id).expect("present");
        let out = match done.handle.join() {
            Ok(Ok(JobResult::Checks(o))) => o,
            Ok(Ok(_)) => return Ok(Verdict::Failed("unexpected verification result".into())),
            Ok(Err(e)) => return Ok(Verdict::Failed(e.to_string())),
            Err(_) => return Ok(Verdict::Failed("the verification thread panicked".into())),
        };
        // A run with no exit status (timed out, stopped, could not start, zero tests) is not
        // evidence: no row is written, so the same commit can be verified again; it is reported.
        let mut unrecorded = vec![];
        for run in &out.runs {
            if run.result() == "error" {
                unrecorded.push(format!(
                    "{}: {}",
                    run.id,
                    run.problem.as_deref().unwrap_or("no result")
                ));
                continue;
            }
            let artifact = if run.output.is_empty() {
                None
            } else {
                Some(w.put_check_output(&sanitize_output(&run.output))?)
            };
            w.record_evidence(
                &self.evidence(
                    item,
                    EvRow {
                        attempt: None,
                        commit: &done.commit,
                        check_id: &run.id,
                        // the hash of the command the CONTRACT froze, whatever the runner reports
                        command_hash: self
                            .contract
                            .checks
                            .iter()
                            .find(|c| c.id == run.id)
                            .map(|c| sha_hex(c.command.as_bytes())),
                        result: run.result(),
                        exit_code: run.exit_code,
                        fingerprint: &out.fingerprint,
                        artifact,
                        flags: out.flags.iter().map(|f| (*f).to_owned()).collect(),
                    },
                ),
            )?;
        }
        self.decide(w, item, &done.commit, &unrecorded)
    }

    /// The item passes when every contract check has unflagged `passed` AUTHENTIC independent
    /// evidence for the commit and no run went unrecorded.
    fn decide(
        &self,
        w: &mut OutcomeWriter,
        item: &WorkItemRecord,
        commit: &str,
        unrecorded: &[String],
    ) -> Result<Verdict, SchedError> {
        let mut bad: Vec<String> = unrecorded.to_vec();
        for c in &self.contract.checks {
            if unrecorded
                .iter()
                .any(|u| u.starts_with(&format!("{}:", c.id)))
            {
                continue;
            }
            let rows = self.rows_for(w, item, commit, &c.id)?;
            if rows.is_empty() {
                bad.push(format!("{} has no result", c.id));
            } else if let Some(r) = rows.iter().find(|r| !r.flags.is_empty()) {
                bad.push(format!("{} flagged ({})", c.id, r.flags.join(", ")));
            } else if let Some(r) = rows.iter().find(|r| r.result != "passed") {
                bad.push(format!("{} {}", c.id, r.result));
            }
        }
        Ok(if bad.is_empty() {
            Verdict::Passed
        } else {
            Verdict::Failed(format!("checks did not pass: {}", bad.join("; ")))
        })
    }
}

/// The background job of a `verify` item: checkout, inspection, the checks, and for runners whose
/// test count can be read, the same checks at the base commit to compare counts.
#[allow(clippy::too_many_arguments)]
fn job(
    runner: &dyn CheckRunner,
    repo: &Path,
    dir: &Path,
    base: &str,
    commit: &str,
    checks: &[ContractCheck],
    timeout: Duration,
    stop: &AtomicBool,
    fingerprint: String,
) -> Result<JobOutput, VerifyError> {
    let _clean = Cleanup(dir.to_path_buf());
    std::fs::create_dir_all(dir).map_err(|e| VerifyError(format!("{}: {e}", dir.display())))?;
    let src = dir.join("src");
    fresh_checkout_stoppable(repo, commit, &src, stop)?;
    let ChangeReport { flags, .. } =
        inspect_change(repo, dir.join("scratch-diff"), base, commit, stop)?;
    let mut flags = flags;
    let mut runs = vec![];
    for c in checks {
        if stop.load(Ordering::SeqCst) {
            return Err(VerifyError("verification was stopped".into()));
        }
        let mut run = runner.run(&src, &dir.join("run"), c, timeout, stop);
        // zero tests is not a pass: a check that ran nothing proves nothing
        if run.result() == "passed" && test_count(&run.output) == Some(0) {
            run.problem = Some("the check ran no tests".into());
        }
        runs.push(run);
    }
    // fewer tests than the base commit had is a weakening (a heuristic: see `test_count`)
    let counted: Vec<usize> = (0..runs.len())
        .filter(|i| runs[*i].result() == "passed" && test_count(&runs[*i].output).is_some())
        .collect();
    if !counted.is_empty() && base != commit {
        let base_src = dir.join("base");
        if fresh_checkout_stoppable(repo, base, &base_src, stop).is_ok() {
            for i in counted {
                if stop.load(Ordering::SeqCst) {
                    break;
                }
                // the base run has its own scratch (HOME, target dir): nothing is shared
                let at_base =
                    runner.run(&base_src, &dir.join("base-run"), &checks[i], timeout, stop);
                if at_base.result() == "passed"
                    && let (Some(b), Some(n)) =
                        (test_count(&at_base.output), test_count(&runs[i].output))
                    && n < b
                    && !flags.contains(&"tests_weakened")
                {
                    flags.push("tests_weakened");
                }
            }
        }
    }
    Ok(JobOutput {
        fingerprint,
        flags,
        runs,
    })
}

impl IndependentVerifier {
    fn stop_all(&self) {
        for j in self.jobs.values() {
            j.stop.store(true, Ordering::SeqCst);
        }
    }
}

impl Drop for IndependentVerifier {
    fn drop(&mut self) {
        // running checks are stopped (their process groups are killed by the runner) and waited
        // for, but not for ever
        self.stop_all();
        let until = Instant::now() + Duration::from_secs(5);
        for (_, j) in std::mem::take(&mut self.jobs) {
            while !j.handle.is_finished() && Instant::now() < until {
                std::thread::sleep(Duration::from_millis(10));
            }
            if j.handle.is_finished() {
                let _ = j.handle.join();
            }
        }
    }
}

impl Verifier for IndependentVerifier {
    fn busy(&self) -> bool {
        !self.jobs.is_empty()
    }

    fn cancel(&mut self) {
        self.stop_all();
    }

    fn verify(
        &mut self,
        w: &mut OutcomeWriter,
        item: &WorkItemRecord,
    ) -> Result<Verdict, SchedError> {
        match item.kind {
            WorkItemKind::Agent => self.verify_agent(w, item),
            WorkItemKind::Verify => self.verify_checks(w, item),
        }
    }
}
