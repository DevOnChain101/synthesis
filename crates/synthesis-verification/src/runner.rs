//! Executes checks through `synthesis_terminal::exec` and classifies the outcome
//! (verification.md §2: exit code first, parsing may only downgrade passed -> unverified).

use crate::types::*;
use regex::Regex;
use sha2::{Digest, Sha256};
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, LazyLock};
use std::time::{Duration, Instant};
use synthesis_core::WorkspaceContext;
use synthesis_events::ArtifactRef;
use synthesis_terminal::{ExecSpec, TerminalRuntime};
use tokio_util::sync::CancellationToken;

const SUMMARY_MAX: usize = 2048;

fn re(p: &str) -> Regex {
    Regex::new(p).expect("static regex")
}

static MISSING_TOOL: LazyLock<Regex> = LazyLock::new(|| {
    re(
        r"(?im)command not found|is not recognized as an internal or external command|no such command:|No module named '?(pytest|tox|mypy|ruff|flake8)\b",
    )
});
static MISSING_ENV: LazyLock<Regex> = LazyLock::new(|| {
    re(
        r"(?i)(environment variable|env var|credentials|api[_ -]?key|auth(entication)? token)\b.{0,40}\b(missing|not set|unset|required|not found)|\b(missing|unset)\b.{0,20}(environment variable|credentials|api[_ -]?key)",
    )
});
static PORT_BUSY: LazyLock<Regex> =
    LazyLock::new(|| re(r"(?i)EADDRINUSE|address already in use|port \d+ is (already )?in use"));
static ZERO_TESTS: LazyLock<Regex> = LazyLock::new(|| {
    re(r"(?m)^# tests 0\s*$|Ran 0 tests|no tests ran|No tests found|collected 0 items")
});
static CARGO_RESULT: LazyLock<Regex> = LazyLock::new(|| re(r"test result: \w+\. (\d+) passed"));
static FAIL_LINE: LazyLock<Regex> = LazyLock::new(|| re(r"(?i)fail|error|panick|assert|not ok"));
static DIGITS: LazyLock<Regex> = LazyLock::new(|| re(r"\d+"));

/// `Some(reason)` when the failure is the environment's, not the code's.
fn environment_problem(out: &str) -> Option<String> {
    let (what, r) = [
        ("tool missing", &*MISSING_TOOL),
        ("credentials or env var missing", &*MISSING_ENV),
        ("port unavailable", &*PORT_BUSY),
    ]
    .into_iter()
    .find(|(_, r)| r.is_match(out))?;
    let line = r.find(out).map_or("", |m| {
        let s = out[..m.start()].rfind('\n').map_or(0, |i| i + 1);
        let e = out[m.end()..].find('\n').map_or(out.len(), |i| m.end() + i);
        out[s..e].trim()
    });
    Some(format!("{what}: {line}"))
}

/// The program the shell said it could not find (exit 127), named from its own message, not guessed from the
/// command line (`cd sub && cargo test`, `sh -c ...`). `None`: other exit code, or no recognisable message.
fn program_missing(exit: Option<i32>, out: &str) -> Option<String> {
    static NAME: LazyLock<Regex> = LazyLock::new(|| {
        re(
            r"(?m)(?:([^\s:']+): (?:command )?not found\s*$|command not found: (\S+)|^'([^']+)' is not recognized as an internal or external command)",
        )
    });
    if exit != Some(127) {
        return None;
    }
    let c = NAME.captures(out)?;
    let prog = c.iter().skip(1).flatten().next()?.as_str();
    (prog.len() <= 64 && !prog.chars().any(char::is_control))
        .then(|| format!("{MISSING_PROGRAM}: {prog}"))
}

/// Runner summaries that show nothing was tested (cargo: every "test result" has 0 passed).
fn ran_nothing(out: &str) -> bool {
    if ZERO_TESTS.is_match(out) {
        return true;
    }
    let mut counts = CARGO_RESULT
        .captures_iter(out)
        .map(|c| c[1].parse::<u64>().unwrap_or(1));
    counts
        .next()
        .is_some_and(|first| first == 0 && counts.all(|n| n == 0))
}

pub(crate) fn fingerprint(out: &str, cwd: &str) -> String {
    let norm = |l: &str| {
        DIGITS
            .replace_all(&l.replace(cwd, "<cwd>"), "N")
            .trim()
            .to_string()
    };
    let lines: Vec<&str> = out.lines().filter(|l| !l.trim().is_empty()).collect();
    let mut picked: Vec<String> = lines
        .iter()
        .filter(|l| FAIL_LINE.is_match(l))
        .take(5)
        .map(|l| norm(l))
        .collect();
    if picked.is_empty() {
        picked = lines.iter().rev().take(5).map(|l| norm(l)).collect();
    }
    format!("{:x}", Sha256::digest(picked.join("\n").as_bytes()))
}

fn clip(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.into();
    }
    let mut i = s.len() - max;
    while !s.is_char_boundary(i) {
        i += 1;
    }
    format!("...{}", &s[i..])
}

/// Answer of a [`CheckAuthorizer`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CheckAuth {
    Allow,
    /// Do not run the check; the reason becomes its `blocked_reason`. By convention
    /// `needs_approval:<rule>` = nobody could approve (the rule would allow it) and
    /// `denied by rule ...` = policy forbids it.
    Blocked(String),
}

/// Policy for the commands the gate runs itself. The harness implements it by judging each
/// check like a `Bash` call, so verification cannot run what the agent itself could not.
pub trait CheckAuthorizer: Send + Sync {
    /// Called once with the whole set before the first `authorize`, so an implementation can
    /// ask the user one question for all checks instead of one per check.
    fn prepare<'a>(
        &'a self,
        _checks: &'a [Check],
        _cancel: &'a CancellationToken,
    ) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>> {
        Box::pin(async {})
    }

    fn authorize<'a>(
        &'a self,
        check: &'a Check,
        cancel: &'a CancellationToken,
    ) -> Pin<Box<dyn Future<Output = CheckAuth> + Send + 'a>>;

    /// Progress callbacks of `run_with`: `index` of `total` is about to be authorised and run.
    fn started(&self, _check: &Check, _index: usize, _total: usize) {}

    /// The check ended (any status, a refused one included).
    fn finished(&self, _result: &CheckResult) {}
}

/// Prefix of a [`CheckAuth::Blocked`] reason that needs an approval nobody could give.
pub const NEEDS_APPROVAL: &str = "needs_approval:";

pub struct VerifyRunner {
    rt: Arc<TerminalRuntime>,
    session_id: String,
    ws: Arc<WorkspaceContext>,
}

impl VerifyRunner {
    /// `session_id` must already be open on `rt` with `ws` (that is the sandbox).
    pub fn new(rt: Arc<TerminalRuntime>, session_id: &str, ws: Arc<WorkspaceContext>) -> Self {
        Self {
            rt,
            session_id: session_id.into(),
            ws,
        }
    }

    pub fn workspace(&self) -> &WorkspaceContext {
        &self.ws
    }

    /// Runs checks in order; stops starting new ones once `cancel` fires (the cancelled check
    /// is reported `blocked`, the rest are not reported).
    pub async fn run(&self, checks: &[Check], cancel: &CancellationToken) -> Vec<CheckResult> {
        self.run_with(checks, cancel, None).await
    }

    /// [`run`](Self::run) where every command first goes through `auth`.
    pub async fn run_with(
        &self,
        checks: &[Check],
        cancel: &CancellationToken,
        auth: Option<&dyn CheckAuthorizer>,
    ) -> Vec<CheckResult> {
        let mut out = vec![];
        if let Some(a) = auth {
            a.prepare(checks, cancel).await;
        }
        for (i, c) in checks.iter().enumerate() {
            if cancel.is_cancelled() {
                break;
            }
            if let Some(a) = auth {
                a.started(c, i, checks.len());
            }
            let r = self.run_one_with(c, cancel, auth).await;
            if let Some(a) = auth {
                a.finished(&r);
            }
            out.push(r);
        }
        out
    }

    pub async fn run_one(&self, c: &Check, cancel: &CancellationToken) -> CheckResult {
        self.run_one_with(c, cancel, None).await
    }

    pub async fn run_one_with(
        &self,
        c: &Check,
        cancel: &CancellationToken,
        auth: Option<&dyn CheckAuthorizer>,
    ) -> CheckResult {
        let started_at = now();
        let t0 = Instant::now();
        let mut r = CheckResult {
            id: c.id.clone(),
            kind: c.kind,
            command: c.command.clone(),
            cwd: c.cwd.clone(),
            declared_by: c.declared_by,
            started_at,
            finished_at: String::new(),
            duration_ms: 0,
            exit_code: None,
            timed_out: false,
            status: VerificationStatus::Unverified,
            blocked_reason: None,
            summary: String::new(),
            failure_fingerprint: None,
            criteria_ids: c.criteria_ids.clone(),
            artifacts: vec![],
            tests: None,
        };
        // (no `if let` match guard: unstable on the 1.89 MSRV)
        let blocked = match (&c.command, auth) {
            (Some(_), Some(a)) => match a.authorize(c, cancel).await {
                CheckAuth::Blocked(why) => Some(why),
                _ => None,
            },
            _ => None,
        };
        match (&c.command, blocked) {
            (None, _) => {
                r.summary = "no executable command: needs a browser, HTTP or manual check".into()
            }
            (Some(_), Some(why)) => {
                r.status = VerificationStatus::Blocked;
                r.summary = format!("not run: {why}");
                r.blocked_reason = Some(why);
            }
            (Some(cmd), None) => {
                let mut spec = ExecSpec::new(cmd.clone());
                spec.cwd = Some(c.cwd.clone());
                spec.timeout = Duration::from_millis(c.timeout_ms);
                spec.max_output_bytes = 64 * 1024;
                match self.rt.exec(&self.session_id, spec, cancel).await {
                    Err(e) => {
                        // sandbox refusal (cwd outside workspace), unknown session, spawn failure
                        r.status = VerificationStatus::Blocked;
                        r.blocked_reason = Some(format!("could not run: {e}"));
                        r.summary = r.blocked_reason.clone().unwrap_or_default();
                    }
                    Ok(x) => {
                        r.exit_code = x.exit_code;
                        r.timed_out = x.timed_out;
                        self.classify(&mut r, &x);
                    }
                }
            }
        }
        r.duration_ms = u64::try_from(t0.elapsed().as_millis()).unwrap_or(u64::MAX);
        r.finished_at = now();
        r
    }

    fn classify(&self, r: &mut CheckResult, x: &synthesis_terminal::ExecResult) {
        use VerificationStatus::*;
        let head = if x.timed_out {
            format!("timed out after {} ms", r.duration_ms.max(1))
        } else {
            format!(
                "exit {}",
                x.exit_code.map_or("none".into(), |c| c.to_string())
            )
        };
        r.summary = clip(&format!("{head}\n{}", x.output.trim_end()), SUMMARY_MAX);
        r.artifacts = self.artifact(&x.log_path).into_iter().collect();
        r.status = if x.cancelled {
            r.blocked_reason = Some("cancelled".into());
            Blocked
        } else if x.timed_out || x.output_limit_exceeded {
            Failed
        } else if x.exit_code == Some(0) {
            r.tests = crate::testout::parse_counts(&x.output);
            // `skipped` is judged by the gate (a baseline may excuse it); zero executed is not.
            let none_ran = r.tests.as_ref().is_some_and(|t| t.passed + t.failed == 0);
            if ran_nothing(&x.output) || none_ran {
                r.blocked_reason = None;
                r.summary = format!(
                    "exit 0 but no tests ran\n{}",
                    clip(x.output.trim_end(), SUMMARY_MAX - 40)
                );
                Unverified
            } else {
                Passed
            }
        } else if let Some(why) = program_missing(x.exit_code, &x.output)
            .or_else(|| {
                (x.exit_code == Some(127)).then(|| {
                    environment_problem(&x.output)
                        .unwrap_or_else(|| "tool missing: exit 127".into())
                })
            })
            .or_else(|| environment_problem(&x.output))
        {
            r.blocked_reason = Some(why);
            Blocked
        } else {
            Failed
        };
        if r.status == Failed {
            r.failure_fingerprint = Some(fingerprint(&x.output, &self.ws.cwd.to_string_lossy()));
        }
    }

    fn artifact(&self, log: &std::path::Path) -> Option<ArtifactRef> {
        let bytes = std::fs::read(log).ok()?;
        let rel = log.strip_prefix(&self.ws.artifact_root).ok()?;
        Some(ArtifactRef {
            artifact_id: log.file_stem()?.to_string_lossy().into_owned(),
            path: rel.to_string_lossy().replace('\\', "/"),
            size: bytes.len() as u64,
            sha256: format!("{:x}", Sha256::digest(&bytes)),
            mime: "text/plain".into(),
        })
    }

    /// Last `n` bytes of an artifact's log (for self-correction feedback).
    pub fn read_tail(&self, a: &ArtifactRef, n: usize) -> String {
        let bytes = std::fs::read(self.ws.artifact_root.join(&a.path)).unwrap_or_default();
        clip(String::from_utf8_lossy(&bytes).trim_end(), n)
    }
}

#[cfg(test)]
mod unit {
    use super::*;

    #[test]
    fn classification_helpers() {
        assert!(
            environment_problem("sh: 1: pytest: command not found")
                .unwrap()
                .starts_with("tool missing")
        );
        assert!(
            environment_problem("Error: listen EADDRINUSE :::3000")
                .unwrap()
                .starts_with("port")
        );
        assert!(
            environment_problem("STRIPE_KEY environment variable is not set")
                .unwrap()
                .starts_with("credentials")
        );
        assert!(environment_problem("assertion failed: 1 == 2").is_none());
        let pm = |e, o| program_missing(e, o).unwrap_or_default();
        for out in [
            "sh: cargo: command not found",
            "sh: 1: cargo: not found",
            "zsh: command not found: cargo",
            "bash: line 1: cargo: command not found",
            "'cargo' is not recognized as an internal or external command,",
        ] {
            assert_eq!(
                pm(Some(127), out),
                format!("{MISSING_PROGRAM}: cargo"),
                "{out}"
            );
        }
        // `cd sub && cargo test` and `sh -c ...`: the message names the program, not the command's first word
        assert!(pm(Some(127), "sh: cargo: command not found").ends_with("cargo"));
        // a real failure that merely prints the words is not a missing program
        assert!(program_missing(Some(1), "sh: cargo: command not found").is_none());
        assert!(program_missing(Some(127), "something else failed").is_none());
        assert!(ran_nothing(
            "running 0 tests\ntest result: ok. 0 passed; 0 failed"
        ));
        assert!(!ran_nothing(
            "test result: ok. 0 passed;\ntest result: ok. 3 passed;"
        ));
        assert!(ran_nothing("Ran 0 tests in 0.000s"));
        assert_eq!(
            fingerprint("FAIL at line 12 in 0.4s", "/x"),
            fingerprint("FAIL at line 99 in 1.7s", "/x")
        );
        assert_ne!(fingerprint("FAIL a", "/x"), fingerprint("FAIL b", "/x"));
    }
}
