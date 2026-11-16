//! Plan, criteria, attempt, evidence and budget view models and the inspector tab content
//! (ADR-021 section 2 and 3). Pure data built from protocol v4 replies: nothing here reads a
//! file, computes a state, a verdict, a result or a remaining budget, or trusts the engine. A
//! value the server did not send is `unknown` (never a guess); a field this build does not know is
//! kept as an extra row. Every string is made printable and bounded.
//!
//! The only derived thing is the layer of a work item in the DAG, laid out from the server's own
//! `requires` edges (presentation, like a sort); it says nothing about what may run.

use std::collections::HashMap;

use serde_json::Value;

use super::{BriefRow, InspectorTab, OutcomeRow, clean, empty, flat, valid_alt_id};

/// The store's list limit: a longer list is not a plan this build shows.
pub const MAX_ROWS: usize = 1000;
const MAX_TEXT: usize = 4096;
/// Work item keys are short names; the plan as a whole has at most this many requirement edges.
const MAX_KEY: usize = 256;
const MAX_EDGES: usize = 10_000;
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

/// A known field of the wrong type refuses the reply: it is never shown as empty or unknown,
/// which would read like "not sent". Absent and null are fine.
fn typed(v: &Value, texts: &[&str], ints: &[&str], bools: &[&str]) -> Result<(), String> {
    type Check<'a> = (&'a [&'a str], &'a str, fn(&Value) -> bool);
    let groups: [Check; 3] = [
        (texts, "text", Value::is_string),
        (ints, "a whole number", Value::is_i64),
        (bools, "true or false", Value::is_boolean),
    ];
    for (keys, want, ok) in groups {
        if let Some(k) = keys
            .iter()
            .find(|k| v.get(**k).is_some_and(|x| !x.is_null() && !ok(x)))
        {
            return Err(format!(
                "unreadable reply: the server sent {k} that is not {want}"
            ));
        }
    }
    Ok(())
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

/// A list of strings. Fail closed: too many entries or an entry that is not a string refuses the
/// reply, so a hidden entry (a requirement, a flag) is never silently left out.
fn strings(v: &Value, k: &str) -> Result<Vec<String>, String> {
    match v.get(k) {
        None | Some(Value::Null) => Ok(vec![]),
        Some(Value::Array(a)) if a.len() <= MAX_ROWS => a
            .iter()
            .map(|x| {
                x.as_str()
                    .map(t)
                    .ok_or_else(|| format!("unreadable reply: {k} has an entry that is not text"))
            })
            .collect(),
        Some(Value::Array(_)) => Err(format!("{k}: more than {MAX_ROWS} entries, not shown")),
        Some(_) => Err(format!("unreadable reply: {k} is not a list")),
    }
}

/// Unknown fields shown at most this many per object.
const MAX_EXTRA: usize = 100;
/// Keys that would pick an option for a person: shown, flagged, never acted on.
const PRESELECT_LIKE: [&str; 5] = [
    "selected",
    "recommended",
    "default",
    "default_choice",
    "preselected",
];

/// The fields of `v` this build has no place for, shown as rows (hostile or future data stays
/// visible). The label is the key itself, marked as the server's extra: an unknown key never
/// borrows the heading of a known one. Rows and text are bounded.
fn extra(v: &Value, known: &[&str]) -> Vec<BriefRow> {
    let rest: Vec<_> = v
        .as_object()
        .into_iter()
        .flatten()
        .filter(|(k, x)| !known.contains(&k.as_str()) && !empty(x))
        .collect();
    let mut rows: Vec<BriefRow> = rest
        .iter()
        .take(MAX_EXTRA)
        .map(|(k, x)| {
            let mut lines = vec![];
            flat(x, &mut lines);
            let flag = if PRESELECT_LIKE.contains(&k.as_str()) {
                " (names a preferred option: ignored)"
            } else {
                ""
            };
            BriefRow {
                label: format!("server extra{flag}: {}", t(k)),
                text: t(&lines.join("\n")),
            }
        })
        .collect();
    if rest.len() > MAX_EXTRA {
        rows.push(BriefRow {
            label: "server extra".into(),
            text: format!("{} more fields not shown", rest.len() - MAX_EXTRA),
        });
    }
    rows
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
    /// 0 for an item with no requirement; `None` (shown "unknown") when the plan has a cycle or
    /// the item, or one it depends on, has a requirement outside the plan.
    pub layer: Option<usize>,
    /// How many attempts the reply lists, when it lists any (they are shown by the Workers tab).
    pub attempts_listed: Option<usize>,
    pub extra: Vec<BriefRow>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct PlanView {
    pub plan_version: Option<i64>,
    /// The server's order.
    pub items: Vec<WorkItemView>,
    /// The plan has a dependency cycle: no layers, shown as the server sent it.
    pub cycle: bool,
    /// Requirements that name no item of this plan (kept visible, not followed): the index of the
    /// item and the requirement as sent. At most `MAX_ROWS`; `unknown_requires_more` counts the rest.
    pub unknown_requires: Vec<(usize, String)>,
    pub unknown_requires_more: usize,
    pub extra: Vec<BriefRow>,
}

impl WorkItemView {
    /// `item_json` of the protocol (also the head of a `work_item.get` reply).
    pub fn from_json(v: &Value) -> Result<Self, String> {
        typed(
            v,
            &["id", "key", "kind", "state"],
            &["revision", "retries_used"],
            &[],
        )?;
        // a key is matched whole: a long or unprintable one is refused, not cut or cleaned
        // (cleaning could make an edge the server never sent)
        for k in v
            .get("requires")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .chain(v.get("key"))
        {
            if k.as_str().is_some_and(|k| k.len() > MAX_KEY || t(k) != k) {
                return Err(format!(
                    "a key or requirement is longer than {MAX_KEY} bytes or has control characters"
                ));
            }
        }
        Ok(Self {
            id: id(v, "id")?,
            key: opt(v, "key").unwrap_or_default(),
            kind: opt(v, "kind").unwrap_or_else(|| UNKNOWN.into()),
            state: req(v, "state")?,
            revision: v.get("revision").and_then(Value::as_i64),
            retries_used: v.get("retries_used").and_then(Value::as_i64),
            requires: strings(v, "requires")?,
            layer: None,
            attempts_listed: v.get("attempts").and_then(Value::as_array).map(Vec::len),
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
        typed(v, &[], &["plan_version"], &[])?;
        let raw = rows_in(v, "items")?;
        let edges: usize = raw
            .iter()
            .filter_map(|w| w.get("requires")?.as_array())
            .map(Vec::len)
            .sum();
        if edges > MAX_EDGES {
            return Err(format!(
                "the plan has more than {MAX_EDGES} requirements, not shown"
            ));
        }
        let mut items = raw
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
        // Kahn: O(items + edges). The layer is 1 + the deepest requirement; items left over are a cycle.
        let n = items.len();
        let mut unknown_requires = vec![];
        let mut unknown_requires_more = 0;
        let mut dependents: Vec<Vec<usize>> = vec![vec![]; n];
        let mut waiting = vec![0usize; n];
        let mut unsure = vec![false; n];
        for (i, w) in items.iter().enumerate() {
            for r in &w.requires {
                match keys.get(r.as_str()) {
                    Some(&d) => {
                        dependents[d].push(i);
                        waiting[i] += 1;
                    }
                    None => {
                        unsure[i] = true;
                        if unknown_requires.len() < MAX_ROWS {
                            unknown_requires.push((i, r.clone()));
                        } else {
                            unknown_requires_more += 1;
                        }
                    }
                }
            }
        }
        let mut layer = vec![0usize; n];
        let mut ready: Vec<usize> = (0..n).filter(|i| waiting[*i] == 0).collect();
        let mut placed = 0;
        while let Some(d) = ready.pop() {
            placed += 1;
            for &i in &dependents[d] {
                layer[i] = layer[i].max(layer[d] + 1);
                unsure[i] |= unsure[d];
                waiting[i] -= 1;
                if waiting[i] == 0 {
                    ready.push(i);
                }
            }
        }
        let cycle = placed < n;
        for (i, w) in items.iter_mut().enumerate() {
            w.layer = (!cycle && !unsure[i]).then_some(layer[i]);
        }
        Ok(Self {
            plan_version: v.get("plan_version").and_then(Value::as_i64),
            items,
            cycle,
            unknown_requires,
            unknown_requires_more,
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
                "{} {} [{}] {} revision {} retries used {} layer {} requires {} attempts {}\n",
                w.id,
                w.key,
                w.kind,
                w.state,
                shown(w.revision),
                shown(w.retries_used),
                w.layer.map_or_else(|| "unknown".into(), |l| l.to_string()),
                w.requires.join(", "),
                shown(w.attempts_listed)
            ));
            push_extra(&mut out, &w.extra);
        }
        for (i, r) in &self.unknown_requires {
            out.push_str(&format!(
                "unknown requirement: {} requires {r}\n",
                self.items[*i].key
            ));
        }
        if self.unknown_requires_more > 0 {
            out.push_str(&format!(
                "{} more unknown requirements not shown\n",
                self.unknown_requires_more
            ));
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
    pub goal: String,
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
            typed(
                &x,
                &["id", "text", "required_trust"],
                &[],
                &["mandatory", "manual"],
            )?;
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
                check_ids: strings(&x, "check_ids")?,
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
        let mut check_extra = vec![];
        let checks = match c.get("checks") {
            None | Some(Value::Null) => vec![],
            Some(_) => rows_in(c, "checks")?
                .iter()
                .map(|k| {
                    typed(k, &["id", "command"], &[], &[])?;
                    let (cid, cmd) = (req(k, "id")?, req(k, "command")?);
                    check_extra.extend(extra(k, &["id", "command"]).into_iter().map(|mut r| {
                        r.label = format!("check {cid}, {}", r.label);
                        r
                    }));
                    Ok(format!("{cid}: {cmd}"))
                })
                .collect::<Result<_, String>>()?,
        };
        typed(c, &["goal"], &[], &[])?;
        typed(v, &["hash"], &["version"], &["approved"])?;
        let mut rest = extra(c, &["version", "goal", "criteria", "checks"]);
        rest.extend(check_extra);
        rest.extend(extra(v, &["version", "hash", "approved", "contract"]));
        Ok(Self {
            goal: opt(c, "goal").unwrap_or_default(),
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
            "Contract version {} hash {} approved {}\nGoal: {}\n",
            shown(self.version),
            self.hash,
            shown(self.approved),
            self.goal
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

/// What the protocol says about the sandbox of an attempt; `unknown` unless it says (the owner records it at launch; older attempts have none).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SandboxView {
    pub source: SandboxSource,
    pub backend: String,
    pub guarantee: String,
    /// Other keys of the `sandbox` object, or the reason it could not be read.
    pub extra: Vec<BriefRow>,
}

/// Where the sandbox data came from. No surface may present it as verified: the engine said it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SandboxSource {
    NotProvided,
    /// The server's word; this surface did not check it.
    ServerReported,
    Unreadable,
}

impl SandboxView {
    fn from_json(sb: Option<&Value>) -> Self {
        let unknown = || UNKNOWN.to_string();
        match sb {
            None | Some(Value::Null) => Self {
                source: SandboxSource::NotProvided,
                backend: unknown(),
                guarantee: unknown(),
                extra: vec![],
            },
            Some(o @ Value::Object(_)) if typed(o, &["backend", "guarantee"], &[], &[]).is_ok() => {
                Self {
                    source: SandboxSource::ServerReported,
                    backend: opt(o, "backend").unwrap_or_else(unknown),
                    guarantee: opt(o, "guarantee").unwrap_or_else(unknown),
                    extra: extra(o, &["backend", "guarantee"]),
                }
            }
            Some(other) => Self {
                source: SandboxSource::Unreadable,
                backend: unknown(),
                guarantee: unknown(),
                extra: vec![BriefRow {
                    label: "sandbox".into(),
                    text: format!("unreadable: not an object: {}", t(&other.to_string())),
                }],
            },
        }
    }

    fn render_text(&self) -> String {
        let head = match self.source {
            SandboxSource::NotProvided => "sandbox: unknown (not provided)".to_string(),
            SandboxSource::Unreadable => "sandbox: unreadable".to_string(),
            SandboxSource::ServerReported => format!(
                "sandbox resolved by the owner at launch (server-reported, not verified by this surface): backend {} guarantee {}",
                self.backend, self.guarantee
            ),
        };
        let mut out = format!("{head}\n");
        push_extra(&mut out, &self.extra);
        out
    }
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
        // the reply says which item holds the attempt; an attempt that names another is refused
        typed(
            a,
            &[
                "id",
                "session_id",
                "state",
                "task_status",
                "subject_commit",
                "retained_reason",
                "started_at",
                "ended_at",
            ],
            &["ordinal"],
            &[],
        )?;
        if a.get("work_item_id")
            .is_some_and(|w| !w.is_null() && w.as_str() != Some(work_item_id))
        {
            return Err("the attempt names a work item other than the one that holds it".into());
        }
        let s = |k| opt(a, k).unwrap_or_default();
        Ok(Self {
            id: id(a, "id")?,
            work_item_id: t(work_item_id),
            ordinal: a.get("ordinal").and_then(Value::as_i64),
            session_id: s("session_id"),
            state: req(a, "state")?,
            task_status: s("task_status"),
            subject_commit: s("subject_commit"),
            retained_reason: s("retained_reason"),
            started_at: s("started_at"),
            ended_at: s("ended_at"),
            sandbox: SandboxView::from_json(a.get("sandbox")),
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
            "Attempt {} of {} ordinal {} {} task {} session {} commit {} retained {} started {} ended {}\n",
            self.id,
            self.work_item_id,
            shown(self.ordinal),
            self.state,
            self.task_status,
            self.session_id,
            self.subject_commit,
            self.retained_reason,
            self.started_at,
            self.ended_at
        );
        out.push_str(&self.sandbox.render_text());
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
        typed(
            v,
            &[
                "id",
                "work_item_id",
                "trust_class",
                "producer",
                "subject_commit",
                "check_id",
                "result",
                "captured_at",
            ],
            &["exit_code"],
            &["event_backed"],
        )?;
        let s = |k| opt(v, k).unwrap_or_default();
        Ok(Self {
            id: id(v, "id")?,
            work_item_id: s("work_item_id"),
            trust_class: opt(v, "trust_class").unwrap_or_else(|| UNKNOWN.into()),
            producer: s("producer"),
            subject_commit: s("subject_commit"),
            check_id: s("check_id"),
            criteria_ids: strings(v, "criteria_ids")?,
            result: req(v, "result")?,
            exit_code: v.get("exit_code").and_then(Value::as_i64),
            flags: strings(v, "flags")?,
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

    /// All three given and `remaining` is not `limit - spent`: the surface shows the server's
    /// number as reported and says so (it never replaces it with its own).
    pub fn remaining_disagrees(&self) -> bool {
        match (self.spent, self.limit, self.remaining) {
            (Some(s), Some(l), Some(r)) => (l - s - r).abs() > 1e-9 * l.max(1.0),
            _ => false,
        }
    }

    fn line(&self, name: &str) -> String {
        let note = if self.remaining_disagrees() {
            " (server-reported, does not match limit and spent)"
        } else {
            ""
        };
        format!(
            "{name} spent {} limit {} remaining {}{note}\n",
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
    /// Cost caps reserved for attempts that have no final usage yet: a cap, not a spend.
    pub reserved_usd: Option<f64>,
}

impl BudgetView {
    /// From an `outcome.budget` reply: `{tokens, cost_usd, wall_secs}`, each `{spent, limit, remaining}`
    /// as far as the server has them (it sends no tokens and no `remaining`), and `reserved_usd`.
    /// Anything absent is unknown.
    pub fn from_json(v: &Value) -> Self {
        Self {
            tokens: Amount::from_json(v.get("tokens")),
            cost_usd: Amount::from_json(v.get("cost_usd")),
            wall_secs: Amount::from_json(v.get("wall_secs")),
            reserved_usd: v
                .get("reserved_usd")
                .and_then(Value::as_f64)
                .filter(|x| x.is_finite() && *x >= 0.0),
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
            format!("reserved_usd {}\n", shown(self.reserved_usd)),
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
    Diff(DiffTab),
    Unavailable { tab: InspectorTab, reason: String },
}

/// One file of an `attempt.diff` reply. A binary file has counts only; a secret-shaped path has its
/// name only (the server withholds the rest); `patch_truncated` says the server cut the patch.
#[derive(Clone, Debug, PartialEq)]
pub struct DiffFileView {
    pub path: String,
    pub status: String,
    pub binary: bool,
    pub secret: bool,
    pub added: Option<u64>,
    pub removed: Option<u64>,
    pub patch: Option<String>,
    pub patch_truncated: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct DiffTab {
    pub base_commit: String,
    pub commit: String,
    pub total_files: u64,
    pub files: Vec<DiffFileView>,
}

/// A patch is up to this long (the server caps it lower); the generic text cap would cut it short.
const MAX_PATCH: usize = 256 * 1024;

pub const DIFF_UNAVAILABLE: &str = "no diff to show for this attempt (it has no candidate commit yet, or the server could not read the repository); its branch and session are inspectable with git and `synt sessions show`";

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
            Self::Diff(_) => InspectorTab::Diff,
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
                .chain(
                    (events.len() > MAX_ROWS)
                        .then(|| format!("{} more events not shown", events.len() - MAX_ROWS)),
                )
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

    /// `reply` is an `attempt.diff` reply.
    pub fn diff_from(reply: &Value) -> Result<Self, String> {
        typed(reply, &["base_commit", "commit"], &["total_files"], &[])?;
        let files = reply
            .get("files")
            .and_then(Value::as_array)
            .ok_or("unreadable reply: no files")?;
        if files.len() > MAX_ROWS {
            return Err(format!("more than {MAX_ROWS} files, not shown"));
        }
        let n = |f: &Value, k| f.get(k).and_then(Value::as_u64);
        let b = |f: &Value, k| f.get(k).and_then(Value::as_bool).unwrap_or(false);
        let files = files
            .iter()
            .map(|f| {
                typed(
                    f,
                    &["path", "status", "patch"],
                    &[],
                    &["binary", "secret", "patch_truncated"],
                )?;
                let mut patch = f
                    .get("patch")
                    .and_then(Value::as_str)
                    .map(|p| p.split('\n').map(clean).collect::<Vec<_>>().join("\n"));
                if let Some(p) = patch.as_mut().filter(|p| p.len() > MAX_PATCH) {
                    let cut = (0..=MAX_PATCH)
                        .rev()
                        .find(|i| p.is_char_boundary(*i))
                        .unwrap_or(0);
                    p.truncate(cut);
                }
                Ok(DiffFileView {
                    path: req(f, "path")?,
                    status: opt(f, "status").unwrap_or_else(|| UNKNOWN.into()),
                    binary: b(f, "binary"),
                    secret: b(f, "secret"),
                    added: n(f, "added"),
                    removed: n(f, "removed"),
                    patch,
                    patch_truncated: b(f, "patch_truncated"),
                })
            })
            .collect::<Result<Vec<_>, String>>()?;
        Ok(Self::Diff(DiffTab {
            base_commit: opt(reply, "base_commit").unwrap_or_default(),
            commit: opt(reply, "commit").unwrap_or_default(),
            total_files: n(reply, "total_files").unwrap_or(files.len() as u64),
            files,
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
            Self::Diff(d) => {
                out.push_str(&format!(
                    "{} -> {}: {} files{}\n",
                    d.base_commit,
                    d.commit,
                    d.total_files,
                    if d.total_files > d.files.len() as u64 {
                        " (list cut)"
                    } else {
                        ""
                    }
                ));
                for f in &d.files {
                    let note = if f.secret {
                        " (secret-shaped path: name only)"
                    } else if f.binary {
                        " (binary)"
                    } else {
                        ""
                    };
                    out.push_str(&format!(
                        "{} {} +{} -{}{note}\n",
                        f.status,
                        f.path,
                        shown(f.added),
                        shown(f.removed)
                    ));
                    if let Some(p) = &f.patch {
                        out.push_str(p);
                        if !p.ends_with('\n') {
                            out.push('\n');
                        }
                    }
                    if f.patch_truncated {
                        out.push_str("[patch cut by the server]\n");
                    }
                }
            }
            Self::Unavailable { reason, .. } => out.push_str(&format!("not available: {reason}\n")),
        }
        out
    }
}

#[cfg(test)]
mod tests;
