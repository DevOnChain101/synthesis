//! What the home shows, as plain rows derived from `OutcomesView`'s state (no window, so it is testable), and the thin
//! `pub(crate)` wrappers over the view's public API that the home acts through. Nothing here computes a state or a
//! verdict (ADR-021): a row is a field of a `synthesis_view::outcomes` model, a word or a count.

use std::collections::BTreeSet;

use gpui::Context;
use synthesis_view::outcomes::{NeedsYou, Source, Tier, WSL2_REFUSAL, ts_ms};

use crate::approvals::{self, Kind, Row};
use crate::live::now_ms;
use crate::superagent::kit::Status;
use crate::superagent::outcomes::OutcomesView;
use crate::superagent::outcomes::model::{Effect, Item, Screen, State};

/// What ships later (approvals-concept A1 queue view model, A4 `resolve_many`): the one-tap recommended button, batch
/// apply and Undo. The home is built so they slot into a row, and nothing draws them while a gate is false.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Gates {
    pub one_tap: bool,
    pub batch: bool,
    pub undo: bool,
}

impl Gates {
    pub const SHIPPED: Self = Self {
        one_tap: false,
        batch: false,
        undo: false,
    };
}

/// One thing that waits for the person: a decision, a contract, or a session that asks for a tool. With an older
/// orchestrator, one row per outcome with decisions pending.
#[derive(Clone, Debug, PartialEq)]
pub struct NeedRow {
    pub key: String,
    pub kind: Kind,
    /// The decision of a queue row.
    pub decision: Option<String>,
    pub title: String,
    /// "Decision \u{b7} blocked", "Contract v2 waits for approval", "2 decisions" or "Tool permission".
    pub detail: String,
    pub age: String,
    /// "expires in 23h".
    pub expires: Option<String>,
    /// "blocks w3 \u{b7} 2 waiting on it".
    pub blocks: Option<String>,
    /// The server's tier; `None` (a tool ask, an older orchestrator) never gets a one-tap.
    pub tier: Option<Tier>,
    pub recommended: Option<String>,
}

impl NeedRow {
    /// The one-tap button: gated, T1 only, and only when the server named a recommended answer.
    pub fn one_tap(&self, g: Gates) -> bool {
        g.one_tap && self.tier == Some(Tier::T1) && self.recommended.is_some()
    }

    /// Unique in the list (the same rule as `approvals::Row::id`).
    pub fn id(&self) -> &str {
        self.decision.as_deref().unwrap_or(&self.key)
    }
}

/// A row that asks nothing: a lapsed decision, or a recorded answer.
#[derive(Clone, Debug, PartialEq)]
pub struct LogRow {
    pub title: String,
    pub meta: Vec<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct RunRow {
    pub id: String,
    pub title: String,
    pub status: Status,
    /// Plain parts: the state word, the budget line.
    pub meta: Vec<String>,
    /// Beige notes: what waits for the person.
    pub notes: Vec<String>,
    /// Budget spent over its limit when both are known (the list carries no work-item counts).
    pub frac: Option<f32>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct FinRow {
    pub id: String,
    pub title: String,
    pub status: Status,
    pub meta: Vec<String>,
    pub notes: Vec<String>,
}

/// A row the keyboard cursor can stand on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Target {
    Need(usize),
    Run(usize),
    Fin(usize),
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Model {
    pub loading: bool,
    /// Why the link is not up (shown above the last known rows, with Retry).
    pub banner: Option<String>,
    pub retry: bool,
    pub needs: Vec<NeedRow>,
    /// What the queue left out (outcomes it could not read, rows over its limit), in words.
    pub gaps: Option<String>,
    pub lapsed: Vec<LogRow>,
    /// The newest recorded answers first.
    pub answered: Vec<LogRow>,
    pub running: Vec<RunRow>,
    pub finished: Vec<FinRow>,
}

impl Model {
    /// Nothing at all: the home invites the person to start something.
    pub fn is_empty(&self) -> bool {
        !self.loading
            && self.needs.is_empty()
            && self.running.is_empty()
            && self.finished.is_empty()
    }

    /// The rows in screen order (J and K walk this).
    pub fn flat(&self) -> Vec<Target> {
        (0..self.needs.len())
            .map(Target::Need)
            .chain((0..self.running.len()).map(Target::Run))
            .chain((0..self.finished.len()).map(Target::Fin))
            .collect()
    }
}

pub fn status_of(state: &str, needs: &NeedsYou) -> Status {
    match state {
        _ if needs.blocked || state == "blocked" => Status::Blocked,
        "failed" | "terminal_failure" | "rejected" | "lost" => Status::Failed,
        "verified" | "completed" | "succeeded" | "finished" | "approved" => Status::Verified,
        "draft" => Status::Draft,
        _ => Status::Running,
    }
}

fn plural(n: u32, one: &str) -> String {
    format!("{n} {one}{}", if n == 1 { "" } else { "s" })
}

/// The newest recorded answers shown.
const ANSWERED: usize = 20;

/// The work item keys of `outcome` that a pending decision holds (not those of a plan approval, which holds them
/// all: the count note says that one).
fn held_keys(st: &State, outcome: &str) -> Vec<String> {
    let rows = st.queue.iter().flat_map(|q| &q.queue.rows);
    let keys: BTreeSet<&String> = rows
        .filter(|r| {
            r.outcome == outcome && r.source == Source::Decision && r.kind != "plan_approval"
        })
        .flat_map(|r| &r.blocks)
        .collect();
    keys.into_iter().take(5).cloned().collect()
}

fn frac(i: &Item) -> Option<f32> {
    match (i.budget.cost_usd.spent, i.budget.cost_usd.limit) {
        (Some(s), Some(l)) if l > 0. => Some((s / l) as f32),
        _ => None,
    }
}

/// The banner of a screen other than the list, with whether a retry makes sense.
fn banner(s: &Screen) -> Option<(String, bool)> {
    let clean = |s: &str| synthesis_harness::sanitize_for_display(s);
    match s {
        Screen::Ready | Screen::Loading => None,
        Screen::Off => Some(("Super Agent is off, so nothing runs here.".into(), true)),
        Screen::NotReady(e) => Some((format!("Super Agent is not ready: {}", clean(e)), true)),
        Screen::Windows => Some((WSL2_REFUSAL.into(), false)),
        Screen::Unreachable(e) => Some((format!("Can't reach Super Agent: {}", clean(e)), true)),
    }
}

/// `rows` are `OutcomesView::approval_rows()` (empty unless the link is up).
pub fn build(st: &State, rows: Vec<Row>, now: i64) -> Model {
    let (banner, retry) = banner(&st.screen).unzip();
    let mut m = Model {
        loading: st.screen == Screen::Loading,
        banner,
        retry: retry.unwrap_or(false),
        ..Model::default()
    };
    m.needs = rows
        .into_iter()
        .map(|r| {
            let decision = r.decision().map(str::to_string);
            let pending = st
                .items
                .iter()
                .find(|i| i.row.id == r.key)
                .map_or(0, |i| i.row.needs_you.decisions_pending);
            let ask = r.ask.as_ref();
            NeedRow {
                detail: match (r.kind, ask) {
                    (Kind::Tool, _) => "Tool permission".into(),
                    (Kind::Contract, a) => format!(
                        "Contract v{} waits for approval",
                        a.map_or(0, |a| a.version)
                    ),
                    (Kind::Decision, Some(a)) => {
                        format!("Decision \u{b7} {}", a.kind.replace('_', " "))
                    }
                    (Kind::Decision, None) => plural(pending, "decision"),
                },
                age: approvals::age_label(now - r.since),
                expires: ask.and_then(|a| a.expires_ms).map(|x| {
                    if x > now {
                        format!("expires in {}", approvals::age_label(x - now))
                    } else {
                        "expired".into()
                    }
                }),
                blocks: ask.and_then(|a| a.blocks_line()),
                // a live tool ask is T2 by definition (spec 3.3 rows 9 and 10), once tiers are shown at all
                tier: ask
                    .map(|a| a.tier)
                    .or_else(|| (r.kind == Kind::Tool && st.queue.is_some()).then_some(Tier::T2)),
                recommended: ask
                    .and_then(|a| a.recommended.as_ref())
                    .map(|a| a.label.clone()),
                title: r.subject,
                key: r.key,
                kind: r.kind,
                decision,
            }
        })
        .collect();
    // like the rows above, these are drawn only while the link is up
    if let Some(q) = st.queue.as_ref().filter(|_| st.screen == Screen::Ready) {
        m.gaps = q.queue.gaps();
        m.lapsed = q
            .queue
            .rows
            .iter()
            .filter(|r| r.source == Source::Lapsed)
            .map(|r| LogRow {
                title: r.goal.clone(),
                meta: [Some(r.kind.replace('_', " ")), Some(r.state.clone())]
                    .into_iter()
                    .chain([r.blocks_line()])
                    .flatten()
                    .collect(),
            })
            .collect();
        let mut answered: Vec<_> = q.answered.iter().collect();
        answered.sort_by(|a, b| (&b.1.ts, b.1.event_id).cmp(&(&a.1.ts, a.1.event_id)));
        m.answered = answered
            .into_iter()
            .take(ANSWERED)
            .map(|(outcome, r)| LogRow {
                title: st
                    .items
                    .iter()
                    .find(|i| i.row.id == *outcome)
                    .map_or_else(|| outcome.clone(), |i| i.row.goal.clone()),
                meta: [
                    Some(r.describe()),
                    ts_ms(&r.ts).map(|t| approvals::age_label(now - t)),
                ]
                .into_iter()
                .flatten()
                .collect(),
            })
            .collect();
    }
    for i in &st.items {
        let (status, n) = (status_of(&i.row.state, &i.row.needs_you), &i.row.needs_you);
        let mut notes = vec![];
        let held = held_keys(st, &i.row.id);
        if !held.is_empty() {
            notes.push(format!(
                "{} {} for your decision",
                held.join(", "),
                if held.len() == 1 { "waits" } else { "wait" }
            ));
        } else if n.decisions_pending > 0 {
            notes.push(format!(
                "{} waiting for a decision",
                plural(n.decisions_pending, "decision")
            ));
        }
        if n.verified_awaiting_ack {
            notes.push("Waiting for your acknowledgement".into());
        }
        let meta = vec![
            i.row.state.clone(),
            crate::superagent::outcomes::render::budget_line(&i.budget),
        ];
        if matches!(status, Status::Verified | Status::Failed) {
            m.finished.push(FinRow {
                id: i.row.id.clone(),
                title: i.row.goal.clone(),
                status,
                meta,
                notes,
            });
        } else {
            m.running.push(RunRow {
                id: i.row.id.clone(),
                title: i.row.goal.clone(),
                status,
                meta,
                notes,
                frac: frac(i),
            });
        }
    }
    m
}

/// The model of what the view holds now.
pub fn model(o: &OutcomesView) -> Model {
    let now = o.pop.fixed_now.unwrap_or_else(now_ms);
    build(&o.state, o.approval_rows(), now)
}

pub fn now(o: &OutcomesView) -> i64 {
    o.pop.fixed_now.unwrap_or_else(now_ms)
}

/// Review: the outcome is selected (its decision card is read) and the home draws the card when it lands.
pub fn review(o: &mut OutcomesView, id: &str, cx: &mut Context<OutcomesView>) {
    o.select_id(id, cx);
}

/// An answer the card made, sent through the view's own worker path.
pub fn answer(o: &mut OutcomesView, e: Effect, cx: &mut Context<OutcomesView>) {
    o.send_effect(e, cx);
}

/// Retry of a failed link. The view's public way to run an effect is `send_effect`, which marks an answer in
/// flight; this undoes that mark at once (a retry is not an answer). Wants a public `retry()` on `OutcomesView`.
pub fn retry(o: &mut OutcomesView, cx: &mut Context<OutcomesView>) {
    o.send_effect(Effect::Retry, cx);
    o.state.ask.sending = false;
    o.state.ask.notice = None;
}
