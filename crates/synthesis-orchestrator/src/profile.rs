//! The launch profile of an attempt's worker (ADR-018 item 5, ADR-019 section 2).
//!
//! The worker is `synt run --unattended ... --resume <session> --attempt-session`: the existing
//! governed headless path, so the sandbox (enforce, or the run refuses to start), the network
//! (deny), the cost and wall caps, the inferred-checks rule and "no project trust" are the ones
//! `synt run --unattended` already keeps. This module only decides the arguments, from the stored
//! contract and the settings, and refuses to build anything weaker. It is pure: no process is
//! started here.

use std::ffi::OsString;
use std::path::PathBuf;

use synthesis_core::outcome::{Contract, Envelope};
use synthesis_session::outcome::OutcomePolicy;

use crate::{AttemptEnv, CompileError, CompiledEnvelope, compile_envelope};

/// Default wall cap of an attempt when neither the contract nor the settings name one.
pub const DEFAULT_WALL_SECS: u64 = 1800;

#[derive(Debug)]
pub enum ProfileError {
    /// No usable cost cap (the contract always names one; this guards a hand-built value).
    NoCostCap,
    /// The earlier attempts of the outcome already spent its whole cap.
    BudgetExhausted { spent: f64, cap: f64 },
    /// A flag outside the allow-list was offered for the worker.
    Forbidden(String),
    /// The envelope cannot be compiled into rules.
    Envelope(CompileError),
}

impl std::fmt::Display for ProfileError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoCostCap => write!(f, "an attempt needs a cost cap and none is usable"),
            Self::BudgetExhausted { spent, cap } => write!(
                f,
                "the outcome's cost cap is used up: {spent:.4} of {cap:.4} USD spent by earlier attempts"
            ),
            Self::Envelope(e) => write!(f, "the envelope cannot be compiled: {e}"),
            Self::Forbidden(a) => write!(
                f,
                "{a:?} is not allowed for an outcome attempt: sandbox, network, trust, MCP, hooks and permission mode come from the profile, never from the caller"
            ),
        }
    }
}
impl std::error::Error for ProfileError {}

/// What the caller may add: the model choice and the turn/tool-call bounds. Everything else that
/// shapes the worker's authority is fixed here.
const EXTRA_WITH_VALUE: [&str; 4] = ["--provider", "--model", "--max-turns", "--max-tool-calls"];

#[derive(Debug, Clone, PartialEq)]
pub struct Profile {
    program: PathBuf,
    cwd: PathBuf,
    session_id: String,
    prompt: String,
    max_cost_usd: f64,
    max_wall_secs: u64,
    envelope: Envelope,
    compiled: CompiledEnvelope,
    extra: Vec<String>,
}

impl Profile {
    /// The caps are the smallest of the contract's and the settings' (the per-attempt cap, and the
    /// outcome cap as an upper bound, so a cap always exists); the wall cap is the smaller of the
    /// two or 30 minutes. `acceptEdits` when the envelope allows writing, else `default`; never
    /// `auto` (no model is the authority) and never `bypassPermissions`.
    pub fn build(
        program: PathBuf,
        env: &AttemptEnv,
        contract: &Contract,
        policy: &OutcomePolicy,
        spent_usd: f64,
        prompt: String,
    ) -> Result<Self, ProfileError> {
        Self::build_shared(program, env, contract, policy, spent_usd, 1, prompt)
    }

    /// Like [`Profile::build`] for one of `share` workers that start together: each gets an equal
    /// part of what is left, so together they cannot spend more than the outcome's cap.
    pub fn build_shared(
        program: PathBuf,
        env: &AttemptEnv,
        contract: &Contract,
        policy: &OutcomePolicy,
        spent_usd: f64,
        share: u32,
        prompt: String,
    ) -> Result<Self, ProfileError> {
        let b = &contract.envelope.budgets;
        // earlier attempts already spent part of the outcome's cap; this one gets what is left
        let outcome_cap = [Some(b.max_cost_usd), policy.max_cost_usd]
            .into_iter()
            .flatten()
            .fold(f64::INFINITY, f64::min);
        // a spend that is not a number is not "nothing spent": the budget is treated as used up
        let spent = if spent_usd < 0.0 { 0.0 } else { spent_usd };
        let left = outcome_cap - spent;
        if !spent.is_finite() || left <= 0.0 {
            return Err(ProfileError::BudgetExhausted {
                spent,
                cap: outcome_cap,
            });
        }
        let cap = [
            Some(left / f64::from(share.max(1))),
            b.attempt_max_cost_usd,
            policy.attempt_max_cost_usd,
        ]
        .into_iter()
        .flatten()
        .fold(f64::INFINITY, f64::min);
        if !cap.is_finite() || cap <= 0.0 {
            return Err(ProfileError::NoCostCap);
        }
        let wall = [b.attempt_max_wall_secs, policy.attempt_max_wall_secs]
            .into_iter()
            .flatten()
            .min()
            .unwrap_or(DEFAULT_WALL_SECS);
        let compiled = compile_envelope(&contract.envelope, &[]).map_err(ProfileError::Envelope)?;
        Ok(Self {
            program,
            cwd: env.context.cwd.clone(),
            session_id: env.session_id.clone(),
            prompt,
            max_cost_usd: cap,
            max_wall_secs: wall,
            envelope: contract.envelope.clone(),
            compiled,
            extra: vec![],
        })
    }

    /// The commands that count as "the tests" when the envelope allows `tests.run` (from the
    /// user's settings; only known runners, see [`compile_envelope`]).
    pub fn with_test_commands(mut self, cmds: &[String]) -> Result<Self, ProfileError> {
        self.compiled = compile_envelope(&self.envelope, cmds).map_err(ProfileError::Envelope)?;
        Ok(self)
    }

    /// What the envelope compiled to.
    pub fn compiled(&self) -> &CompiledEnvelope {
        &self.compiled
    }

    /// Adds allow-listed options (`--provider X`, `--model X`, `--max-turns N`,
    /// `--max-tool-calls N`, each with its value). Anything else is refused.
    pub fn with_extra(mut self, args: &[&str]) -> Result<Self, ProfileError> {
        let mut it = args.iter();
        while let Some(a) = it.next() {
            let value = it.next();
            match value {
                Some(v) if EXTRA_WITH_VALUE.contains(a) && !v.starts_with('-') => {
                    self.extra.push((*a).to_string());
                    self.extra.push((*v).to_string());
                }
                _ => return Err(ProfileError::Forbidden((*a).to_string())),
            }
        }
        Ok(self)
    }

    pub fn program(&self) -> &PathBuf {
        &self.program
    }

    pub fn cwd(&self) -> &PathBuf {
        &self.cwd
    }

    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    pub fn max_wall_secs(&self) -> u64 {
        self.max_wall_secs
    }

    pub fn max_cost_usd(&self) -> f64 {
        self.max_cost_usd
    }

    pub fn permission_mode(&self) -> &'static str {
        CompiledEnvelope::MODE
    }

    /// The arguments after the program name. No `--sandbox`, `--network`, `--trust-project`,
    /// `--approve-mcp`, `--allow-host`, `--settings`, `--add-dir` or `--no-worktree`: the defaults
    /// of `--unattended` stand (sandbox enforce or refuse, network deny, no project trust).
    pub fn argv(&self) -> Vec<OsString> {
        let mut v: Vec<OsString> = vec!["run".into(), "--unattended".into()];
        v.extend(["--cwd".into(), self.cwd.clone().into()]);
        v.extend([
            "--resume".into(),
            self.session_id.clone().into(),
            "--attempt-session".into(),
        ]);
        v.extend([
            "--max-cost-usd".into(),
            format!("{}", self.max_cost_usd).into(),
        ]);
        v.extend([
            "--max-wall-secs".into(),
            self.max_wall_secs.to_string().into(),
        ]);
        v.extend(["--permission-mode".into(), CompiledEnvelope::MODE.into()]);
        v.extend(self.compiled.args());
        v.extend(["--output-format".into(), "json".into()]);
        v.extend(self.extra.iter().map(OsString::from));
        // `--prompt=<text>`: a prompt that starts with `-` is a value, not a flag
        v.push(format!("--prompt={}", self.prompt).into());
        v
    }
}
