//! Plan, criteria, attempt, evidence and budget view models and the inspector tab content
//! (ADR-021 section 2 and 3). Pure data built from protocol v4 replies: nothing here reads a
//! file, computes a state, a verdict, a result or a remaining budget, or trusts the engine. A
//! value the server did not send is `unknown` (never a guess); a field this build does not know is
//! kept as an extra row. Every string is made printable and bounded.
//!
//! The only derived thing is the layer of a work item in the DAG, laid out from the server's own
//! `requires` edges (presentation, like a sort); it says nothing about what may run.

use std::collections::HashMap;

use serde_json::{Map, Value};

use super::{BriefRow, InspectorTab, OutcomeRow, brief_rows, clean, valid_alt_id};

/// The store's list limit: a longer list is not a plan this build shows.
pub const MAX_ROWS: usize = 1000;
const MAX_TEXT: usize = 4096;
const UNKNOWN: &str = "unknown";

/// Printable and bounded.
fn t(s: &str) -> String {
    let c = clean(s);
    match c.char_indices().nth(MAX_TEXT) {
        Some((i, _)) => format!("{}[truncated]", &c[..i]),
        None => c,
    }
}

fn opt(v: &Value, k: &str) -> Option<String> {
    v.get(k)?.as_str().map(t).filter(|s| !s.is_empty())
}

fn req(v: &Value, k: &str) -> Result<String, String> {
    opt(v, k).ok_or_else(|| format!("unreadable reply: no {k}"))
}

/// Ids are plain ASCII like answer ids: a bad one refuses the reply, it is not cleaned.
fn id(v: &Value, k: &str) -> Result<String, String> {
    let s = v
        .get(k)
        .and_then(Value::as_str)
        .ok_or_else(|| format!("unreadable reply: no {k}"))?;
    if valid_alt_id(s) {
        Ok(s.into())
    } else {
        Err(format!(
            "unreadable reply: {k} is not 1 to 64 of A-Z a-z 0-9 _ . -: {}",
            t(s)
        ))
    }
}

fn strings(v: &Value, k: &str) -> Vec<String> {
    v.get(k)
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .take(MAX_ROWS)
        .filter_map(Value::as_str)
        .map(t)
        .collect()
}

/// The fields of `v` this build has no place for, shown as rows (hostile or future data stays visible).
fn extra(v: &Value, known: &[&str]) -> Vec<BriefRow> {
    let o: Map<String, Value> = v
        .as_object()
        .into_iter()
        .flatten()
        .filter(|(k, _)| !known.contains(&k.as_str()))
        .map(|(k, x)| (k.clone(), x.clone()))
        .collect();
    brief_rows(&Value::Object(o))
}

fn rows_in(v: &Value, k: &str) -> Result<Vec<Value>, String> {
    let a = v
        .get(k)
        .and_then(Value::as_array)
        .ok_or_else(|| format!("unreadable reply: no {k} list"))?;
    if a.len() > MAX_ROWS {
        return Err(format!("{k}: more than {MAX_ROWS} rows, not shown"));
    }
    Ok(a.clone())
}

fn shown<T: ToString>(x: Option<T>) -> String {
    x.map_or_else(|| UNKNOWN.into(), |x| x.to_string())
}

fn push_extra(out: &mut String, rows: &[BriefRow]) {
    for r in rows {
        out.push_str(&format!("{}: {}\n", r.label, r.text));
    }
}

// ------------------------------------------------------------------------------------- plan

#[derive(Clone, Debug, PartialEq)]
pub struct WorkItemView {
    pub id: String,
    pub key: String,
    pub kind: String,
    /// As the server states it.
    pub state: String,
    pub revision: Option<i64>,
    pub retries_used: Option<i64>,
    /// Keys of the items this one requires, as sent.
    pub requires: Vec<String>,
    /// 0 for an item with no known requirement; `None` when the plan has a cycle.
    pub layer: Option<usize>,
    pub extra: Vec<BriefRow>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct PlanView {
    pub plan_version: Option<i64>,
    /// The server's order.
    pub items: Vec<WorkItemView>,
    /// The plan has a dependency cycle: no layers, shown as the server sent it.
    pub cycle: bool,
    /// Requirements that name no item of this plan (kept visible, not followed).
    pub unknown_requires: Vec<String>,
    pub extra: Vec<BriefRow>,
}

impl WorkItemView {
    /// `item_json` of the protocol (also the head of a `work_item.get` reply).
    pub fn from_json(v: &Value) -> Result<Self, String> {
        Ok(Self {
            id: id(v, "id")?,
            key: opt(v, "key").unwrap_or_default(),
            kind: opt(v, "kind").unwrap_or_else(|| UNKNOWN.into()),
            state: req(v, "state")?,
            revision: v.get("revision").and_then(Value::as_i64),
            retries_used: v.get("retries_used").and_then(Value::as_i64),
            requires: strings(v, "requires"),
            layer: None,
            extra: extra(
                v,
                &[
                    "id",
                    "key",
                    "kind",
                    "state",
                    "revision",
                    "retries_used",
                    "requires",
                    "attempts",
                ],
            ),
        })
    }
}

impl PlanView {
    /// `Err` (fail closed) for a reply that is not a list, a bad or duplicate id or key, or too many items.
    pub fn from_json(v: &Value) -> Result<Self, String> {
        let mut items = rows_in(v, "items")?
            .iter()
            .map(WorkItemView::from_json)
            .collect::<Result<Vec<_>, _>>()?;
        let mut ids = HashMap::new();
        let mut keys = HashMap::new();
        for (i, w) in items.iter().enumerate() {
            if ids.insert(w.id.as_str(), i).is_some() {
                return Err(format!("the plan lists work item {} twice", w.id));
            }
            if !w.key.is_empty() && keys.insert(w.key.as_str(), i).is_some() {
                return Err(format!("the plan lists key {} twice", w.key));
            }
        }
        let mut unknown_requires = vec![];
        let deps: Vec<Vec<usize>> = items
            .iter()
            .map(|w| {
                w.requires
                    .iter()
                    .filter_map(|r| {
                        let d = keys.get(r.as_str()).copied();
                        if d.is_none() {
                            unknown_requires.push(format!("{} requires {r}", w.key));
                        }
                        d
                    })
                    .collect()
            })
            .collect();
        // layer = 1 + the deepest known requirement; a round that places nothing means a cycle
        let mut layer: Vec<Option<usize>> = vec![None; items.len()];
        loop {
            let mut moved = false;
            for i in 0..items.len() {
                if layer[i].is_none() && deps[i].iter().all(|d| layer[*d].is_some()) {
                    layer[i] = Some(
                        deps[i]
                            .iter()
                            .filter_map(|d| layer[*d])
                            .max()
                            .map_or(0, |m| m + 1),
                    );
                    moved = true;
                }
            }
            if !moved {
                break;
            }
        }
        let cycle = layer.iter().any(Option::is_none);
        for (w, l) in items.iter_mut().zip(layer) {
            w.layer = if cycle { None } else { l };
        }
        Ok(Self {
            plan_version: v.get("plan_version").and_then(Value::as_i64),
            items,
            cycle,
            unknown_requires,
            extra: extra(v, &["plan_version", "items"]),
        })
    }

    /// Items grouped by layer, the server's order inside a layer; empty when there is a cycle.
    pub fn layers(&self) -> Vec<Vec<&WorkItemView>> {
        let mut out: Vec<Vec<&WorkItemView>> = vec![];
        for w in &self.items {
            if let Some(l) = w.layer {
                if out.len() <= l {
                    out.resize_with(l + 1, Vec::new);
                }
                out[l].push(w);
            }
        }
        out
    }

    pub fn render_text(&self) -> String {
        let mut out = format!("Plan version {}\n", shown(self.plan_version));
        if self.cycle {
            out.push_str("dependency cycle: layers not shown\n");
        }
        for w in &self.items {
            out.push_str(&format!(
                "{} {} [{}] {} revision {} retries used {} layer {} requires {}\n",
                w.id,
                w.key,
                w.kind,
                w.state,
                shown(w.revision),
                shown(w.retries_used),
                shown(w.layer),
                w.requires.join(", ")
            ));
            push_extra(&mut out, &w.extra);
        }
        for u in &self.unknown_requires {
            out.push_str(&format!("unknown requirement: {u}\n"));
        }
        push_extra(&mut out, &self.extra);
        out
    }
}

// -------------------------------------------------------------------------------- criteria

#[derive(Clone, Debug, PartialEq)]
pub struct CriterionView {
    pub id: String,
    pub text: String,
    pub mandatory: Option<bool>,
    pub manual: Option<bool>,
    pub required_trust: String,
    pub check_ids: Vec<String>,
    pub extra: Vec<BriefRow>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct CriteriaView {
    pub version: Option<i64>,
    pub hash: String,
    pub approved: Option<bool>,
    pub criteria: Vec<CriterionView>,
    /// `id: command` of the contract's frozen checks.
    pub checks: Vec<String>,
    pub extra: Vec<BriefRow>,
}

impl CriteriaView {
    /// From an `outcome.contract.get` reply. The criteria are the contract's, not a status.
    pub fn from_json(v: &Value) -> Result<Self, String> {
        let c = v
            .get("contract")
            .filter(|c| c.is_object())
            .ok_or("unreadable reply: no contract")?;
        let mut seen = vec![];
        let mut criteria = vec![];
        for x in rows_in(c, "criteria")? {
            let cid = id(&x, "id")?;
            if seen.contains(&cid) {
                return Err(format!("the contract lists criterion {cid} twice"));
            }
            seen.push(cid.clone());
            criteria.push(CriterionView {
                id: cid,
                text: opt(&x, "text").unwrap_or_default(),
                mandatory: x.get("mandatory").and_then(Value::as_bool),
                manual: x.get("manual").and_then(Value::as_bool),
                required_trust: opt(&x, "required_trust").unwrap_or_else(|| UNKNOWN.into()),
                check_ids: strings(&x, "check_ids"),
                extra: extra(
                    &x,
                    &[
                        "id",
                        "text",
                        "mandatory",
                        "manual",
                        "required_trust",
                        "check_ids",
                    ],
                ),
            });
        }
        let checks = c
            .get("checks")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .take(MAX_ROWS)
            .map(|k| {
                format!(
                    "{}: {}",
                    opt(k, "id").unwrap_or_default(),
                    opt(k, "command").unwrap_or_default()
                )
            })
            .collect();
        let mut rest = extra(c, &["version", "goal", "criteria", "checks"]);
        rest.extend(extra(v, &["version", "hash", "approved", "contract"]));
        Ok(Self {
            version: v.get("version").and_then(Value::as_i64),
            hash: opt(v, "hash").unwrap_or_default(),
            approved: v.get("approved").and_then(Value::as_bool),
            criteria,
            checks,
            extra: rest,
        })
    }

    pub fn render_text(&self) -> String {
        let mut out = format!(
            "Contract version {} hash {} approved {}\n",
            shown(self.version),
            self.hash,
            shown(self.approved)
        );
        for c in &self.criteria {
            out.push_str(&format!(
                "{} {} mandatory {} manual {} trust {} checks {}\n",
                c.id,
                c.text,
                shown(c.mandatory),
                shown(c.manual),
                c.required_trust,
                c.check_ids.join(", ")
            ));
            push_extra(&mut out, &c.extra);
        }
        for k in &self.checks {
            out.push_str(&format!("check {k}\n"));
        }
        push_extra(&mut out, &self.extra);
        out
    }
}

// ---------------------------------------------------------------------------------- attempts

/// What the protocol says about the sandbox of an attempt; `unknown` unless it says (it does not today).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SandboxView {
    pub backend: String,
    pub guarantee: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct AttemptView {
    pub id: String,
    pub work_item_id: String,
    pub ordinal: Option<i64>,
    pub session_id: String,
    pub state: String,
    pub task_status: String,
    pub subject_commit: String,
    pub retained_reason: String,
    pub started_at: String,
    pub ended_at: String,
    pub sandbox: SandboxView,
    pub extra: Vec<BriefRow>,
}

impl AttemptView {
    /// One attempt object; `work_item_id` is the item that holds it (the reply's own field when it has one).
    pub fn from_json(a: &Value, work_item_id: &str) -> Result<Self, String> {
        let sb = a.get("sandbox");
        let sbt = |k| sb.and_then(|s| opt(s, k)).unwrap_or_else(|| UNKNOWN.into());
        let s = |k| opt(a, k).unwrap_or_default();
        Ok(Self {
            id: id(a, "id")?,
            work_item_id: opt(a, "work_item_id").unwrap_or_else(|| t(work_item_id)),
            ordinal: a.get("ordinal").and_then(Value::as_i64),
            session_id: s("session_id"),
            state: req(a, "state")?,
            task_status: s("task_status"),
            subject_commit: s("subject_commit"),
            retained_reason: s("retained_reason"),
            started_at: s("started_at"),
            ended_at: s("ended_at"),
            sandbox: SandboxView {
                backend: sbt("backend"),
                guarantee: sbt("guarantee"),
            },
            extra: extra(
                a,
                &[
                    "id",
                    "work_item_id",
                    "ordinal",
                    "session_id",
                    "state",
                    "task_status",
                    "subject_commit",
                    "retained_reason",
                    "started_at",
                    "ended_at",
                    "sandbox",
                ],
            ),
        })
    }

    /// An `attempt.inspect` reply: `{work_item_id, attempt}`.
    pub fn from_inspect(v: &Value) -> Result<Self, String> {
        let a = v.get("attempt").ok_or("unreadable reply: no attempt")?;
        Self::from_json(a, &id(v, "work_item_id")?)
    }

    /// The attempts of a `work_item.get` reply.
    pub fn list_from_work_item(v: &Value) -> Result<Vec<Self>, String> {
        let item = id(v, "id")?;
        rows_in(v, "attempts")?
            .iter()
            .map(|a| Self::from_json(a, &item))
            .collect()
    }

    pub fn render_text(&self) -> String {
        let mut out = format!(
            "Attempt {} of {} ordinal {} {} task {} session {} commit {} retained {} started {} ended {} sandbox {} guarantee {}\n",
            self.id,
            self.work_item_id,
            shown(self.ordinal),
            self.state,
            self.task_status,
            self.session_id,
            self.subject_commit,
            self.retained_reason,
            self.started_at,
            self.ended_at,
            self.sandbox.backend,
            self.sandbox.guarantee
        );
        push_extra(&mut out, &self.extra);
        out
    }
}

// ----------------------------------------------------------------------------------- evidence

#[derive(Clone, Debug, PartialEq)]
pub struct EvidenceView {
    pub id: String,
    pub work_item_id: String,
    pub trust_class: String,
    pub producer: String,
    pub subject_commit: String,
    pub check_id: String,
    pub criteria_ids: Vec<String>,
    /// As recorded; the surface never turns it into pass or fail.
    pub result: String,
    pub exit_code: Option<i64>,
    /// Disqualifying flags written with the row.
    pub flags: Vec<String>,
    pub captured_at: String,
    /// The server's `event_backed`, when it sends one (protocol v4 does not: `None` is shown as unknown).
    pub event_backed: Option<bool>,
    pub extra: Vec<BriefRow>,
}

impl EvidenceView {
    pub fn from_json(v: &Value) -> Result<Self, String> {
        let s = |k| opt(v, k).unwrap_or_default();
        Ok(Self {
            id: id(v, "id")?,
            work_item_id: s("work_item_id"),
            trust_class: opt(v, "trust_class").unwrap_or_else(|| UNKNOWN.into()),
            producer: s("producer"),
            subject_commit: s("subject_commit"),
            check_id: s("check_id"),
            criteria_ids: strings(v, "criteria_ids"),
            result: req(v, "result")?,
            exit_code: v.get("exit_code").and_then(Value::as_i64),
            flags: strings(v, "flags"),
            captured_at: s("captured_at"),
            event_backed: v.get("event_backed").and_then(Value::as_bool),
            extra: extra(
                v,
                &[
                    "id",
                    "work_item_id",
                    "trust_class",
                    "producer",
                    "subject_commit",
                    "check_id",
                    "criteria_ids",
                    "result",
                    "exit_code",
                    "flags",
                    "captured_at",
                    "event_backed",
                ],
            ),
        })
    }

    pub fn render_text(&self) -> String {
        let mut out = format!(
            "Evidence {} of {} {} by {} commit {} check {} criteria {} result {} exit {} flags {} at {} event-backed {}\n",
            self.id,
            self.work_item_id,
            self.trust_class,
            self.producer,
            self.subject_commit,
            self.check_id,
            self.criteria_ids.join(", "),
            self.result,
            shown(self.exit_code),
            self.flags.join(", "),
            self.captured_at,
            shown(self.event_backed)
        );
        push_extra(&mut out, &self.extra);
        out
    }
}

// -------------------------------------------------------------------------------------- budget

/// One measure exactly as the server gave it; a number it did not give (or a negative or
/// non-finite one) is `None`. `remaining` is never `limit - spent` here.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Amount {
    pub spent: Option<f64>,
    pub limit: Option<f64>,
    pub remaining: Option<f64>,
}

impl Amount {
    fn from_json(v: Option<&Value>) -> Self {
        let n = |k| {
            v.and_then(|v| v.get(k))
                .and_then(Value::as_f64)
                .filter(|x| x.is_finite() && *x >= 0.0)
        };
        Self {
            spent: n("spent"),
            limit: n("limit"),
            remaining: n("remaining"),
        }
    }

    fn line(&self, name: &str) -> String {
        format!(
            "{name} spent {} limit {} remaining {}\n",
            shown(self.spent),
            shown(self.limit),
            shown(self.remaining)
        )
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct BudgetView {
    pub tokens: Amount,
    pub cost_usd: Amount,
    pub wall_secs: Amount,
}

impl BudgetView {
    /// From `{tokens, cost_usd, wall_secs}`, each `{spent, limit, remaining}` (no protocol read sends
    /// this yet: ADR-021 notes). Anything absent is unknown.
    pub fn from_json(v: &Value) -> Self {
        Self {
            tokens: Amount::from_json(v.get("tokens")),
            cost_usd: Amount::from_json(v.get("cost_usd")),
            wall_secs: Amount::from_json(v.get("wall_secs")),
        }
    }

    /// The limits the contract states (`envelope.budgets`); nothing is spent or remaining as far as this knows.
    pub fn from_contract(reply: &Value) -> Self {
        let b = reply.pointer("/contract/envelope/budgets");
        let n = |k| {
            b.and_then(|b| b.get(k))
                .and_then(Value::as_f64)
                .filter(|x| x.is_finite() && *x >= 0.0)
        };
        Self {
            cost_usd: Amount {
                limit: n("max_cost_usd"),
                ..Amount::default()
            },
            wall_secs: Amount {
                limit: n("attempt_max_wall_secs"),
                ..Amount::default()
            },
            ..Self::default()
        }
    }

    pub fn render_text(&self) -> String {
        [
            self.tokens.line("tokens"),
            self.cost_usd.line("cost_usd"),
            self.wall_secs.line("wall_secs"),
        ]
        .concat()
    }
}

// ------------------------------------------------------------------------------ inspector tabs

#[derive(Clone, Debug, PartialEq)]
pub struct OverviewTab {
    pub outcome: OutcomeRow,
    pub criteria: CriteriaView,
    pub budget: BudgetView,
}

#[derive(Clone, Debug, PartialEq)]
pub struct WorkersTab {
    pub plan: PlanView,
    pub attempts: Vec<AttemptView>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ActivityTab {
    /// `seq  type  event`, printable.
    pub lines: Vec<String>,
    pub next_event_id: Option<u64>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct EvidenceTab {
    pub rows: Vec<EvidenceView>,
}

/// What one inspector tab shows. `Unavailable` says why (no method, a refused or unreadable
/// reply, the feature off); a tab never shows an empty page in place of an error.
#[derive(Clone, Debug, PartialEq)]
pub enum InspectorContent {
    Overview(Box<OverviewTab>),
    Workers(WorkersTab),
    Activity(ActivityTab),
    Evidence(EvidenceTab),
    Unavailable { tab: InspectorTab, reason: String },
}

pub const DIFF_UNAVAILABLE: &str = "protocol v4 has no diff method; the attempt's branch and session are inspectable with git and `synt sessions show`";

impl InspectorContent {
    pub fn unavailable(tab: InspectorTab, reason: &str) -> Self {
        Self::Unavailable {
            tab,
            reason: t(reason),
        }
    }

    /// The Diff tab: there is no server data for it.
    pub fn diff() -> Self {
        Self::unavailable(InspectorTab::Diff, DIFF_UNAVAILABLE)
    }

    /// A failed build or call becomes `Unavailable` with the error as the reason.
    pub fn or_unavailable(tab: InspectorTab, r: Result<Self, String>) -> Self {
        r.unwrap_or_else(|e| Self::unavailable(tab, &e))
    }

    pub fn tab(&self) -> InspectorTab {
        match self {
            Self::Overview(_) => InspectorTab::Overview,
            Self::Workers(_) => InspectorTab::Workers,
            Self::Activity(_) => InspectorTab::Activity,
            Self::Evidence(_) => InspectorTab::Evidence,
            Self::Unavailable { tab, .. } => *tab,
        }
    }

    /// `outcome` is an `outcome.get` reply, `contract` an `outcome.contract.get` reply.
    pub fn overview(outcome: &Value, contract: &Value) -> Result<Self, String> {
        Ok(Self::Overview(Box::new(OverviewTab {
            outcome: OutcomeRow::from_json(outcome).ok_or("unreadable outcome")?,
            criteria: CriteriaView::from_json(contract)?,
            budget: BudgetView::from_contract(contract),
        })))
    }

    /// `plan` is a `plan.get` reply, `work_items` the `work_item.get` replies of its items.
    pub fn workers(plan: &Value, work_items: &[Value]) -> Result<Self, String> {
        let mut attempts = vec![];
        for w in work_items {
            attempts.extend(AttemptView::list_from_work_item(w)?);
        }
        Ok(Self::Workers(WorkersTab {
            plan: PlanView::from_json(plan)?,
            attempts,
        }))
    }

    pub fn activity(events: &[Value], next_event_id: u64) -> Self {
        let n = |e: &Value, k: &str| {
            e.get(k).map_or_else(
                || UNKNOWN.into(),
                |x| t(&x.as_str().map_or_else(|| x.to_string(), str::to_string)),
            )
        };
        Self::Activity(ActivityTab {
            lines: events
                .iter()
                .take(MAX_ROWS)
                .map(|e| format!("{}  {}  {}", n(e, "seq"), n(e, "type"), t(&e.to_string())))
                .collect(),
            next_event_id: Some(next_event_id),
        })
    }

    /// `rows` are the `evidence` list of an `evidence.get` reply.
    pub fn evidence(rows: &[Value]) -> Result<Self, String> {
        if rows.len() > MAX_ROWS {
            return Err(format!("more than {MAX_ROWS} evidence rows, not shown"));
        }
        Ok(Self::Evidence(EvidenceTab {
            rows: rows
                .iter()
                .map(EvidenceView::from_json)
                .collect::<Result<_, _>>()?,
        }))
    }

    pub fn render_text(&self) -> String {
        let mut out = format!("[{}]\n", self.tab().label());
        match self {
            Self::Overview(o) => {
                out.push_str(&format!(
                    "{}: {} r{} {}\n",
                    o.outcome.id, o.outcome.state, o.outcome.revision, o.outcome.goal
                ));
                out.push_str(&o.criteria.render_text());
                out.push_str(&o.budget.render_text());
            }
            Self::Workers(w) => {
                out.push_str(&w.plan.render_text());
                w.attempts
                    .iter()
                    .for_each(|a| out.push_str(&a.render_text()));
            }
            Self::Activity(a) => {
                a.lines.iter().for_each(|l| out.push_str(&format!("{l}\n")));
            }
            Self::Evidence(e) => e.rows.iter().for_each(|r| out.push_str(&r.render_text())),
            Self::Unavailable { reason, .. } => out.push_str(&format!("not available: {reason}\n")),
        }
        out
    }
}

#[cfg(test)]
mod tests;
