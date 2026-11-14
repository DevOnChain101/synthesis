//! The authority envelope compiled into the existing permission engine (ADR-019 sections 1 and 3).
//!
//! The output is a list of `--allow` / `--deny` rules (settings rule syntax, CLI layer: a managed
//! deny still wins and deny beats every allow here), a network choice for the OS sandbox and the
//! permission mode. The mode is always `default`: with `acceptEdits` every edit inside the
//! worktree would be allowed whatever `allowed_paths` says, so writes are allowed by explicit
//! `Edit(/<path>)` rules instead and anything else asks, which in an attempt (nobody to ask)
//! becomes a recorded Decision. Unknown and unlisted capabilities are denied. An allow for an
//! irreversible capability (push, secrets, deploy) is refused by `validate_envelope`, and no rule
//! this module emits can lift the engine's own irreversible table.
//!
//! Path patterns are worktree-relative globs of `/`-separated, non-empty segments (`*` within a
//! segment, `**` across segments; no `.`, `..`, empty segments or leading `/`). `allowed_paths`
//! `src` allows only a file named `src`; write `src/**` for a directory. A `denied_paths` entry
//! that does not end in `**` denies the path and everything under it.
//!
//! Residuals, stated: `repo.read: deny` leaves read-only shell commands (`cat`, `ls`) that the
//! engine auto-allows; reads of a denied path through an interpreter (`python -c`, `node -e`)
//! are not parsed; `secrets.read: deny` covers five path patterns, not every secret store. The OS
//! sandbox and the path rules together are the boundary, not the rules alone.

use std::collections::BTreeSet;

use sha2::{Digest, Sha256};
use synthesis_core::outcome::{
    Capability, CapabilityMode, Envelope, EnvelopeError, validate_envelope,
};

/// Network for the attempt's commands (the OS sandbox decides): `deny`, or `hosts`, which reaches
/// only the hosts the user's or managed settings list (never the project, never the caller).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Network {
    Deny,
    Hosts,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CompileError {
    Envelope(EnvelopeError),
    /// A path pattern the rule syntax cannot carry safely.
    PathPattern(String),
    /// A test command outside the known runners, or with shell syntax.
    TestCommand(String),
}

impl std::fmt::Display for CompileError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Envelope(e) => write!(f, "{e}"),
            Self::PathPattern(p) => write!(f, "path pattern {p:?} cannot be used in a rule"),
            Self::TestCommand(c) => write!(
                f,
                "test command {c:?} is not a plain invocation of a known test runner"
            ),
        }
    }
}
impl std::error::Error for CompileError {}

impl From<EnvelopeError> for CompileError {
    fn from(e: EnvelopeError) -> Self {
        Self::Envelope(e)
    }
}

/// Commands the user's settings may name as "the tests" (`tests.run: allow`). Anything else, or
/// any shell syntax, is refused: such a rule could lift the engine's irreversible table.
const TEST_RUNNERS: [&str; 14] = [
    "cargo test",
    "cargo check",
    "cargo clippy",
    "cargo fmt",
    "cargo build",
    "npm test",
    "npm run test",
    "npx jest",
    "node --test",
    "pytest",
    "python -m pytest",
    "go test",
    "make test",
    "dotnet test",
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompiledEnvelope {
    /// Sorted, deduplicated.
    pub allow: Vec<String>,
    pub deny: Vec<String>,
    pub network: Network,
}

impl CompiledEnvelope {
    /// Permission mode of every attempt (see the module comment).
    pub const MODE: &'static str = "default";

    /// Hash of what the envelope compiled to (not of the user's test commands): the decision
    /// binding names it so a decision made under another envelope is stale.
    pub fn policy_hash(&self) -> String {
        let mut h = Sha256::new();
        for l in &self.allow {
            h.update(format!("allow {l}\n"));
        }
        for l in &self.deny {
            h.update(format!("deny {l}\n"));
        }
        h.update(format!("network {:?}\nmode {}\n", self.network, Self::MODE));
        h.finalize().iter().map(|b| format!("{b:02x}")).collect()
    }

    /// `--allow R ... --deny R ... --network N` for `synt run`.
    pub fn args(&self) -> Vec<std::ffi::OsString> {
        let mut v: Vec<std::ffi::OsString> = vec![];
        for r in &self.allow {
            v.extend(["--allow".into(), r.into()]);
        }
        for r in &self.deny {
            v.extend(["--deny".into(), r.into()]);
        }
        v.extend([
            "--network".into(),
            match self.network {
                Network::Deny => "deny",
                Network::Hosts => "hosts",
            }
            .into(),
        ]);
        v
    }
}

/// A worktree-relative envelope pattern as a rule path (`src/**` -> `/src/**`). A leading `/`
/// is project-relative in a rule, `//` would be an absolute path: the pattern is re-checked here
/// (the envelope validator checks it too) and the result must start with exactly one `/`.
fn rule_path(p: &str) -> Result<String, CompileError> {
    let bad = p.is_empty()
        || p.starts_with('/')
        || p.contains(['(', ')', '\\'])
        || p.split('/').any(|s| s.is_empty() || s == "." || s == "..");
    let rule = format!("/{p}");
    if bad || rule.starts_with("//") {
        return Err(CompileError::PathPattern(p.to_owned()));
    }
    Ok(rule)
}

fn test_rule(cmd: &str) -> Result<String, CompileError> {
    let plain = cmd
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, ' ' | '-' | '_' | '.' | '/' | '=' | ':'));
    let known = TEST_RUNNERS
        .iter()
        .any(|r| cmd == *r || cmd.strip_prefix(r).is_some_and(|t| t.starts_with(' ')));
    if cmd.is_empty() || !plain || !known {
        return Err(CompileError::TestCommand(cmd.to_owned()));
    }
    Ok(format!("Bash({cmd}:*)"))
}

/// Compiles `env`. `test_commands` come from the user's settings (trusted), used only when the
/// envelope allows `tests.run`.
pub fn compile_envelope(
    env: &Envelope,
    test_commands: &[String],
) -> Result<CompiledEnvelope, CompileError> {
    validate_envelope(env)?;
    let mut allow = BTreeSet::new();
    let mut deny = BTreeSet::new();
    let mode = |c| env.mode(c);
    let add = |set: &mut BTreeSet<String>, rules: &[&str]| {
        set.extend(rules.iter().map(|r| (*r).to_owned()));
    };

    // paths: the denied ones are refused for every tool that reads, lists or edits. A pattern
    // that does not end in `/**` names a file or a directory: both the path and what is under it
    // are denied (`secrets` and `secrets/` style entries cannot fail open).
    for p in &env.denied_paths {
        let rp = rule_path(p)?;
        let mut forms = vec![rp.clone()];
        if !rp.ends_with("**") {
            forms.push(format!("{rp}/**"));
        }
        for tool in ["Read", "Grep", "Glob", "LS", "Edit"] {
            for f in &forms {
                deny.insert(format!("{tool}({f})"));
            }
        }
    }
    if mode(Capability::RepoRead) != CapabilityMode::Allow {
        add(&mut deny, &["Read", "Grep", "Glob", "LS"]);
    }
    match mode(Capability::RepoWrite) {
        CapabilityMode::Allow => {
            for p in &env.allowed_paths {
                allow.insert(format!("Edit({})", rule_path(p)?));
            }
        }
        CapabilityMode::Deny => add(&mut deny, &["Edit"]),
        // asks: nothing allowed, so every edit becomes a decision
        CapabilityMode::ApprovalRequired => {}
    }
    if mode(Capability::TestsRun) == CapabilityMode::Allow {
        for c in test_commands {
            allow.insert(test_rule(c)?);
        }
    }
    // irreversible: `deny` (or unlisted) gets explicit deny rules; `approval_required` is left to
    // the engine's irreversible table, which asks (a decision in an attempt)
    if mode(Capability::GitPush) == CapabilityMode::Deny {
        add(&mut deny, &["Bash(git push:*)"]);
    }
    if mode(Capability::SecretsRead) == CapabilityMode::Deny {
        add(
            &mut deny,
            &[
                "Read(**/.env)",
                "Read(**/.env.*)",
                "Read(~/.ssh/**)",
                "Read(~/.aws/**)",
                "Read(~/.gnupg/**)",
            ],
        );
    }
    if mode(Capability::DeployProduction) == CapabilityMode::Deny {
        add(
            &mut deny,
            &[
                "Bash(npm publish:*)",
                "Bash(cargo publish:*)",
                "Bash(docker push:*)",
                "Bash(kubectl:*)",
                "Bash(terraform apply:*)",
            ],
        );
    }
    let network = match mode(Capability::NetworkOutbound) {
        CapabilityMode::Allow => Network::Hosts,
        CapabilityMode::Deny => {
            add(
                &mut deny,
                &["WebFetch", "WebSearch", "Bash(curl:*)", "Bash(wget:*)"],
            );
            Network::Deny
        }
        CapabilityMode::ApprovalRequired => Network::Deny,
    };
    Ok(CompiledEnvelope {
        allow: allow.into_iter().collect(),
        deny: deny.into_iter().collect(),
        network,
    })
}
