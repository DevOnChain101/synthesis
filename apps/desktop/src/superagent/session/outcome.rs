//! The Outcome frame (phase C3; `super-agent-s1.md` 5.1, mock `super-agent-v3` `vOutcome`): the goal, its state and
//! budget, the stored contract, the work items with their state, the attempts, the decisions and, for a read-only
//! goal, the report. Nothing is invented: every line is a field of what `OutcomesView` read from the server, or says
//! why it is not there. `build` is pure (state in, rows out); the entity only draws and moves a cursor.
//!
//! A decision is answered on the home, where the one card with its rules lives (nothing preselected, the brief
//! before Send, the 400 ms guard): this frame lists them and sends the person there. It sends nothing itself.

use gpui::{
    AnyElement, App, Context, Entity, EventEmitter, FocusHandle, Focusable,
    InteractiveElement as _, IntoElement, KeyDownEvent, ParentElement as _, Render, ScrollHandle,
    StatefulInteractiveElement as _, Styled as _, Window, div, px,
};
use synthesis_core::outcome::ContractKind;
use synthesis_view::outcomes::{BriefRow, InspectorContent, WSL2_REFUSAL};

use crate::superagent::outcomes::OutcomesView;
use crate::superagent::outcomes::model::{Screen, State};
use crate::superagent::outcomes::render::budget_line;
use crate::superagent::home::data::status_of;
use crate::superagent::home::{RowSpec, row, section};
use crate::superagent::kit::{self, ButtonKind, Status};
use crate::superagent::tokens::{self as t, hsla};

/// The most characters of a report shown; the rest is said, not hidden.
const REPORT_CHARS: usize = 6000;

/// The frame asked the shell for something.
pub enum OutcomeEvent {
    /// A work item row was opened: its worker frame.
    OpenWorker(String),
    /// "Answer on the home": the home opens this outcome's decision card.
    AnswerOnHome,
}

impl EventEmitter<OutcomeEvent> for OutcomeFrame {}

#[derive(Clone, Debug, PartialEq)]
pub struct ItemRow {
    pub id: String,
    pub status: Status,
    pub title: String,
    pub meta: Vec<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct AttemptRow {
    pub status: Status,
    pub title: String,
    pub meta: Vec<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct DecisionRow {
    pub status: Status,
    pub title: String,
    pub meta: Vec<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Ready {
    pub goal: String,
    pub status: Status,
    pub meta: Vec<String>,
    pub notes: Vec<String>,
    pub read_only: bool,
    /// `(label, value)` of the stored contract, or why it cannot be shown.
    pub contract: Result<Vec<(String, String)>, String>,
    pub items: Result<Vec<ItemRow>, String>,
    pub attempts: Vec<AttemptRow>,
    pub decisions: Result<Vec<DecisionRow>, String>,
    pub pending: usize,
    /// The worker's report and citations of a read-only goal (its own words: inert text), if one came.
    pub report: Vec<BriefRow>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Body {
    /// The list or the detail has not landed.
    Loading,
    /// Nothing to show, with the reason in words.
    Gone(String),
    Ready {
        ready: Box<Ready>,
        /// The link is down: these are the last rows read.
        banner: Option<String>,
    },
}

/// How an item or attempt state word reads as a status. The word itself is always shown beside it.
pub fn item_status(state: &str) -> Status {
    match state {
        "running" | "verifying" | "leased" | "recovering" | "cancel_requested" => Status::Running,
        "waiting_decision" => Status::Needs,
        "blocked" => Status::Blocked,
        "succeeded" | "finished" => Status::Verified,
        "terminal_failure" | "lost" => Status::Failed,
        "cancelled" => Status::Stopped,
        _ => Status::Idle,
    }
}

/// No message can reach this work item any more (the owner refuses it too).
pub fn item_finished(state: &str) -> bool {
    matches!(state, "succeeded" | "terminal_failure" | "cancelled")
}

pub fn word(s: &str) -> String {
    s.replace('_', " ")
}

fn plural(n: usize, one: &str) -> String {
    format!("{n} {one}{}", if n == 1 { "" } else { "s" })
}

fn duration(secs: f64) -> String {
    if secs < 90. {
        format!("{secs:.0} s")
    } else if secs < 5400. {
        format!("{:.0} min", secs / 60.)
    } else {
        format!("{:.1} h", secs / 3600.)
    }
}

/// "12 min of 1.0 h" when the server gave the time spent; nothing when it did not.
fn time_line(st: &State, id: &str) -> Option<String> {
    let b = st.items.iter().find(|i| i.row.id == id)?.budget.wall_secs;
    match (b.spent, b.limit) {
        (Some(s), Some(l)) => Some(format!("{} of {}", duration(s), duration(l))),
        (Some(s), None) => Some(duration(s)),
        _ => None,
    }
}

/// The work items, attempts and decisions of outcome `id` as rows, from what the link last read.
pub fn build(st: &State, id: &str) -> Body {
    let Some(item) = st.items.iter().find(|i| i.row.id == id) else {
        return match &st.screen {
            Screen::Loading => Body::Loading,
            Screen::Ready => Body::Gone("This outcome is not in the list. It may be gone.".into()),
            Screen::Off => Body::Gone("Super Agent is off, so nothing is shown here.".into()),
            Screen::NotReady(e) => Body::Gone(format!("Super Agent is not ready: {e}")),
            Screen::Windows => Body::Gone(WSL2_REFUSAL.into()),
            Screen::Unreachable(e) => Body::Gone(format!("Can't reach Super Agent: {e}")),
        };
    };
    let Some(d) = st
        .detail
        .as_ref()
        .filter(|d| d.outcome == id && st.selected.as_deref() == Some(id))
    else {
        return Body::Loading;
    };
    let banner = match &st.screen {
        Screen::Ready | Screen::Loading => None,
        Screen::Off => Some("Super Agent is off. Showing what was read last.".to_string()),
        Screen::NotReady(e) | Screen::Unreachable(e) => Some(format!(
            "Can't reach Super Agent: {e}. Showing what was read last."
        )),
        Screen::Windows => Some(WSL2_REFUSAL.to_string()),
    };
    let read_only = d
        .current
        .as_ref()
        .is_some_and(|c| c.kind == ContractKind::ReadOnly);
    let workers = match &d.workers {
        InspectorContent::Workers(w) => Ok(w),
        InspectorContent::Unavailable { reason, .. } => Err(reason.clone()),
        _ => Err("the work items were not read".to_string()),
    };
    let items = workers.as_ref().map_err(Clone::clone).map(|w| {
        w.plan
            .items
            .iter()
            .map(|i| {
                let n = w.attempts.iter().filter(|a| a.work_item_id == i.id).count();
                let mut meta = vec![word(&i.state), plural(n, "attempt")];
                if !i.requires.is_empty() {
                    meta.push(format!("after {}", i.requires.join(", ")));
                }
                ItemRow {
                    id: i.id.clone(),
                    status: item_status(&i.state),
                    title: format!("{} {}", i.key, word(&i.kind)),
                    meta,
                }
            })
            .collect::<Vec<_>>()
    });
    let attempts = workers.as_ref().map_or_else(
        |_| vec![],
        |w| {
            let key = |id: &str| {
                w.plan
                    .items
                    .iter()
                    .find(|i| i.id == id)
                    .map_or_else(|| id.to_string(), |i| i.key.clone())
            };
            let mut a: Vec<_> = w.attempts.iter().collect();
            a.sort_by(|x, y| (&x.started_at, x.ordinal).cmp(&(&y.started_at, y.ordinal)));
            a.into_iter()
                .map(|a| AttemptRow {
                    status: item_status(&a.state),
                    title: format!(
                        "{} attempt {}",
                        key(&a.work_item_id),
                        a.ordinal.map_or_else(|| "?".into(), |o| o.to_string())
                    ),
                    meta: [
                        Some(word(&a.state)),
                        (!a.started_at.is_empty()).then(|| format!("started {}", a.started_at)),
                        (!a.ended_at.is_empty()).then(|| format!("ended {}", a.ended_at)),
                    ]
                    .into_iter()
                    .flatten()
                    .collect(),
                })
                .collect()
        },
    );
    let decisions = d.cards.as_ref().map_err(Clone::clone).map(|cards| {
        cards
            .iter()
            .map(|c| DecisionRow {
                status: match c.state.as_str() {
                    "pending" => Status::Needs,
                    "approved" | "resolved" => Status::Verified,
                    "expired" | "invalidated" => Status::Stopped,
                    _ => Status::Idle,
                },
                title: word(&c.kind),
                meta: vec![
                    word(&c.state),
                    format!("decision {}", c.id),
                    format!(
                        "expires {}",
                        if c.expires_at.is_empty() {
                            "unknown"
                        } else {
                            &c.expires_at
                        }
                    ),
                ],
            })
            .collect::<Vec<_>>()
    });
    let pending = d
        .cards
        .as_ref()
        .map_or(0, |c| c.iter().filter(|c| c.resolvable()).count());
    let report = d.cards.as_ref().map_or_else(
        |_| vec![],
        |cards| {
            cards
                .iter()
                .filter(|c| c.kind == "report_review")
                .flat_map(|c| c.rows.iter())
                .filter(|r| r.label.starts_with("Report") || r.label == "Citations")
                .cloned()
                .collect()
        },
    );
    let contract = d.contract.as_ref().map_err(Clone::clone).map(|c| {
        let mut v = vec![
            ("Goal".to_string(), c.goal.clone()),
            (
                "Kind".into(),
                if read_only {
                    "Read-only. Never writes.".into()
                } else {
                    "Writes in a git worktree".into()
                },
            ),
            (
                "Version".into(),
                format!(
                    "{} {}",
                    c.version
                        .map_or_else(|| "unknown".into(), |v| format!("v{v}")),
                    match c.approved {
                        Some(true) => "approved",
                        Some(false) => "not approved",
                        None => "approval unknown",
                    }
                ),
            ),
            ("Budget".into(), budget_line(&item.budget)),
        ];
        v.extend(c.criteria.iter().map(|k| {
            (
                format!("Criterion {}", k.id),
                format!(
                    "{}{}",
                    k.text,
                    if k.manual == Some(true) {
                        " (you decide)"
                    } else {
                        ""
                    }
                ),
            )
        }));
        v.extend(c.checks.iter().map(|k| ("Check".to_string(), k.clone())));
        v
    });
    let done = items.as_ref().ok().map(|i| {
        let n = i.iter().filter(|r| r.status == Status::Verified).count();
        format!("{n} of {} work items done", i.len())
    });
    let mut meta = vec![word(&item.row.state)];
    meta.extend(done);
    meta.push(budget_line(&item.budget));
    meta.extend(time_line(st, id));
    let mut notes = vec![];
    if pending > 0 {
        notes.push(format!("{} waiting for you", plural(pending, "decision")));
    }
    if item.row.needs_you.verified_awaiting_ack {
        notes.push("Waiting for your acknowledgement".into());
    }
    Body::Ready {
        ready: Box::new(Ready {
            goal: item.row.goal.clone(),
            status: status_of(&item.row.state, &item.row.needs_you),
            meta,
            notes,
            read_only,
            contract,
            items,
            attempts,
            decisions,
            pending,
            report,
        }),
        banner,
    }
}

/// The cursor after Up or Down over `n` rows: it starts on the first row, never wraps.
pub fn step(cur: Option<usize>, n: usize, down: bool) -> Option<usize> {
    match (cur, n) {
        (_, 0) => None,
        (None, _) => Some(0),
        (Some(c), _) if down => Some((c + 1).min(n - 1)),
        (Some(c), _) => Some(c.saturating_sub(1)),
    }
}

pub struct OutcomeFrame {
    outcomes: Entity<OutcomesView>,
    pub(crate) id: String,
    pub(crate) focus: FocusHandle,
    cursor: Option<usize>,
    scroll: ScrollHandle,
}

impl OutcomeFrame {
    pub fn new(outcomes: Entity<OutcomesView>, id: String, cx: &mut Context<Self>) -> Self {
        cx.observe(&outcomes, |_, _, cx| cx.notify()).detach();
        Self {
            outcomes,
            id,
            focus: cx.focus_handle(),
            cursor: None,
            scroll: ScrollHandle::new(),
        }
    }

    fn body(&self, cx: &App) -> Body {
        build(&self.outcomes.read(cx).state, &self.id)
    }

    fn open(&mut self, i: usize, cx: &mut Context<Self>) {
        if let Body::Ready { ready, .. } = self.body(cx)
            && let Ok(items) = &ready.items
            && let Some(it) = items.get(i)
        {
            cx.emit(OutcomeEvent::OpenWorker(it.id.clone()));
        }
    }

    /// Up and Down (or K and J) move the cursor, Enter opens its work item. Everything else is the shell's.
    pub fn on_key(&mut self, ev: &KeyDownEvent, cx: &mut Context<Self>) -> bool {
        let (k, m) = (&ev.keystroke, &ev.keystroke.modifiers);
        if m.control || m.alt || m.platform || m.shift {
            return false;
        }
        let n = match self.body(cx) {
            Body::Ready { ready, .. } => ready.items.as_ref().map_or(0, Vec::len),
            _ => 0,
        };
        match k.key.as_str() {
            "down" | "j" | "up" | "k" => {
                self.cursor = step(self.cursor, n, matches!(k.key.as_str(), "down" | "j"));
                cx.notify();
            }
            "enter" => match self.cursor {
                Some(i) => self.open(i, cx),
                None => return false,
            },
            _ => return false,
        }
        true
    }

    fn notice(text: &str) -> AnyElement {
        kit::text(t::T3, t::W_REGULAR, t::TEXT)
            .debug_selector(|| "oc-note".to_string())
            .whitespace_normal()
            .px(px(t::S4))
            .child(text.to_string())
            .into_any_element()
    }

    fn kv(rows: &[(String, String)]) -> AnyElement {
        div()
            .debug_selector(|| "oc-contract".to_string())
            .flex()
            .flex_col()
            .gap(px(t::S2))
            .children(rows.iter().map(|(k, v)| {
                div()
                    .flex()
                    .gap(px(t::S4))
                    .child(
                        kit::text(t::T2, t::W_REGULAR, t::TEXT)
                            .flex_none()
                            .w(px(120.))
                            .child(k.clone()),
                    )
                    .child(
                        kit::text(t::T3, t::W_REGULAR, t::TEXT_STRONG)
                            .flex_1()
                            .min_w(px(0.))
                            .whitespace_normal()
                            .child(v.clone()),
                    )
            }))
            .into_any_element()
    }

    fn report(rows: &[BriefRow]) -> AnyElement {
        kit::card("oc-report", false)
            .children(rows.iter().map(|r| {
                let mut text = r.text.clone();
                if text.chars().count() > REPORT_CHARS {
                    text = text.chars().take(REPORT_CHARS).collect::<String>() + "\n... (cut here)";
                }
                div()
                    .flex()
                    .flex_col()
                    .gap(px(t::S1))
                    .child(kit::text(t::T2, t::W_MEDIUM, t::TEXT).child(r.label.clone()))
                    // the worker's own words: inert text, never a link or a command
                    .child(
                        kit::text(t::T3, t::W_REGULAR, t::TEXT_STRONG)
                            .whitespace_normal()
                            .child(text),
                    )
            }))
            .into_any_element()
    }

    fn ready(&self, r: &Ready, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let mut out = vec![];
        out.push(
            div()
                .debug_selector(|| "oc-head".to_string())
                .flex()
                .flex_col()
                .gap(px(t::S2))
                .px(px(t::S4))
                .child(
                    kit::text(t::T5, t::W_SEMIBOLD, t::TEXT_STRONG)
                        .whitespace_normal()
                        .child(r.goal.clone()),
                )
                .child(
                    div()
                        .flex()
                        .flex_wrap()
                        .items_center()
                        .gap_x(px(t::S4))
                        .child(kit::status_glyph("oc-status", r.status))
                        .children(
                            r.meta
                                .iter()
                                .map(|m| kit::text(t::T2, t::W_REGULAR, t::TEXT).child(m.clone())),
                        )
                        .children(r.notes.iter().map(|m| {
                            kit::text(t::T2, t::W_REGULAR, t::SOFT_BEIGE).child(m.clone())
                        })),
                )
                .into_any_element(),
        );
        if r.pending > 0 {
            out.push(
                kit::card("oc-needs", true)
                    .child(kit::text(t::T3, t::W_MEDIUM, t::TEXT_STRONG).child(format!(
                        "{} for you to answer",
                        plural(r.pending, "decision")
                    )))
                    .child(
                        kit::button(
                            "oc-answer",
                            "Answer on the home",
                            ButtonKind::Primary,
                            false,
                            Some(Box::new(
                                cx.listener(|_, _, _, cx| cx.emit(OutcomeEvent::AnswerOnHome)),
                            )),
                        )
                        .self_start(),
                    )
                    .into_any_element(),
            );
        }
        if r.read_only {
            let body = if r.report.is_empty() {
                Self::notice("No report yet. It appears here when the read-only run ends.")
            } else {
                Self::report(&r.report)
            };
            out.push(
                section("oc-report-sec", "Report", r.report.len(), vec![body]).into_any_element(),
            );
        }
        let items = match &r.items {
            Err(e) => vec![Self::notice(&format!(
                "The work items could not be read: {e}"
            ))],
            Ok(i) if i.is_empty() => {
                vec![Self::notice(
                    "No plan yet. Work items appear after the contract is approved.",
                )]
            }
            Ok(i) => i
                .iter()
                .enumerate()
                .map(|(n, it)| {
                    let id = it.id.clone();
                    row(
                        ("oc-item", n),
                        RowSpec {
                            status: it.status,
                            title: it.title.clone(),
                            meta: it.meta.clone(),
                            notes: vec![],
                            trailing: vec![],
                            current: self.cursor == Some(n),
                        },
                        Some(Box::new(cx.listener(move |_, _, _, cx| {
                            cx.emit(OutcomeEvent::OpenWorker(id.clone()))
                        }))),
                    )
                    .into_any_element()
                })
                .collect(),
        };
        out.push(
            section(
                "oc-items",
                "Work items",
                r.items.as_ref().map_or(0, Vec::len),
                items,
            )
            .into_any_element(),
        );
        if !r.attempts.is_empty() {
            let rows = r
                .attempts
                .iter()
                .enumerate()
                .map(|(n, a)| {
                    row(
                        ("oc-attempt", n),
                        RowSpec {
                            status: a.status,
                            title: a.title.clone(),
                            meta: a.meta.clone(),
                            notes: vec![],
                            trailing: vec![],
                            current: false,
                        },
                        None,
                    )
                    .into_any_element()
                })
                .collect();
            out.push(section("oc-attempts", "Attempts", r.attempts.len(), rows).into_any_element());
        }
        let decisions = match &r.decisions {
            Err(e) => vec![Self::notice(&format!(
                "The decisions could not be read: {e}"
            ))],
            Ok(d) if d.is_empty() => vec![Self::notice("Nothing was asked of you yet.")],
            Ok(d) => d
                .iter()
                .enumerate()
                .map(|(n, d)| {
                    row(
                        ("oc-decision", n),
                        RowSpec {
                            status: d.status,
                            title: d.title.clone(),
                            meta: d.meta.clone(),
                            notes: vec![],
                            trailing: vec![],
                            current: false,
                        },
                        None,
                    )
                    .into_any_element()
                })
                .collect(),
        };
        out.push(
            section(
                "oc-decisions",
                "Decisions",
                r.decisions.as_ref().map_or(0, Vec::len),
                decisions,
            )
            .into_any_element(),
        );
        let contract = match &r.contract {
            Err(e) => Self::notice(&format!("The contract could not be read: {e}")),
            Ok(rows) => kit::card("oc-contract-card", false)
                .child(Self::kv(rows))
                .into_any_element(),
        };
        out.push(section("oc-contract-sec", "Contract", 0, vec![contract]).into_any_element());
        out
    }
}

impl Focusable for OutcomeFrame {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for OutcomeFrame {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let (banner, body): (Option<String>, Vec<AnyElement>) = match self.body(cx) {
            Body::Loading => (
                None,
                vec![
                    kit::text(t::T3, t::W_REGULAR, t::TEXT)
                        .debug_selector(|| "oc-loading".to_string())
                        .px(px(t::S4))
                        .child("Loading the outcome...")
                        .into_any_element(),
                ],
            ),
            Body::Gone(why) => (None, vec![Self::notice(&why)]),
            Body::Ready { ready, banner } => (banner, self.ready(&ready, cx)),
        };
        div()
            .id("sa-outcome")
            .debug_selector(|| "sa-outcome".to_string())
            .track_focus(&self.focus)
            .on_key_down(cx.listener(|v, ev: &KeyDownEvent, _, cx| {
                if v.on_key(ev, cx) {
                    cx.stop_propagation();
                }
            }))
            .size_full()
            .flex()
            .flex_col()
            .children(banner.map(|b| {
                kit::text(t::T2, t::W_REGULAR, t::SOFT_BEIGE)
                    .debug_selector(|| "oc-banner".to_string())
                    .whitespace_normal()
                    .px(px(t::S5))
                    .py(px(t::S2))
                    .bg(hsla(t::ELEVATED))
                    .child(b)
            }))
            .child(
                div()
                    .id("oc-scroll")
                    .flex_1()
                    .min_h(px(0.))
                    .overflow_y_scroll()
                    .track_scroll(&self.scroll)
                    .child(
                        div()
                            .w_full()
                            .max_w(px(728.))
                            .mx_auto()
                            .px(px(t::S5))
                            .py(px(t::S6))
                            .flex()
                            .flex_col()
                            .gap(px(t::S5))
                            .children(body),
                    ),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::superagent::outcomes::sample;
    use synthesis_view::outcomes::InspectorTab;

    fn ready(st: &State, id: &str) -> Ready {
        match build(st, id) {
            Body::Ready { ready, .. } => *ready,
            other => panic!("not ready: {other:?}"),
        }
    }

    #[test]
    fn the_sample_outcome_has_its_work_items_attempts_and_decisions_as_rows() {
        let st = sample::state();
        let r = ready(&st, "o_auth");
        let items = r.items.unwrap();
        assert_eq!(items.len(), 5);
        assert_eq!(items[2].title, "w3 code");
        assert_eq!(items[2].status, Status::Running);
        assert_eq!(items[2].meta, ["running", "1 attempt", "after w2"]);
        assert_eq!(items[4].status, Status::Needs, "waiting_decision");
        assert_eq!(items[0].status, Status::Verified);
        assert_eq!(r.attempts.len(), 4, "one row per attempt, none made up");
        assert!(r.attempts.windows(2).all(|w| w[0].title != w[1].title));
        let d = r.decisions.unwrap();
        assert_eq!(d.len(), 3);
        assert_eq!(r.pending, 2, "only a pending one counts");
        assert!(r.notes[0].starts_with("2 decisions waiting"));
        assert!(r.meta.contains(&"2 of 5 work items done".to_string()));
        let c = r.contract.unwrap();
        assert!(c.contains(&("Kind".into(), "Writes in a git worktree".into())));
        assert!(
            c.iter()
                .any(|(k, v)| k == "Check" && v.contains("cargo test"))
        );
        assert!(!r.read_only && r.report.is_empty());
    }

    #[test]
    fn loading_missing_and_unreachable_say_so_and_keep_the_last_rows() {
        let mut st = sample::state();
        assert_eq!(
            build(&st, "o_ci"),
            Body::Loading,
            "its detail has not landed"
        );
        assert!(matches!(build(&st, "nope"), Body::Gone(m) if m.contains("not in the list")));
        st.screen = Screen::Unreachable("socket closed".into());
        match build(&st, "o_auth") {
            Body::Ready { banner, .. } => assert!(banner.unwrap().contains("socket closed")),
            other => panic!("{other:?}"),
        }
        assert!(matches!(build(&st, "nope"), Body::Gone(m) if m.contains("socket closed")));
        let mut st = State::new(Screen::Loading);
        st.screen = Screen::Loading;
        assert_eq!(build(&st, "o_auth"), Body::Loading);
    }

    #[test]
    fn a_plan_that_could_not_be_read_is_an_error_not_an_empty_list() {
        let mut st = sample::state();
        if let Some(d) = st.detail.as_mut() {
            d.workers = InspectorContent::Unavailable {
                tab: InspectorTab::Workers,
                reason: "plan.get refused".into(),
            };
        }
        let r = ready(&st, "o_auth");
        assert_eq!(r.items, Err("plan.get refused".into()));
        assert!(r.attempts.is_empty());
    }

    #[test]
    fn a_read_only_goal_shows_the_report_rows_of_its_review_card_and_nothing_else() {
        let mut st = sample::state();
        let rows = vec![
            BriefRow {
                label: "Report (the worker's own words, untrusted)".into(),
                text: "all good [x](http://evil)".into(),
            },
            BriefRow {
                label: "Citations".into(),
                text: "a.txt 1-2 verified".into(),
            },
            BriefRow {
                label: "Why you".into(),
                text: "not part of the report".into(),
            },
        ];
        if let Some(d) = st.detail.as_mut() {
            d.current.as_mut().unwrap().kind = ContractKind::ReadOnly;
            if let Ok(c) = d.cards.as_mut() {
                c[0].kind = "report_review".into();
                c[0].rows = rows;
            }
        }
        let r = ready(&st, "o_auth");
        assert!(r.read_only);
        assert_eq!(
            r.report
                .iter()
                .map(|x| x.label.as_str())
                .collect::<Vec<_>>(),
            ["Report (the worker's own words, untrusted)", "Citations"]
        );
        assert!(
            r.contract
                .unwrap()
                .contains(&("Kind".into(), "Read-only. Never writes.".into()))
        );
    }

    #[test]
    fn the_cursor_starts_on_the_first_row_and_stops_at_the_ends() {
        assert_eq!(step(None, 0, true), None);
        assert_eq!(step(None, 3, true), Some(0));
        assert_eq!(step(Some(0), 3, false), Some(0));
        assert_eq!(step(Some(2), 3, true), Some(2));
        assert_eq!(step(Some(1), 3, true), Some(2));
    }

    #[test]
    fn finished_items_take_no_message() {
        for s in ["succeeded", "terminal_failure", "cancelled"] {
            assert!(item_finished(s));
        }
        for s in [
            "running",
            "waiting_decision",
            "blocked",
            "created",
            "retryable_failure",
        ] {
            assert!(!item_finished(s));
        }
    }
}
