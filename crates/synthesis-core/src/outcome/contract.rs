//! The contract: goal, criteria and the authority envelope (ADR-019 section 3, ADR-020 section 4),
//! plus budgets. Validation is a pure function; the contract hash and immutability are the
//! orchestrator's (PR-07).

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

pub const MAX_GOAL_CHARS: usize = 8_000;
pub const MAX_CRITERIA: usize = 64;
pub const MAX_CRITERION_CHARS: usize = 2_000;
pub const MAX_PATH_PATTERNS: usize = 256;
pub const MAX_PATH_PATTERN_CHARS: usize = 512;
/// Hard ceilings: a contract cannot ask for more than these whatever the settings say.
pub const MAX_PARALLEL_WORKERS: u32 = 16;
pub const MAX_ATTEMPTS_PER_WORK_ITEM: u32 = 10;
/// Defaults of plan D10: 2 workers, 2 retries (so 3 attempts). The cost cap has no default.
pub const DEFAULT_MAX_PARALLEL_WORKERS: u32 = 2;
pub const DEFAULT_MAX_ATTEMPTS_PER_WORK_ITEM: u32 = 3;

/// Ids of criteria, work items and checks: short, no path or shell significance, and safe as a
/// file name on Windows too (no trailing `.`, no reserved device name such as `NUL` or `COM1.txt`).
pub(crate) fn valid_key(s: &str) -> bool {
    let mut b = s.bytes();
    s.len() <= 64
        && b.next().is_some_and(|c| c.is_ascii_alphanumeric())
        && b.all(|c| c.is_ascii_alphanumeric() || matches!(c, b'_' | b'-' | b'.'))
        && !s.ends_with('.')
        && !reserved_device_name(s)
}

fn reserved_device_name(s: &str) -> bool {
    let stem = s.split('.').next().unwrap_or(s).to_ascii_uppercase();
    matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || ["COM", "LPT"].iter().any(|p| {
            stem.strip_prefix(p)
                .is_some_and(|n| n.len() == 1 && matches!(n.as_bytes()[0], b'1'..=b'9'))
        })
}

/// Longest list a contract or plan item may carry (untrusted input is bounded).
pub const MAX_LIST_ENTRIES: usize = 64;

// -------------------------------------------------------------- capabilities

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum Capability {
    #[serde(rename = "repo.read")]
    RepoRead,
    #[serde(rename = "repo.write")]
    RepoWrite,
    #[serde(rename = "tests.run")]
    TestsRun,
    #[serde(rename = "network.outbound")]
    NetworkOutbound,
    #[serde(rename = "git.push")]
    GitPush,
    #[serde(rename = "secrets.read")]
    SecretsRead,
    #[serde(rename = "deploy.production")]
    DeployProduction,
}

impl Capability {
    pub const ALL: [Capability; 7] = [
        Self::RepoRead,
        Self::RepoWrite,
        Self::TestsRun,
        Self::NetworkOutbound,
        Self::GitPush,
        Self::SecretsRead,
        Self::DeployProduction,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::RepoRead => "repo.read",
            Self::RepoWrite => "repo.write",
            Self::TestsRun => "tests.run",
            Self::NetworkOutbound => "network.outbound",
            Self::GitPush => "git.push",
            Self::SecretsRead => "secrets.read",
            Self::DeployProduction => "deploy.production",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|c| c.as_str() == s)
    }

    /// The irreversible table of the permission engine (push, publish, deploy) and secrets: an
    /// envelope may deny these or make them need approval, never allow them (ADR-019 section 3).
    pub fn is_irreversible(self) -> bool {
        matches!(
            self,
            Self::GitPush | Self::SecretsRead | Self::DeployProduction
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CapabilityMode {
    Allow,
    Deny,
    ApprovalRequired,
}

// ------------------------------------------------------------------- budgets

fn default_parallel() -> u32 {
    DEFAULT_MAX_PARALLEL_WORKERS
}
fn default_attempts() -> u32 {
    DEFAULT_MAX_ATTEMPTS_PER_WORK_ITEM
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Budgets {
    /// Required, no default: the person names the money they accept to spend (D10).
    pub max_cost_usd: f64,
    #[serde(default = "default_parallel")]
    pub max_parallel_workers: u32,
    /// Attempts per work item, the first included.
    #[serde(default = "default_attempts")]
    pub max_attempts_per_work_item: u32,
    #[serde(default)]
    pub attempt_max_cost_usd: Option<f64>,
    #[serde(default)]
    pub attempt_max_wall_secs: Option<u64>,
}

impl Budgets {
    pub fn new(max_cost_usd: f64) -> Self {
        Self {
            max_cost_usd,
            max_parallel_workers: DEFAULT_MAX_PARALLEL_WORKERS,
            max_attempts_per_work_item: DEFAULT_MAX_ATTEMPTS_PER_WORK_ITEM,
            attempt_max_cost_usd: None,
            attempt_max_wall_secs: None,
        }
    }

    /// Retries after the first attempt (the `WorkItemGuards::max_retries` input).
    pub fn max_retries(&self) -> u32 {
        self.max_attempts_per_work_item.saturating_sub(1)
    }

    /// True when `spent` reached the outcome cap. Not-a-number counts as reached.
    pub fn cost_exhausted(&self, spent_usd: f64) -> bool {
        spent_usd.is_nan() || spent_usd >= self.max_cost_usd
    }

    pub fn validate(&self) -> Result<(), EnvelopeError> {
        let money = |v: f64| v.is_finite() && v > 0.0;
        if !money(self.max_cost_usd) {
            return Err(EnvelopeError::Budget(
                "max_cost_usd must be a positive number",
            ));
        }
        if !(1..=MAX_PARALLEL_WORKERS).contains(&self.max_parallel_workers) {
            return Err(EnvelopeError::Budget(
                "max_parallel_workers is out of range",
            ));
        }
        if !(1..=MAX_ATTEMPTS_PER_WORK_ITEM).contains(&self.max_attempts_per_work_item) {
            return Err(EnvelopeError::Budget(
                "max_attempts_per_work_item is out of range",
            ));
        }
        if let Some(c) = self.attempt_max_cost_usd
            && !(money(c) && c <= self.max_cost_usd)
        {
            return Err(EnvelopeError::Budget(
                "attempt_max_cost_usd must be positive and not above max_cost_usd",
            ));
        }
        if self.attempt_max_wall_secs == Some(0) {
            return Err(EnvelopeError::Budget(
                "attempt_max_wall_secs must be positive",
            ));
        }
        Ok(())
    }
}

// ------------------------------------------------------------------ envelope

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Envelope {
    /// Worktree-relative glob patterns the attempt may touch.
    #[serde(default)]
    pub allowed_paths: Vec<String>,
    /// Worktree-relative glob patterns that are refused whatever `allowed_paths` says.
    #[serde(default)]
    pub denied_paths: Vec<String>,
    /// A capability that is not listed is denied.
    #[serde(default)]
    pub capabilities: BTreeMap<Capability, CapabilityMode>,
    pub budgets: Budgets,
    /// Names of quality gates the contract requires (interpreted by the orchestrator).
    #[serde(default)]
    pub quality_gates: Vec<String>,
}

impl Envelope {
    pub fn mode(&self, c: Capability) -> CapabilityMode {
        self.capabilities
            .get(&c)
            .copied()
            .unwrap_or(CapabilityMode::Deny)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EnvelopeError {
    /// `allow` for a capability of the irreversible table.
    IrreversibleAllow(Capability),
    BadPath(String),
    TooManyPaths,
    Budget(&'static str),
}

impl std::fmt::Display for EnvelopeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::IrreversibleAllow(c) => write!(
                f,
                "envelope allows {}: irreversible capabilities must be deny or approval_required",
                c.as_str()
            ),
            Self::BadPath(p) => write!(
                f,
                "path pattern {p:?} must be a non-empty relative pattern with `/` separators and no `..`"
            ),
            Self::TooManyPaths => write!(f, "more than {MAX_PATH_PATTERNS} path patterns"),
            Self::Budget(m) => write!(f, "budget: {m}"),
        }
    }
}
impl std::error::Error for EnvelopeError {}

/// Control characters and the invisible or direction-changing ones (soft hyphen, bidi, zero-width,
/// variation selectors, fillers, line and paragraph separators, the tag block): text from a model
/// must not rewrite what a person reads. One list, used for decision briefs; path patterns use an
/// allow rule instead (see `valid_pattern`).
pub fn hidden_char(c: char) -> bool {
    c.is_control()
        || matches!(
            c,
            '\u{00AD}'
                | '\u{034F}'
                | '\u{061C}'
                | '\u{115F}'
                | '\u{1160}'
                | '\u{17B4}'
                | '\u{17B5}'
                | '\u{180B}'..='\u{180F}'
                | '\u{200B}'..='\u{200F}'
                | '\u{2028}'..='\u{202E}'
                | '\u{2060}'..='\u{206F}'
                | '\u{3164}'
                | '\u{FE00}'..='\u{FE0F}'
                | '\u{FEFF}'
                | '\u{FFA0}'
                | '\u{FFF9}'..='\u{FFFB}'
                | '\u{E0000}'..='\u{E01EF}'
        )
}

/// A worktree-relative pattern. Only printable ASCII is accepted: a deny-list of lookalike and
/// invisible characters can never be complete (a fullwidth star or a Cyrillic letter would compile
/// to a rule that matches nothing). Non-ASCII file names need a future, explicit escape syntax.
/// The glob language is `*`, `**` and `?` only: `[ ] { } !` are refused because the permission
/// engine treats them as literals, so a pattern using them would deny nothing. Segments between
/// `/` must be non-empty and neither `.` nor `..` (`.//x`, `a//b`, `./x`, `x/` are refused: a
/// leading empty segment would become an absolute path in a permission rule). Parentheses
/// (rule syntax) and backslash (Windows separator) are refused.
fn valid_pattern(p: &str) -> bool {
    let bytes = p.as_bytes();
    !p.is_empty()
        && p.chars().count() <= MAX_PATH_PATTERN_CHARS
        && p.chars().all(|c| {
            matches!(c, ' '..='~')
                && !matches!(c, '\\' | '(' | ')' | '[' | ']' | '{' | '}' | '!')
        })
        && !p.starts_with('/')
        && !p.starts_with('~')
        && !p.starts_with(' ')
        && !p.ends_with(' ')
        // `C:...` drive paths
        && !(bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':')
        && p.split('/').all(|seg| !seg.is_empty() && seg != "." && seg != "..")
}

/// Refuses an envelope that could never be compiled safely (ADR-019 section 3).
pub fn validate_envelope(e: &Envelope) -> Result<(), EnvelopeError> {
    for (&c, &m) in &e.capabilities {
        if c.is_irreversible() && m == CapabilityMode::Allow {
            return Err(EnvelopeError::IrreversibleAllow(c));
        }
    }
    if e.allowed_paths.len() > MAX_PATH_PATTERNS || e.denied_paths.len() > MAX_PATH_PATTERNS {
        return Err(EnvelopeError::TooManyPaths);
    }
    if let Some(bad) = e
        .allowed_paths
        .iter()
        .chain(&e.denied_paths)
        .find(|p| !valid_pattern(p))
    {
        return Err(EnvelopeError::BadPath(bad.clone()));
    }
    e.budgets.validate()
}

// ------------------------------------------------------------------ criteria

/// Ordered, lowest first (ADR-020 section 2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TrustClass {
    ClaimedByWorker,
    ModelAssessment,
    ObservedInAttempt,
    IndependentCheck,
    HumanReview,
}

fn independent() -> TrustClass {
    TrustClass::IndependentCheck
}
fn yes() -> bool {
    true
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContractCriterion {
    pub id: String,
    pub text: String,
    #[serde(default = "yes")]
    pub mandatory: bool,
    /// Only a person's review can satisfy it (then `required_trust` is `human_review`).
    #[serde(default)]
    pub manual: bool,
    #[serde(default = "independent")]
    pub required_trust: TrustClass,
    /// Checks (frozen at approval) that produce evidence for this criterion.
    #[serde(default)]
    pub check_ids: Vec<String>,
}

/// A check the independent runner runs on a fresh checkout of the attempt's commit. It is part of
/// the contract the person approves (the hash covers it): the runner runs nothing else, and the
/// evidence names the command's hash.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContractCheck {
    pub id: String,
    pub command: String,
}

pub const MAX_CHECKS: usize = 32;
pub const MAX_CHECK_COMMAND_CHARS: usize = 512;

/// What the outcome may do to the folder. Part of the approved body, so the contract hash and its
/// MAC cover it; a revision cannot change it (the store refuses), and a write contract can never
/// become a read-only one (or the reverse) without a new outcome.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContractKind {
    /// Today's outcome: attempts edit a worktree and independent checks verify them.
    #[default]
    Write,
    /// Research or review: attempts only read the folder in place and send back a report.
    ReadOnly,
}

impl ContractKind {
    pub fn is_write(&self) -> bool {
        *self == Self::Write
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Contract {
    /// 1-based; a change is a new version, never an edit (ADR-017).
    pub version: u32,
    /// Absent from the stored JSON for a write contract (older contracts keep their hash, and a
    /// binary from before read-only goals refuses the unknown field: fail closed).
    #[serde(default, skip_serializing_if = "ContractKind::is_write")]
    pub kind: ContractKind,
    pub goal: String,
    pub criteria: Vec<ContractCriterion>,
    pub envelope: Envelope,
    /// Frozen at approval. Absent from the stored JSON when empty (older contracts keep their hash).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub checks: Vec<ContractCheck>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ContractError {
    Version,
    Goal,
    Criteria(&'static str),
    BadId(String),
    DuplicateCriterion(String),
    /// A trust requirement that no evidence class could honestly meet, or that weakens the gate.
    Trust(String),
    Envelope(EnvelopeError),
    /// A check with a bad id or command, a duplicate, or a criterion naming an unknown check.
    Check(String),
    /// A read-only goal that asks for more than reading.
    ReadOnly(&'static str),
}

impl std::fmt::Display for ContractError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Version => write!(f, "contract version must be 1 or more"),
            Self::Goal => write!(f, "goal must be 1 to {MAX_GOAL_CHARS} characters"),
            Self::Criteria(m) => write!(f, "criteria: {m}"),
            Self::BadId(i) => write!(
                f,
                "invalid id {i:?} (letters, digits, `_`, `-`, `.`; at most 64)"
            ),
            Self::DuplicateCriterion(i) => write!(f, "duplicate criterion id {i:?}"),
            Self::Trust(i) => write!(
                f,
                "criterion {i:?}: required_trust must be independent_check, or human_review with manual = true"
            ),
            Self::Envelope(e) => write!(f, "{e}"),
            Self::Check(m) => write!(f, "checks: {m}"),
            Self::ReadOnly(m) => write!(f, "read-only goal: {m}"),
        }
    }
}
impl std::error::Error for ContractError {}

impl From<EnvelopeError> for ContractError {
    fn from(e: EnvelopeError) -> Self {
        Self::Envelope(e)
    }
}

pub fn validate_contract(c: &Contract) -> Result<(), ContractError> {
    if c.version == 0 {
        return Err(ContractError::Version);
    }
    if c.goal.trim().is_empty() || c.goal.chars().count() > MAX_GOAL_CHARS {
        return Err(ContractError::Goal);
    }
    if c.criteria.is_empty() || c.criteria.len() > MAX_CRITERIA {
        return Err(ContractError::Criteria("1 to 64 criteria are required"));
    }
    let mut seen = BTreeSet::new();
    for k in &c.criteria {
        if !valid_key(&k.id)
            || k.check_ids.len() > MAX_LIST_ENTRIES
            || !k.check_ids.iter().all(|i| valid_key(i))
        {
            return Err(ContractError::BadId(k.id.clone()));
        }
        // `C1` and `c1` would be one file on a case-folding volume
        if !seen.insert(k.id.to_ascii_lowercase()) {
            return Err(ContractError::DuplicateCriterion(k.id.clone()));
        }
        if k.text.trim().is_empty() || k.text.chars().count() > MAX_CRITERION_CHARS {
            return Err(ContractError::Criteria("text must be 1 to 2000 characters"));
        }
        // Lowering the bar (worker claim, model opinion, in-attempt gate) would let a criterion
        // pass without independent evidence; `human_review` is for `manual` criteria only.
        let want = if k.manual {
            TrustClass::HumanReview
        } else {
            TrustClass::IndependentCheck
        };
        if k.required_trust != want {
            return Err(ContractError::Trust(k.id.clone()));
        }
    }
    // A contract with no mandatory criterion would be "verified" vacuously.
    if !c.criteria.iter().any(|k| k.mandatory) {
        return Err(ContractError::Criteria(
            "at least one mandatory criterion is required",
        ));
    }
    validate_envelope(&c.envelope)?;
    if c.kind == ContractKind::ReadOnly {
        validate_read_only(c)?;
    }
    if c.checks.len() > MAX_CHECKS {
        return Err(ContractError::Check(format!(
            "more than {MAX_CHECKS} checks"
        )));
    }
    let mut ids = BTreeSet::new();
    for k in &c.checks {
        if !valid_key(&k.id) || !ids.insert(k.id.to_ascii_lowercase()) {
            return Err(ContractError::Check(format!(
                "check id {:?} is invalid or repeated",
                k.id
            )));
        }
        if k.command.trim().is_empty()
            || k.command.chars().count() > MAX_CHECK_COMMAND_CHARS
            || k.command.chars().any(char::is_control)
            || k.command != k.command.trim()
        {
            return Err(ContractError::Check(format!(
                "check {:?}: the command must be 1 to {MAX_CHECK_COMMAND_CHARS} printable characters, one line",
                k.id
            )));
        }
    }
    // with frozen checks, a criterion may only name those (without them `check_ids` are free labels)
    for k in c.criteria.iter().filter(|_| !c.checks.is_empty()) {
        if let Some(bad) = k
            .check_ids
            .iter()
            .find(|i| !ids.contains(&i.to_ascii_lowercase()))
        {
            return Err(ContractError::Check(format!(
                "criterion {:?} names the unknown check {bad:?}",
                k.id
            )));
        }
    }
    Ok(())
}

/// A read-only goal is the goal and a budget: only a person's review can satisfy it, and it asks
/// for nothing but reading (S5: nothing here lets an attempt write, run or reach out).
fn validate_read_only(c: &Contract) -> Result<(), ContractError> {
    let no = ContractError::ReadOnly;
    if c.criteria.iter().any(|k| !k.manual) {
        return Err(no("every criterion is a person's review (manual)"));
    }
    if !c.checks.is_empty() {
        return Err(no("it has no checks"));
    }
    if !c.envelope.allowed_paths.is_empty() {
        return Err(no("it has no write scope (allowed_paths)"));
    }
    if c.envelope
        .capabilities
        .iter()
        .any(|(&cap, &m)| cap != Capability::RepoRead && m != CapabilityMode::Deny)
    {
        return Err(no("only repo.read may be allowed"));
    }
    if c.envelope.budgets.max_parallel_workers != 1 {
        return Err(no("max_parallel_workers must be 1"));
    }
    Ok(())
}

#[cfg(test)]
pub(crate) mod fixtures {
    use super::*;

    pub fn criterion(id: &str) -> ContractCriterion {
        ContractCriterion {
            id: id.into(),
            text: format!("criterion {id}"),
            mandatory: true,
            manual: false,
            required_trust: TrustClass::IndependentCheck,
            check_ids: vec![],
        }
    }

    /// A valid read-only goal: the implicit `report` criterion and one rubric line.
    pub fn read_only() -> Contract {
        let mut c = contract(&[]);
        c.kind = ContractKind::ReadOnly;
        c.criteria = ["report", "r1"]
            .iter()
            .map(|id| ContractCriterion {
                manual: true,
                required_trust: TrustClass::HumanReview,
                ..criterion(id)
            })
            .collect();
        c.envelope.allowed_paths = vec![];
        c.envelope.denied_paths = vec![];
        c.envelope.capabilities = BTreeMap::from([(Capability::RepoRead, CapabilityMode::Allow)]);
        c.envelope.budgets.max_parallel_workers = 1;
        c
    }

    pub fn contract(ids: &[&str]) -> Contract {
        let mut capabilities = BTreeMap::new();
        capabilities.insert(Capability::RepoRead, CapabilityMode::Allow);
        capabilities.insert(Capability::RepoWrite, CapabilityMode::Allow);
        capabilities.insert(Capability::TestsRun, CapabilityMode::Allow);
        capabilities.insert(Capability::GitPush, CapabilityMode::Deny);
        Contract {
            version: 1,
            kind: ContractKind::Write,
            goal: "implement the endpoint".into(),
            criteria: ids.iter().map(|i| criterion(i)).collect(),
            envelope: Envelope {
                allowed_paths: vec!["src/**".into(), "tests/**".into()],
                denied_paths: vec!["infra/**".into()],
                capabilities,
                budgets: Budgets::new(5.0),
                quality_gates: vec![],
            },
            checks: vec![],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::fixtures::*;
    use super::*;

    #[test]
    fn a_good_contract_validates_and_round_trips() {
        let c = contract(&["c1", "c2", "c3"]);
        assert_eq!(validate_contract(&c), Ok(()));
        let j = serde_json::to_string(&c).unwrap();
        assert!(j.contains("\"repo.read\":\"allow\""), "{j}");
        assert_eq!(serde_json::from_str::<Contract>(&j).unwrap(), c);
    }

    #[test]
    fn kind_is_stored_only_when_it_is_not_the_default() {
        let w = serde_json::to_value(contract(&["c1"])).unwrap();
        assert!(w.get("kind").is_none(), "{w}");
        let r = serde_json::to_value(read_only()).unwrap();
        assert_eq!(r["kind"], "read_only");
        assert_eq!(serde_json::from_value::<Contract>(r).unwrap(), read_only());
        assert_eq!(
            serde_json::from_value::<Contract>(w).unwrap().kind,
            ContractKind::Write
        );
    }

    /// An older binary parses the contract with the shape below (every field of v1, nothing
    /// else, `deny_unknown_fields`): a read-only body is refused there, so that outcome fails
    /// closed and the old binary never runs it as a write goal. The write body is unchanged.
    #[test]
    fn a_pre_read_only_parser_refuses_a_read_only_body_and_reads_a_write_one() {
        #[derive(Debug, Deserialize)]
        #[serde(deny_unknown_fields)]
        #[allow(dead_code)]
        struct OldContract {
            version: u32,
            goal: String,
            criteria: Vec<ContractCriterion>,
            envelope: Envelope,
            #[serde(default)]
            checks: Vec<ContractCheck>,
        }
        let ro = serde_json::to_string(&read_only()).unwrap();
        let err = serde_json::from_str::<OldContract>(&ro)
            .unwrap_err()
            .to_string();
        assert!(err.contains("unknown field `kind`"), "{err}");
        let wr = serde_json::to_string(&contract(&["c1"])).unwrap();
        assert!(serde_json::from_str::<OldContract>(&wr).is_ok());
        assert!(!wr.contains("kind"), "{wr}");
    }

    #[test]
    fn a_read_only_goal_asks_for_nothing_but_reading() {
        assert_eq!(validate_contract(&read_only()), Ok(()));
        let bad: [(&str, fn(&mut Contract)); 6] = [
            ("checked criterion", |c| {
                c.criteria[0].manual = false;
                c.criteria[0].required_trust = TrustClass::IndependentCheck;
            }),
            ("check", |c| {
                c.checks.push(ContractCheck {
                    id: "t".into(),
                    command: "true".into(),
                })
            }),
            ("write scope", |c| {
                c.envelope.allowed_paths = vec!["src/**".into()]
            }),
            ("repo.write", |c| {
                c.envelope
                    .capabilities
                    .insert(Capability::RepoWrite, CapabilityMode::Allow);
            }),
            ("approval for tests", |c| {
                c.envelope
                    .capabilities
                    .insert(Capability::TestsRun, CapabilityMode::ApprovalRequired);
            }),
            ("two workers", |c| {
                c.envelope.budgets.max_parallel_workers = 2
            }),
        ];
        for (name, f) in bad {
            let mut c = read_only();
            f(&mut c);
            assert!(
                matches!(validate_contract(&c), Err(ContractError::ReadOnly(_))),
                "{name}"
            );
        }
        // a denied capability is fine; the same body as a write goal is judged by the old rules
        let mut c = read_only();
        c.envelope
            .capabilities
            .insert(Capability::RepoWrite, CapabilityMode::Deny);
        assert_eq!(validate_contract(&c), Ok(()));
    }

    #[test]
    fn serde_defaults_follow_d10_and_the_cost_cap_is_required() {
        let e: Envelope = serde_json::from_str(r#"{"budgets":{"max_cost_usd":3}}"#).unwrap();
        assert_eq!(e.budgets.max_parallel_workers, 2);
        assert_eq!(e.budgets.max_attempts_per_work_item, 3);
        assert_eq!(e.budgets.max_retries(), 2);
        assert_eq!(
            e.mode(Capability::GitPush),
            CapabilityMode::Deny,
            "unlisted = deny"
        );
        assert!(serde_json::from_str::<Envelope>(r#"{"budgets":{}}"#).is_err());
        assert!(serde_json::from_str::<Envelope>(r#"{}"#).is_err());
        // unknown fields and unknown capabilities are errors, never silently dropped
        assert!(
            serde_json::from_str::<Envelope>(r#"{"budgets":{"max_cost_usd":1},"x":1}"#).is_err()
        );
        assert!(
            serde_json::from_str::<Envelope>(
                r#"{"budgets":{"max_cost_usd":1},"capabilities":{"root.shell":"allow"}}"#
            )
            .is_err()
        );
    }

    #[test]
    fn irreversible_capabilities_are_never_allowed() {
        for c in Capability::ALL {
            let mut k = contract(&["c1"]);
            k.envelope.capabilities.insert(c, CapabilityMode::Allow);
            let r = validate_contract(&k);
            if c.is_irreversible() {
                assert_eq!(
                    r,
                    Err(ContractError::Envelope(EnvelopeError::IrreversibleAllow(c)))
                );
            } else {
                assert_eq!(r, Ok(()), "{c:?}");
            }
            for m in [CapabilityMode::Deny, CapabilityMode::ApprovalRequired] {
                k.envelope.capabilities.insert(c, m);
                assert_eq!(validate_contract(&k), Ok(()), "{c:?} {m:?}");
            }
        }
        assert_eq!(
            Capability::ALL
                .iter()
                .filter(|c| c.is_irreversible())
                .count(),
            3
        );
    }

    #[test]
    fn capability_names_round_trip() {
        for c in Capability::ALL {
            assert_eq!(Capability::parse(c.as_str()), Some(c));
            assert_eq!(
                serde_json::to_string(&c).unwrap(),
                format!("\"{}\"", c.as_str())
            );
        }
        assert_eq!(Capability::parse("repo.delete"), None);
    }

    #[test]
    fn bad_path_patterns_are_refused() {
        for p in [
            "",
            "/etc/passwd",
            "../x",
            "a/../b",
            "a\\b",
            "~/x",
            "C:/x",
            "c:x",
            "a\nb",
            "..",
            &"x".repeat(MAX_PATH_PATTERN_CHARS + 1),
        ] {
            let mut k = contract(&["c1"]);
            k.envelope.allowed_paths = vec![p.into()];
            assert!(
                matches!(
                    validate_contract(&k),
                    Err(ContractError::Envelope(EnvelopeError::BadPath(_)))
                ),
                "{p:?}"
            );
            k.envelope.allowed_paths = vec![];
            k.envelope.denied_paths = vec![p.into()];
            assert!(validate_contract(&k).is_err(), "{p:?} (denied)");
        }
        for p in [
            "src/**",
            "tests/*.rs",
            "a..b/c",
            ".github/workflows/*",
            "Cargo.toml",
        ] {
            let mut k = contract(&["c1"]);
            k.envelope.allowed_paths = vec![p.into()];
            assert_eq!(validate_contract(&k), Ok(()), "{p:?}");
        }
        let mut k = contract(&["c1"]);
        k.envelope.allowed_paths = vec!["a".into(); MAX_PATH_PATTERNS + 1];
        assert_eq!(
            validate_contract(&k),
            Err(ContractError::Envelope(EnvelopeError::TooManyPaths))
        );
    }

    #[test]
    fn budgets_are_bounded_and_nan_safe() {
        let ok = Budgets::new(1.0);
        assert_eq!(ok.validate(), Ok(()));
        let bad = |f: fn(&mut Budgets)| {
            let mut b = Budgets::new(1.0);
            f(&mut b);
            assert!(
                matches!(b.validate(), Err(EnvelopeError::Budget(_))),
                "{b:?}"
            );
        };
        bad(|b| b.max_cost_usd = 0.0);
        bad(|b| b.max_cost_usd = -1.0);
        bad(|b| b.max_cost_usd = f64::NAN);
        bad(|b| b.max_cost_usd = f64::INFINITY);
        bad(|b| b.max_parallel_workers = 0);
        bad(|b| b.max_parallel_workers = MAX_PARALLEL_WORKERS + 1);
        bad(|b| b.max_attempts_per_work_item = 0);
        bad(|b| b.max_attempts_per_work_item = MAX_ATTEMPTS_PER_WORK_ITEM + 1);
        bad(|b| b.attempt_max_cost_usd = Some(2.0));
        bad(|b| b.attempt_max_cost_usd = Some(f64::NAN));
        bad(|b| b.attempt_max_wall_secs = Some(0));
        assert!(ok.cost_exhausted(1.0));
        assert!(ok.cost_exhausted(f64::NAN));
        assert!(!ok.cost_exhausted(0.99));
    }

    #[test]
    fn contract_shape_errors_are_distinct() {
        let base = contract(&["c1"]);
        let mut k = base.clone();
        k.version = 0;
        assert_eq!(validate_contract(&k), Err(ContractError::Version));
        let mut k = base.clone();
        k.goal = "  ".into();
        assert_eq!(validate_contract(&k), Err(ContractError::Goal));
        let mut k = base.clone();
        k.criteria.clear();
        assert!(matches!(
            validate_contract(&k),
            Err(ContractError::Criteria(_))
        ));
        let mut k = contract(&["c1", "c1"]);
        assert_eq!(
            validate_contract(&k),
            Err(ContractError::DuplicateCriterion("c1".into()))
        );
        k.criteria[1].id = "../x".into();
        assert!(matches!(
            validate_contract(&k),
            Err(ContractError::BadId(_))
        ));
        let mut k = base.clone();
        k.criteria[0].mandatory = false;
        assert!(
            matches!(validate_contract(&k), Err(ContractError::Criteria(_))),
            "vacuous"
        );
        let mut k = base.clone();
        k.criteria[0].text.clear();
        assert!(matches!(
            validate_contract(&k),
            Err(ContractError::Criteria(_))
        ));
    }

    #[test]
    fn a_criterion_cannot_lower_the_trust_bar() {
        for t in [
            TrustClass::ClaimedByWorker,
            TrustClass::ModelAssessment,
            TrustClass::ObservedInAttempt,
            TrustClass::HumanReview,
        ] {
            let mut k = contract(&["c1"]);
            k.criteria[0].required_trust = t;
            assert_eq!(
                validate_contract(&k),
                Err(ContractError::Trust("c1".into())),
                "{t:?}"
            );
        }
        // a manual criterion needs a person's review and nothing else
        let mut k = contract(&["c1", "m1"]);
        k.criteria[1].manual = true;
        assert!(validate_contract(&k).is_err());
        k.criteria[1].required_trust = TrustClass::HumanReview;
        assert_eq!(validate_contract(&k), Ok(()));
        assert!(TrustClass::ClaimedByWorker < TrustClass::IndependentCheck);
    }

    #[test]
    fn frozen_checks_are_validated_and_do_not_change_the_hash_of_contracts_without_them() {
        let check = |id: &str, cmd: &str| ContractCheck {
            id: id.into(),
            command: cmd.into(),
        };
        let mut k = contract(&["c1"]);
        // no checks: the stored JSON has no `checks` key (older contracts keep their hash)
        assert!(!serde_json::to_string(&k).unwrap().contains("checks"));
        k.checks = vec![check("unit", "cargo test")];
        k.criteria[0].check_ids = vec!["unit".into()];
        assert_eq!(validate_contract(&k), Ok(()));
        assert!(serde_json::to_string(&k).unwrap().contains("\"checks\""));
        for bad in [
            vec![check("unit", "")],
            vec![check("unit", "a\nb")],
            vec![check("unit", " lead")],
            vec![check("bad id", "x")],
            vec![check("unit", "x"), check("UNIT", "y")],
            vec![check("unit", &"x".repeat(MAX_CHECK_COMMAND_CHARS + 1))],
        ] {
            let mut b = k.clone();
            b.checks = bad;
            assert!(
                matches!(validate_contract(&b), Err(ContractError::Check(_))),
                "{:?}",
                b.checks
            );
        }
        // a criterion may only name checks that exist
        let mut b = k.clone();
        b.criteria[0].check_ids = vec!["lint".into()];
        assert!(matches!(
            validate_contract(&b),
            Err(ContractError::Check(_))
        ));
        let mut many = k.clone();
        many.checks = (0..=MAX_CHECKS)
            .map(|i| check(&format!("c{i}"), "x"))
            .collect();
        assert!(matches!(
            validate_contract(&many),
            Err(ContractError::Check(_))
        ));
    }
}
