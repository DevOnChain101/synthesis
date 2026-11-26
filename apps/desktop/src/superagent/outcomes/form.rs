//! The "New outcome" and "Revise contract" form, with no window in it: the plain text of the fields, the
//! `Contract` built from it (checked by the core's own `validate_contract`, nothing re-implemented), the
//! stages of the flow (edit, sending, review of what the server stored, approving) and the idempotency keys.
//!
//! Nothing here approves anything. A created or revised contract waits; approving is a separate, explicit act
//! on the review stage, with the version and hash the server reported, after the server's own diff is shown.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::Value;
use synthesis_core::outcome::{
    Budgets, Capability, CapabilityMode, Contract, ContractCheck, ContractCriterion,
    DEFAULT_MAX_ATTEMPTS_PER_WORK_ITEM, Envelope, HARD_MAX_TURNS, MAX_ATTEMPTS_PER_WORK_ITEM,
    TrustClass, validate_contract,
};
use synthesis_harness::sanitize_for_display as clean;
use synthesis_view::outcomes::CriteriaView;

/// What the person types: every field as plain text (lists: one entry per line).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct FormText {
    pub goal: String,
    pub criteria: String,
    pub checks: String,
    pub budget: String,
    pub scope: String,
    pub workspace: String,
    /// Read-only goals only: attempts per work item (empty: the core's default).
    pub attempts: String,
    /// Turns (model calls) per attempt (empty: none named, the setting applies).
    pub turns: String,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Mode {
    New,
    /// `revision` is the one the person saw when the form was opened: the server refuses a revision of
    /// an outcome that has moved since.
    Revise {
        outcome: String,
        revision: i64,
    },
    /// No fields: the stored contract of an outcome that waits for its first approval, read for review.
    Draft {
        outcome: String,
    },
}

/// The text of a contract, as the form shows it.
pub fn text_of(c: &Contract) -> FormText {
    FormText {
        goal: c.goal.clone(),
        criteria: c
            .criteria
            .iter()
            .map(|k| flat(&k.text))
            .collect::<Vec<_>>()
            .join("\n"),
        checks: c
            .checks
            .iter()
            .map(|k| flat(&k.command))
            .collect::<Vec<_>>()
            .join("\n"),
        budget: c.envelope.budgets.max_cost_usd.to_string(),
        scope: c.envelope.allowed_paths.join("\n"),
        workspace: String::new(),
        attempts: String::new(),
        turns: c
            .envelope
            .budgets
            .max_turns
            .map_or_else(String::new, |t| t.to_string()),
    }
}

/// The turn limit typed: empty names none (the setting applies), else a whole number in the core's range.
fn turns_of(t: &FormText) -> Result<Option<u32>, String> {
    match t.turns.trim() {
        "" => Ok(None),
        n => n
            .parse::<u32>()
            .ok()
            .filter(|n| (1..=HARD_MAX_TURNS).contains(n))
            .map(Some)
            .ok_or_else(|| {
                format!(
                    "Turns: a whole number from 1 to {HARD_MAX_TURNS}, or leave it empty for the setting."
                )
            }),
    }
}

/// A criterion or command as one form line (the form shows one entry per line).
fn flat(s: &str) -> String {
    s.replace(['\n', '\r'], " ").trim().to_string()
}

fn lines(s: &str) -> Vec<String> {
    s.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(String::from)
        .collect()
}

/// The next id `prefix`N that `used` does not have (case-folded: `C1` and `c1` would be one file on a
/// case-folding volume).
fn fresh(prefix: &str, used: &mut BTreeSet<String>) -> String {
    (1..)
        .map(|n| format!("{prefix}{n}"))
        .find(|id| used.insert(id.to_ascii_lowercase()))
        .unwrap_or_default()
}

/// A new contract takes what attempts need to work and nothing that is irreversible: read, write and run
/// tests; everything else is denied by not being listed.
fn default_envelope() -> Envelope {
    Envelope {
        allowed_paths: vec![],
        denied_paths: vec![],
        capabilities: BTreeMap::from([
            (Capability::RepoRead, CapabilityMode::Allow),
            (Capability::RepoWrite, CapabilityMode::Allow),
            (Capability::TestsRun, CapabilityMode::Allow),
        ]),
        budgets: Budgets::new(0.0),
        quality_gates: vec![],
    }
}

/// The contract the text says. With `prev` (a revision) everything the form cannot show (denied paths,
/// capabilities, caps per attempt, a criterion's flags and checks) stays as it was, matched by the exact text
/// of the line (see below); the server's diff, shown before anything is approved, says what changed. Errors are one line each; `validate_contract` is the
/// core's own check and runs last.
pub fn build(t: &FormText, prev: Option<&Contract>) -> Result<Contract, Vec<String>> {
    let mut errs = vec![];
    let goal = t.goal.trim().to_string();
    if goal.is_empty() {
        errs.push("Goal: say what the outcome must achieve.".to_string());
    }
    let (criteria, checks, scope) = (lines(&t.criteria), lines(&t.checks), lines(&t.scope));
    if criteria.is_empty() {
        errs.push("Acceptance criteria: add at least one, one per line.".to_string());
    }
    if checks.is_empty() {
        errs.push(
            "Checks: add at least one verification command, one per line; the outcome is verified by running them."
                .to_string(),
        );
    }
    if scope.is_empty() {
        errs.push(
            "Write scope: list at least one path pattern, one per line (for example src/**); with none, attempts can change nothing."
                .to_string(),
        );
    }
    let budget = t.budget.trim().parse::<f64>().ok();
    if budget.is_none() {
        errs.push(
            "Budget: enter the most you accept to spend, in USD (for example 5); there is no default."
                .to_string(),
        );
    }
    let turns = turns_of(t);
    if let Err(e) = &turns {
        errs.push(e.clone());
    }
    let (true, Some(budget), Ok(turns)) = (errs.is_empty(), budget, turns) else {
        return Err(errs);
    };

    let mut used_checks: BTreeSet<String> = prev
        .iter()
        .flat_map(|p| &p.checks)
        .map(|k| k.id.to_ascii_lowercase())
        .collect();
    let mut used_crit: BTreeSet<String> = prev
        .iter()
        .flat_map(|p| &p.criteria)
        .map(|k| k.id.to_ascii_lowercase())
        .collect();
    // Lines are matched to the previous contract by their exact text: an unchanged line keeps its id and
    // flags whatever moved around it; a new or edited line is a new entry with the STRICT defaults
    // (mandatory, independent check, backed by every check). The server's diff then shows what changed.
    // checks: a command that is unchanged keeps its id; when exactly one old command and one new command
    // are left over, the new one is that check edited, so it keeps the id (criteria keep naming it)
    let mut taken_checks = vec![false; prev.map_or(0, |p| p.checks.len())];
    let mut ids: Vec<Option<String>> = checks
        .iter()
        .map(|command| {
            let p = prev?;
            let i = (0..p.checks.len())
                .find(|&i| !taken_checks[i] && flat(&p.checks[i].command) == *command)?;
            taken_checks[i] = true;
            Some(p.checks[i].id.clone())
        })
        .collect();
    let left_old: Vec<usize> = (0..taken_checks.len())
        .filter(|&i| !taken_checks[i])
        .collect();
    let left_new: Vec<usize> = (0..ids.len()).filter(|&i| ids[i].is_none()).collect();
    if let (Some(p), [old], [new]) = (prev, &left_old[..], &left_new[..]) {
        ids[*new] = Some(p.checks[*old].id.clone());
    }
    let checks: Vec<ContractCheck> = checks
        .into_iter()
        .zip(ids)
        .map(|(command, id)| ContractCheck {
            id: id.unwrap_or_else(|| fresh("t", &mut used_checks)),
            command,
        })
        .collect();
    let all: Vec<String> = checks.iter().map(|k| k.id.clone()).collect();
    let mut taken_crit = vec![false; prev.map_or(0, |p| p.criteria.len())];
    let criteria: Vec<ContractCriterion> = criteria
        .into_iter()
        .map(|text| {
            let old = prev.and_then(|p| {
                let i = (0..p.criteria.len())
                    .find(|&i| !taken_crit[i] && flat(&p.criteria[i].text) == text)?;
                taken_crit[i] = true;
                Some(p.criteria[i].clone())
            });
            match old {
                Some(k) => ContractCriterion {
                    // a check that is gone from the list is not named any more
                    check_ids: k
                        .check_ids
                        .iter()
                        .filter(|c| all.contains(c))
                        .cloned()
                        .collect(),
                    ..k
                },
                None => ContractCriterion {
                    id: fresh("c", &mut used_crit),
                    text,
                    mandatory: true,
                    manual: false,
                    required_trust: TrustClass::IndependentCheck,
                    check_ids: all.clone(),
                },
            }
        })
        .collect();
    let mut envelope = prev.map_or_else(default_envelope, |p| p.envelope.clone());
    envelope.allowed_paths = scope;
    envelope.budgets.max_cost_usd = budget;
    envelope.budgets.max_turns = turns;
    let c = Contract {
        version: prev.map_or(1, |p| p.version),
        kind: prev.map_or_else(Default::default, |p| p.kind),
        goal,
        criteria,
        envelope,
        checks,
    };
    validate_contract(&c).map_err(|e| vec![e.to_string()])?;
    Ok(c)
}

/// The contract of a read-only goal (S1 section 1): `criteria` holds the rubric lines (optional), `scope` and
/// `checks` are not used. The core builds the contract and `validate_contract` (kind `read_only`) checks it.
pub fn build_read_only(t: &FormText) -> Result<Contract, Vec<String>> {
    let mut errs = vec![];
    let goal = t.goal.trim().to_string();
    if goal.is_empty() {
        errs.push("Goal: say what the goal must check or find out.".to_string());
    }
    let budget = t.budget.trim().parse::<f64>().ok();
    if budget.is_none() {
        errs.push(
            "Budget: enter the most you accept to spend, in USD (for example 2); there is no default."
                .to_string(),
        );
    }
    let attempts = match t.attempts.trim() {
        "" => Some(DEFAULT_MAX_ATTEMPTS_PER_WORK_ITEM),
        a => a
            .parse::<u32>()
            .ok()
            .filter(|n| (1..=MAX_ATTEMPTS_PER_WORK_ITEM).contains(n)),
    };
    if attempts.is_none() {
        errs.push(format!(
            "Attempts: a whole number from 1 to {MAX_ATTEMPTS_PER_WORK_ITEM}, or leave it empty for {DEFAULT_MAX_ATTEMPTS_PER_WORK_ITEM}."
        ));
    }
    let turns = turns_of(t);
    if let Err(e) = &turns {
        errs.push(e.clone());
    }
    let (true, Some(budget), Some(attempts), Ok(turns)) =
        (errs.is_empty(), budget, attempts, turns)
    else {
        return Err(errs);
    };
    let mut budgets = Budgets::new(budget);
    budgets.max_attempts_per_work_item = attempts;
    budgets.max_turns = turns;
    let c = Contract::read_only(goal, &lines(&t.criteria), budgets);
    validate_contract(&c).map_err(|e| vec![e.to_string()])?;
    Ok(c)
}

// ------------------------------------------------------------------------------------------ server's diff

#[derive(Clone, Debug, PartialEq)]
pub struct ChangeRow {
    pub area: String,
    pub key: String,
    pub old: String,
    pub new: String,
}

/// `outcome.contract.diff` as the server computed it: nothing here decides what widens or weakens.
#[derive(Clone, Debug, PartialEq)]
pub struct DiffView {
    pub base_version: String,
    pub changes: Vec<ChangeRow>,
    pub widens: Vec<String>,
    pub weakens: Vec<String>,
}

impl DiffView {
    /// From the reply for `version` with `hash`; a reply about another version or hash is refused.
    pub fn from_json(v: &Value, version: u32, hash: &str) -> Result<Self, String> {
        let about = v.get("version").and_then(Value::as_u64) == Some(u64::from(version))
            && v.get("hash").and_then(Value::as_str) == Some(hash);
        let Some(d) = v.get("diff").filter(|_| about) else {
            return Err(
                "the diff is about another contract version than the one to approve".into(),
            );
        };
        let strings = |k: &str| -> Result<Vec<String>, String> {
            d.get(k)
                .and_then(Value::as_array)
                .ok_or_else(|| format!("unreadable diff: no {k}"))?
                .iter()
                .map(|s| {
                    s.as_str()
                        .map(clean)
                        .ok_or_else(|| format!("unreadable diff: {k}"))
                })
                .collect()
        };
        let side = |c: &Value, k: &str| match c.get(k) {
            Some(Value::String(s)) => clean(s),
            _ => "(none)".to_string(),
        };
        let changes = d
            .get("changes")
            .and_then(Value::as_array)
            .ok_or("unreadable diff: no changes")?
            .iter()
            .map(|c| ChangeRow {
                area: side(c, "area"),
                key: side(c, "key"),
                old: side(c, "old"),
                new: side(c, "new"),
            })
            .collect();
        let (widens, weakens) = (strings("widens")?, strings("weakens")?);
        if v.get("widens_scope").and_then(Value::as_bool) != Some(!widens.is_empty()) {
            return Err("unreadable diff: widens_scope does not match widens".into());
        }
        Ok(Self {
            base_version: v
                .pointer("/base/version")
                .and_then(Value::as_u64)
                .map_or_else(|| "?".into(), |b| b.to_string()),
            changes,
            widens,
            weakens,
        })
    }
}

// ------------------------------------------------------------------------------------------ the flow

/// What the server stored and the person is asked to approve.
#[derive(Clone, Debug, PartialEq)]
pub struct Review {
    pub outcome: String,
    pub version: u32,
    pub hash: String,
    /// The stored contract; an error blocks the approval (what cannot be read cannot be approved).
    pub contract: Result<CriteriaView, String>,
    /// Always read for a version above 1; `None` only for the first version, which has nothing to compare
    /// with. An error (or none, above 1) blocks the approval.
    pub diff: Option<Result<DiffView, String>>,
    /// Sent with the approval and kept for a retry after a lost reply.
    pub key: String,
}

impl Review {
    pub fn approvable(&self) -> bool {
        // a later version is compared with the approved one, and only a diff that was read allows approving it
        self.contract.is_ok()
            && match self.version {
                1 => true,
                _ => matches!(self.diff, Some(Ok(_))),
            }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum Stage {
    Edit,
    Sending,
    Review(Review),
    Approving(Review),
}

/// The base commit shown on the form: `HEAD` of `ws`, read by the worker.
#[derive(Clone, Debug, PartialEq)]
pub enum Head {
    Resolving(String),
    Known { ws: String, commit: String },
    Failed { ws: String, why: String },
}

/// What a submit asks the link to do.
#[derive(Clone, Debug, PartialEq)]
pub enum Submit {
    Create {
        workspace: String,
        base_commit: String,
        contract: Box<Contract>,
        key: String,
    },
    Revise {
        outcome: String,
        revision: i64,
        contract: Box<Contract>,
        key: String,
    },
}

#[derive(Clone, Debug)]
pub struct Form {
    pub mode: Mode,
    /// The contract a revision starts from.
    pub prev: Option<Contract>,
    /// What the fields start with.
    pub initial: FormText,
    pub head: Option<Head>,
    pub stage: Stage,
    /// A read-only goal: the folder is any directory and there is no base commit (S1 section 1).
    pub read_only: bool,
    /// Inline errors and refusals, one line each; typed text is never cleared by one.
    pub errors: Vec<String>,
    /// The last request sent whose answer is not known: sent again unchanged, it carries the same key.
    sent: Option<Submit>,
}

impl Form {
    pub fn new_outcome(workspace: &str) -> Self {
        Self {
            mode: Mode::New,
            prev: None,
            initial: FormText {
                workspace: workspace.to_string(),
                ..Default::default()
            },
            head: None,
            stage: Stage::Edit,
            read_only: false,
            errors: vec![],
            sent: None,
        }
    }

    /// "New goal (read-only)": a new outcome whose contract has kind `read_only`; no `HEAD` is read.
    pub fn new_read_only(workspace: &str) -> Self {
        Self {
            read_only: true,
            ..Self::new_outcome(workspace)
        }
    }

    pub fn revise(outcome: &str, revision: i64, prev: Contract) -> Self {
        Self {
            mode: Mode::Revise {
                outcome: outcome.to_string(),
                revision,
            },
            initial: text_of(&prev),
            prev: Some(prev),
            head: None,
            stage: Stage::Edit,
            read_only: false,
            errors: vec![],
            sent: None,
        }
    }

    /// "Review and approve" of a draft: the worker reads the stored contract.
    pub fn reading(outcome: &str) -> Self {
        Self {
            mode: Mode::Draft {
                outcome: outcome.to_string(),
            },
            prev: None,
            initial: FormText::default(),
            head: None,
            stage: Stage::Sending,
            read_only: false,
            errors: vec![],
            sent: None,
        }
    }

    pub fn is_new(&self) -> bool {
        self.mode == Mode::New
    }

    /// The worker is asked for `HEAD` of `ws`.
    pub fn resolving(&mut self, ws: &str) {
        self.head = Some(Head::Resolving(ws.to_string()));
    }

    /// The worker's answer; one for a folder that is no longer the one asked about is dropped.
    pub fn head_known(&mut self, ws: &str, r: Result<String, String>) {
        if matches!(&self.head, Some(Head::Resolving(w)) if w == ws) {
            let ws = ws.to_string();
            self.head = Some(match r {
                Ok(commit) => Head::Known { ws, commit },
                Err(why) => Head::Failed { ws, why },
            });
        }
    }

    /// The folder `t` names has a resolved commit (or says why not): `None` when it is already known or asked.
    pub fn needs_head(&self, t: &FormText) -> Option<String> {
        let ws = t.workspace.trim();
        let known = match &self.head {
            Some(Head::Resolving(w)) => w,
            Some(Head::Known { ws, .. } | Head::Failed { ws, .. }) => ws,
            None => "",
        };
        (self.is_new() && !self.read_only && !ws.is_empty() && known != ws).then(|| ws.to_string())
    }

    /// Edit stage only: the request for the text, or the errors (inline, text kept). A request equal to
    /// one whose answer was lost goes out with the same key; a changed one is a new request with a new key.
    pub fn submit(&mut self, t: &FormText) -> Option<Submit> {
        if self.stage != Stage::Edit || matches!(self.mode, Mode::Draft { .. }) {
            return None;
        }
        let mut errs = vec![];
        let built = if self.read_only {
            build_read_only(t)
        } else {
            build(t, self.prev.as_ref())
        };
        let contract = match built {
            Ok(c) => Some(c),
            Err(e) => {
                errs = e;
                None
            }
        };
        let ws = t.workspace.trim().to_string();
        let base = match (&self.mode, &self.head) {
            (Mode::New, _) if self.read_only => {
                if !std::path::Path::new(&ws).is_absolute() {
                    errs.push("Folder: give the absolute path of the folder.".into());
                }
                None
            }
            (Mode::Revise { .. } | Mode::Draft { .. }, _) => None,
            (Mode::New, _) if !std::path::Path::new(&ws).is_absolute() => {
                errs.push("Workspace: give the absolute path of the repository's folder.".into());
                None
            }
            (Mode::New, Some(Head::Known { ws: w, commit })) if *w == ws => Some(commit.clone()),
            (Mode::New, Some(Head::Failed { ws: w, why })) if *w == ws => {
                errs.push(format!("Base commit: {why}"));
                None
            }
            (Mode::New, _) => {
                errs.push(
                    "Base commit: it is not resolved for this folder yet; wait a moment (or Tab out of the Workspace field) and confirm again."
                        .into(),
                );
                None
            }
        };
        let (Some(contract), true) = (contract, errs.is_empty()) else {
            self.errors = errs;
            return None;
        };
        let contract = Box::new(contract);
        let key = |sent: &Option<Submit>, same: &dyn Fn(&Submit) -> bool, prefix: &str| match sent {
            Some(s @ (Submit::Create { key, .. } | Submit::Revise { key, .. })) if same(s) => {
                key.clone()
            }
            _ => format!("{prefix}-{}", uuid::Uuid::new_v4()),
        };
        let req = match &self.mode {
            Mode::New => {
                let base_commit = base.unwrap_or_default();
                let same = |s: &Submit| matches!(s, Submit::Create { workspace, base_commit: b, contract: c, .. } if *workspace == ws && *b == base_commit && *c == contract);
                Submit::Create {
                    key: key(&self.sent, &same, "create"),
                    workspace: ws.clone(),
                    base_commit,
                    contract,
                }
            }
            Mode::Draft { .. } => return None,
            Mode::Revise { outcome, revision } => {
                let same = |s: &Submit| matches!(s, Submit::Revise { outcome: o, revision: r, contract: c, .. } if o == outcome && r == revision && *c == contract);
                Submit::Revise {
                    key: key(&self.sent, &same, "revise"),
                    outcome: outcome.clone(),
                    revision: *revision,
                    contract,
                }
            }
        };
        self.sent = Some(req.clone());
        self.errors.clear();
        self.stage = Stage::Sending;
        Some(req)
    }

    /// A refusal or a failure of the request in flight: back to the stage it came from, text kept. `spent`:
    /// the server answered (the key is used up); otherwise the answer is lost and a retry reuses the key.
    pub fn failed(&mut self, text: String, spent: bool) {
        self.stage = match std::mem::replace(&mut self.stage, Stage::Edit) {
            Stage::Approving(mut r) => {
                if spent {
                    r.key = format!("approve-{}", uuid::Uuid::new_v4());
                }
                Stage::Review(r)
            }
            Stage::Review(r) => Stage::Review(r),
            _ => Stage::Edit,
        };
        if spent {
            self.sent = None;
        }
        self.errors = vec![text];
    }

    /// The commit moved between what the form showed and the moment of sending: nothing was created, the new
    /// base is shown and the person confirms again.
    pub fn base_moved(&mut self, ws: &str, commit: String, text: String) {
        self.head = Some(Head::Known {
            ws: ws.to_string(),
            commit,
        });
        self.sent = None;
        self.stage = Stage::Edit;
        self.errors = vec![text];
    }

    /// A draft whose read failed: ask again (`Some(outcome)`).
    pub fn retry_read(&mut self) -> Option<String> {
        let (Mode::Draft { outcome }, Stage::Edit) = (&self.mode, &self.stage) else {
            return None;
        };
        let outcome = outcome.clone();
        self.stage = Stage::Sending;
        self.errors.clear();
        Some(outcome)
    }

    /// The server stored the contract; the person reads it before approving.
    pub fn reviewing(&mut self, r: Review) {
        self.sent = None;
        self.errors.clear();
        self.stage = Stage::Review(r);
    }

    /// Review stage: `(outcome, version, hash, key)` to approve, when it can be.
    pub fn approve(&mut self) -> Option<(String, u32, String, String)> {
        let Stage::Review(r) = &self.stage else {
            return None;
        };
        if !r.approvable() {
            return None;
        }
        let out = (r.outcome.clone(), r.version, r.hash.clone(), r.key.clone());
        self.errors.clear();
        if let Stage::Review(r) = std::mem::replace(&mut self.stage, Stage::Edit) {
            self.stage = Stage::Approving(r);
        }
        Some(out)
    }
}

#[cfg(test)]
mod tests;
