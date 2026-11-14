//! The independent runner and the verifier that uses it (ADR-020 sections 3 and 5).
//!
//! After an attempt ends, the host looks at what it changed and runs the contract's frozen
//! checks, never in the attempt's worktree (the worker could still have a process there, or have
//! forged a report): a FRESH checkout of the attempt's commit is made from the object database
//! (`read-tree` + `checkout-index`: no `.git`, no hooks, no attributes-driven export filters, no
//! config), the checks run there under the OS sandbox with the source read-only, a private temp
//! directory, no network and no state paths, and the results are written as `independent_check`
//! evidence by this process alone (the control-session lock holder). The runner refuses to produce
//! evidence when the sandbox cannot confine, and a check that fails to start, times out or is
//! stopped is evidence of an `error`, never a pass.
//!
//! Two kinds of work item reach the verifier:
//! - an `agent` item: the changed files of its commit against the envelope (a write outside
//!   `allowed_paths`, which the permission rules cannot see in a shell redirect or a built program,
//!   fails it and the item retries), recorded as the `envelope-scope` check;
//! - a `verify` item: the contract's checks on the candidate commit, run on a background thread so
//!   the scheduler keeps ticking (heartbeats, cancel and the wall cap advance in `tick`).
//!
//! Changes to definition files or test weakening flag the evidence (`definitions_changed`,
//! `tests_weakened`): a flagged pass is not a pass for the criterion.

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
use synthesis_core::definitions::is_definition;
use synthesis_core::outcome::{
    AttemptState, Contract, ContractCheck, TrustClass, WorkItemId, WorkItemKind, outside_envelope,
};
use synthesis_sandbox::{
    Guarantee, NetworkPolicy, SandboxBackend, SandboxPolicy, SandboxedCommand,
};
use synthesis_session::outcome::{NewEvidence, OutcomeWriter, WorkItemRecord};
use synthesis_verification::{FileEdit, removed_prerequisite_guards, weakened_tests};

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

fn err<T>(m: impl Into<String>) -> Result<T, VerifyError> {
    Err(VerifyError(m.into()))
}

fn sha_hex(b: &[u8]) -> String {
    Sha256::digest(b)
        .iter()
        .map(|x| format!("{x:02x}"))
        .collect()
}

// ---------------------------------------------------------------- git, scrubbed

fn git(repo: &Path, index: Option<&Path>, args: &[&str]) -> Result<Vec<u8>, VerifyError> {
    let null = if cfg!(windows) { "NUL" } else { "/dev/null" };
    let mut c = Command::new("git");
    c.env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", null)
        .env("GIT_TERMINAL_PROMPT", "0")
        .args(["-c", "core.fsmonitor=false", "-c", "core.quotePath=false"])
        .arg("-C")
        .arg(repo)
        .args(args)
        .stdin(Stdio::null());
    if let Some(root) = std::env::var_os("SystemRoot") {
        c.env("SystemRoot", root);
    }
    if let Some(i) = index {
        c.env("GIT_INDEX_FILE", i);
    }
    let o = c.output().map_err(|e| VerifyError(format!("git: {e}")))?;
    if !o.status.success() {
        return err(format!(
            "git {}: {}",
            args.first().unwrap_or(&""),
            String::from_utf8_lossy(&o.stderr).trim()
        ));
    }
    Ok(o.stdout)
}

fn valid_commit(c: &str) -> bool {
    matches!(c.len(), 40 | 64) && c.bytes().all(|b| b.is_ascii_hexdigit())
}

/// A fresh copy of `commit`'s tree in `dest` (which is emptied first): straight from the object
/// database, so no `.git`, no hooks, no repository config and no `.gitattributes` export rules are
/// involved. The index file lives next to `dest`, outside it.
pub fn fresh_checkout(repo: &Path, commit: &str, dest: &Path) -> Result<(), VerifyError> {
    if !valid_commit(commit) {
        return err("not a commit id");
    }
    let _ = std::fs::remove_dir_all(dest);
    std::fs::create_dir_all(dest).map_err(|e| VerifyError(format!("{}: {e}", dest.display())))?;
    let index = dest.with_extension("index");
    let _ = std::fs::remove_file(&index);
    let result = (|| {
        git(repo, Some(&index), &["read-tree", commit])?;
        let mut prefix = dest.to_string_lossy().replace('\\', "/");
        if !prefix.ends_with('/') {
            prefix.push('/');
        }
        git(
            repo,
            Some(&index),
            &["checkout-index", "-a", "-f", &format!("--prefix={prefix}")],
        )
        .map(|_| ())
    })();
    let _ = std::fs::remove_file(&index);
    result
}

/// Files changed between `base` and `commit` (relative, `/` separators).
pub fn changed_files(repo: &Path, base: &str, commit: &str) -> Result<Vec<String>, VerifyError> {
    if !valid_commit(base) || !valid_commit(commit) {
        return err("not a commit id");
    }
    let out = git(
        repo,
        None,
        &[
            "diff",
            "--name-only",
            "-z",
            "--no-renames",
            "--no-ext-diff",
            "--no-textconv",
            base,
            commit,
        ],
    )?;
    Ok(out
        .split(|b| *b == 0)
        .filter(|s| !s.is_empty())
        .map(|s| String::from_utf8_lossy(s).into_owned())
        .collect())
}

/// The line diff of each changed file, for the weakening scan.
pub fn file_edits(repo: &Path, base: &str, commit: &str) -> Result<Vec<FileEdit>, VerifyError> {
    if !valid_commit(base) || !valid_commit(commit) {
        return err("not a commit id");
    }
    let out = git(
        repo,
        None,
        &[
            "diff",
            "-U0",
            "--no-renames",
            "--no-ext-diff",
            "--no-textconv",
            "--no-color",
            base,
            commit,
        ],
    )?;
    let mut edits: Vec<FileEdit> = vec![];
    for line in String::from_utf8_lossy(&out).lines() {
        if let Some(p) = line.strip_prefix("+++ b/") {
            edits.push(FileEdit {
                path: p.to_owned(),
                ..FileEdit::default()
            });
        } else if let Some(p) = line.strip_prefix("--- a/") {
            // a deleted file has no `+++ b/` path: its edit is opened by the `---` line
            if edits.last().is_none_or(|e| e.path != p) {
                edits.push(FileEdit {
                    path: p.to_owned(),
                    ..FileEdit::default()
                });
            }
        } else if let (Some(e), Some(l)) = (edits.last_mut(), line.strip_prefix('+')) {
            e.added.push(l.to_owned());
        } else if let (Some(e), Some(l)) = (edits.last_mut(), line.strip_prefix('-')) {
            e.removed.push(l.to_owned());
        }
    }
    Ok(edits)
}

/// The flags the changes of `base..commit` earn: definition files or weakened tests.
pub fn change_flags(
    repo: &Path,
    base: &str,
    commit: &str,
) -> Result<Vec<&'static str>, VerifyError> {
    let mut flags = vec![];
    let files = changed_files(repo, base, commit)?;
    if files.iter().any(|f| is_definition(f)) {
        flags.push("definitions_changed");
    }
    let edits = file_edits(repo, base, commit)?;
    if weakened_tests(&edits).is_some() || removed_prerequisite_guards(&edits).is_some() {
        flags.push("tests_weakened");
    }
    Ok(flags)
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
/// state is inaccessible, and the environment is `PATH`, `LANG` and the temp locations only.
pub struct SandboxRunner {
    pub home: PathBuf,
    pub backend: Box<dyn SandboxBackend>,
}

impl SandboxRunner {
    pub fn detect(home: PathBuf) -> Self {
        Self {
            home,
            backend: synthesis_sandbox::detect(),
        }
    }
}

impl CheckRunner for SandboxRunner {
    fn fingerprint(&self) -> Result<String, VerifyError> {
        let caps = self.backend.capabilities();
        let net = self.backend.network_guarantee(&NetworkPolicy::Deny);
        if cfg!(windows) || caps.filesystem < Guarantee::Weak || net < Guarantee::Weak {
            return err(format!(
                "the {} sandbox cannot confine a check (filesystem {:?}, network {:?}): independent evidence needs an enforcing sandbox",
                self.backend.name(),
                caps.filesystem,
                net
            ));
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
        let wrapped = match self.backend.wrap(
            &policy,
            &SandboxedCommand {
                program: "sh".into(),
                args: vec!["-c".into(), check.command.clone().into()],
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
            match child.try_wait() {
                Ok(Some(st)) => {
                    break match st.code() {
                        Some(c) => (Some(c), None),
                        None => (None, Some("the check was killed by a signal".to_owned())),
                    };
                }
                Ok(None) => {}
                Err(e) => break (None, Some(format!("cannot wait for the check: {e}"))),
            }
            let why = if stop.load(Ordering::SeqCst) {
                Some("verification was stopped")
            } else if Instant::now() >= deadline {
                Some("the check timed out")
            } else {
                None
            };
            if let Some(why) = why {
                crate::proc::kill_unreaped_tree(pid);
                let _ = child.kill();
                let _ = child.wait();
                break (None, Some(why.to_owned()));
            }
            std::thread::sleep(Duration::from_millis(20));
        };
        // whatever the check left in its process group goes with it
        crate::proc::kill_unreaped_tree(pid);
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

// ---------------------------------------------------------------- the verifier

struct JobOutput {
    fingerprint: String,
    flags: Vec<&'static str>,
    runs: Vec<CheckRun>,
}

struct Job {
    commit: String,
    handle: JoinHandle<Result<JobOutput, VerifyError>>,
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
}

/// The [`Verifier`] for real outcomes: replaces `HandOff` once an outcome runs real attempts.
pub struct IndependentVerifier {
    /// The outcome's repository (the object database the commits live in).
    pub repo: PathBuf,
    /// `$SYNT_HOME`: fresh checkouts and logs live under `<home>/verify/`.
    pub home: PathBuf,
    pub contract: Contract,
    pub runner: Arc<dyn CheckRunner>,
    /// Longest a single check may run.
    pub timeout: Duration,
    stop: Arc<AtomicBool>,
    jobs: BTreeMap<WorkItemId, Job>,
}

impl IndependentVerifier {
    pub fn new(
        repo: PathBuf,
        home: PathBuf,
        contract: Contract,
        runner: Arc<dyn CheckRunner>,
        timeout: Duration,
    ) -> Self {
        Self {
            repo,
            home,
            contract,
            runner,
            timeout,
            stop: Arc::new(AtomicBool::new(false)),
            jobs: BTreeMap::new(),
        }
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
        }
    }

    /// An `agent` item: its commit's changes against the envelope.
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
        let Some(commit) = att.subject_commit.clone() else {
            return Ok(Verdict::Failed("the attempt recorded no commit".into()));
        };
        let base = w.get_outcome(w.id())?.base_commit;
        let fp = self
            .runner
            .fingerprint()
            .map_err(|e| SchedError::State(e.to_string()))?;
        let files = changed_files(&self.repo, &base, &commit)
            .map_err(|e| SchedError::State(e.to_string()))?;
        let outside = outside_envelope(&files, &self.contract.envelope);
        let result = if outside.is_empty() {
            "passed"
        } else {
            "failed"
        };
        let (id, _) = w.record_evidence(&self.evidence(
            item,
            EvRow {
                attempt: Some((&att.id, att.fence)),
                commit: &commit,
                check_id: SCOPE_CHECK,
                command_hash: None,
                result,
                exit_code: None,
                fingerprint: &fp,
                artifact: None,
            },
        ))?;
        if !outside.is_empty() {
            w.flag_evidence(&id, "outside_envelope")?;
            let shown: Vec<_> = outside.iter().take(10).cloned().collect();
            return Ok(Verdict::Failed(format!(
                "the attempt changed files outside its envelope: {}",
                crate::decision::clean(&shown.join(", "))
            )));
        }
        let flags = change_flags(&self.repo, &base, &commit)
            .map_err(|e| SchedError::State(e.to_string()))?;
        for f in &flags {
            w.flag_evidence(&id, f)?;
        }
        if !flags.is_empty() {
            // flagged evidence cannot make an item succeed: the change to definition files or
            // tests needs a person (a decision), not a quiet pass
            return Ok(Verdict::Failed(format!(
                "the attempt changed what the checks are made of ({}): definition files or weakened tests need review",
                flags.join(", ")
            )));
        }
        Ok(Verdict::Passed)
    }

    /// A `verify` item: the contract's checks on the candidate commit, on a background thread.
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
        // a job for another commit is stale: forget it (its thread ends by itself)
        if self.jobs.get(&item.id).is_some_and(|j| j.commit != commit) {
            self.jobs.remove(&item.id);
        }
        if !self.jobs.contains_key(&item.id) {
            let existing: Vec<String> = w
                .list_evidence(w.id())?
                .into_iter()
                .filter(|e| e.work_item_id == item.id && e.subject_commit == commit)
                .map(|e| e.check_id)
                .collect();
            let all_done = self
                .contract
                .checks
                .iter()
                .all(|c| existing.contains(&c.id));
            if all_done {
                return self.decide(w, item, &commit);
            }
            let fp = self
                .runner
                .fingerprint()
                .map_err(|e| SchedError::State(e.to_string()))?;
            let (repo, base) = (self.repo.clone(), w.get_outcome(w.id())?.base_commit);
            let scratch = self.home.join("verify").join(format!(
                "{}-{}",
                item.id.as_str(),
                commit.get(..12).unwrap_or(&commit)
            ));
            let (runner, checks, timeout, stop) = (
                self.runner.clone(),
                self.contract.checks.clone(),
                self.timeout,
                self.stop.clone(),
            );
            let c2 = commit.clone();
            let handle = std::thread::spawn(move || {
                let src = scratch.join("src");
                std::fs::create_dir_all(&scratch)
                    .map_err(|e| VerifyError(format!("{}: {e}", scratch.display())))?;
                fresh_checkout(&repo, &c2, &src)?;
                let flags = change_flags(&repo, &base, &c2)?;
                let mut runs = vec![];
                for c in &checks {
                    runs.push(runner.run(&src, &scratch, c, timeout, &stop));
                }
                let _ = std::fs::remove_dir_all(&src);
                Ok(JobOutput {
                    fingerprint: fp,
                    flags,
                    runs,
                })
            });
            self.jobs.insert(item.id.clone(), Job { commit, handle });
            return Ok(Verdict::Pending);
        }
        if !self.jobs[&item.id].handle.is_finished() {
            return Ok(Verdict::Pending);
        }
        let job = self.jobs.remove(&item.id).expect("present");
        let out = match job.handle.join() {
            Ok(Ok(o)) => o,
            Ok(Err(e)) => return Ok(Verdict::Failed(e.to_string())),
            Err(_) => return Ok(Verdict::Failed("the verification thread panicked".into())),
        };
        for run in &out.runs {
            let artifact = if run.output.is_empty() {
                None
            } else {
                Some(w.put_check_output(&run.output)?)
            };
            let (id, _) = w.record_evidence(&self.evidence(
                item,
                EvRow {
                    attempt: None,
                    commit: &job.commit,
                    check_id: &run.id,
                    command_hash: Some(run.command_hash.clone()),
                    result: run.result(),
                    exit_code: run.exit_code,
                    fingerprint: &out.fingerprint,
                    artifact,
                },
            ))?;
            for f in &out.flags {
                w.flag_evidence(&id, f)?;
            }
        }
        self.decide(w, item, &job.commit)
    }

    /// The item passes when every contract check has `passed` evidence for the commit.
    fn decide(
        &self,
        w: &mut OutcomeWriter,
        item: &WorkItemRecord,
        commit: &str,
    ) -> Result<Verdict, SchedError> {
        let rows = w.list_evidence(w.id())?;
        let mut bad = vec![];
        for c in &self.contract.checks {
            let row = rows.iter().find(|e| {
                e.work_item_id == item.id && e.subject_commit == commit && e.check_id == c.id
            });
            match row.map(|e| e.result.as_str()) {
                Some("passed") => {}
                Some(r) => bad.push(format!("{} {r}", c.id)),
                None => bad.push(format!("{} has no result", c.id)),
            }
        }
        Ok(if bad.is_empty() {
            Verdict::Passed
        } else {
            Verdict::Failed(format!("checks did not pass: {}", bad.join(", ")))
        })
    }
}

impl Drop for IndependentVerifier {
    fn drop(&mut self) {
        // a check still running is stopped (its process group is killed by the runner)
        self.stop.store(true, Ordering::SeqCst);
    }
}

impl Verifier for IndependentVerifier {
    fn busy(&self) -> bool {
        !self.jobs.is_empty()
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
