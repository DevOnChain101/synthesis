//! Outcomes in the TUI view (`docs/specs/desktop.md` "TUI view: outcomes"): the "outcomes" tab of the centre strip,
//! the Approvals panel on top of the right column, and the outcome decision popup above the prompt. Everything is
//! inert while `TuiView::outcomes` is `None` (`approvals::gate` said off or unsupported when the shell started):
//! no panel, tab, popup, worker thread or poll. The data is the Outcomes tab's own (`OutcomesView::state`, read
//! only here), a decision is answered through the same worker (`OutcomesView::send_effect`), and an outcome is
//! opened in the Outcomes view, never inspected here. This file depends on no orchestrator crate.

use gpui::{
    AnyElement, Context, Entity, FontWeight, InteractiveElement as _, IntoElement as _,
    ParentElement as _, StatefulInteractiveElement as _, Styled as _, div, prelude::*, px,
};
use synthesis_cli_ui_render::theme as t;

use super::activity::RunState;
use super::panels::{OnClick, dim, icon, strong, text, with_alpha};
use super::{ChatTab, TuiView};
use crate::approvals::{self, Ages, Kind, Out, Popup, Row};
use crate::live::now_ms;
use crate::outcomes_view::model::Screen;
use crate::outcomes_view::{OutcomesView, SENDING, render};
use crate::theme::hsla;
use crate::widgets::chrome::{Head, Pad, panel};

/// A click on an outcome of the tab asks the shell to open it in the Outcomes view (Cmd/Ctrl+2).
pub struct OpenOutcome(pub String);

impl gpui::EventEmitter<OpenOutcome> for TuiView {}

/// Rows the Approvals panel shows before it scrolls.
const PANEL_ROWS: f32 = 5.;
/// Two Enter presses closer than this are one held key (the keystroke hook has no `is_held`).
const HELD_MS: i64 = 150;

/// What the popup shows.
pub enum Shown {
    /// The outcome was asked for and its card has not come yet.
    Loading {
        outcome: String,
        goal: String,
    },
    Card(Box<Popup>, String),
}

/// The TUI view's side of the Outcomes link.
pub struct Wire {
    pub view: Entity<OutcomesView>,
    ages: Ages,
    tool_ages: Ages,
    /// Rows the sample adds (the shots); empty in a live view.
    pub extra_tools: Vec<Row>,
    /// A fixed clock (the shots); `None`: the real one.
    pub fixed_now: Option<i64>,
    pub shown: Option<Shown>,
    /// The key of the row Alt+A went to last.
    cursor: Option<String>,
    /// The row of the outcomes tab Up/Down moved to.
    list_sel: usize,
    last_enter: i64,
    _sub: gpui::Subscription,
}

impl Wire {
    pub fn seed_tool(&mut self, row: Row) {
        self.tool_ages.seed(&row.key, row.since);
        self.extra_tools.push(row);
    }

    pub fn seed_outcome(&mut self, id: &str, since: i64) {
        self.ages.seed(id, since);
    }
}

impl TuiView {
    /// Connects the view to the Outcomes link. Called once by the shell after `approvals::gate` said yes (and the
    /// link was started); a second call changes nothing.
    pub fn set_outcomes(&mut self, view: Entity<OutcomesView>, cx: &mut Context<Self>) {
        if self.outcomes.is_some() {
            return;
        }
        let sub = cx.observe(&view, |_, _, cx| cx.notify());
        self.outcomes = Some(Wire {
            view,
            ages: Ages::default(),
            tool_ages: Ages::default(),
            extra_tools: vec![],
            fixed_now: None,
            shown: None,
            cursor: None,
            list_sel: 0,
            last_enter: 0,
            _sub: sub,
        });
        cx.notify();
    }

    fn now(&self) -> i64 {
        self.outcomes
            .as_ref()
            .and_then(|w| w.fixed_now)
            .unwrap_or_else(now_ms)
    }

    /// The tabs of the strip: the five of the design, and "outcomes" while outcomes are on.
    pub(super) fn tab_list(&self) -> Vec<ChatTab> {
        let mut v = ChatTab::ALL.to_vec();
        if self.outcomes.is_some() {
            v.push(ChatTab::Outcomes);
        }
        v
    }

    /// The count on the tab: outcomes that need the person.
    pub(super) fn outcomes_badge(&self, cx: &Context<Self>) -> Option<String> {
        let st = &self.outcomes.as_ref()?.view.read(cx).state;
        let n = st.items.iter().filter(|i| i.row.needs_you.any()).count();
        (st.screen == Screen::Ready && n > 0).then(|| n.to_string())
    }

    /// Per frame, before drawing: first-seen ages of what waits, and the popup follows the fresh card (a new
    /// revision or brief hash is a new card with a notice; a card no longer pending closes it).
    pub(super) fn sync_outcomes(&mut self, cx: &Context<Self>) {
        let tools = self.tool_keys();
        let now = self.now();
        let Some(w) = self.outcomes.as_mut() else {
            return;
        };
        let st = &w.view.read(cx).state;
        let ready = st.screen == Screen::Ready;
        let pending: Vec<&str> = if ready {
            st.items
                .iter()
                .filter(|i| i.row.needs_you.decisions_pending > 0)
                .map(|i| i.row.id.as_str())
                .collect()
        } else {
            vec![]
        };
        w.ages.observe(pending, now);
        w.tool_ages.observe(
            tools
                .iter()
                .map(String::as_str)
                .chain(w.extra_tools.iter().map(|r| r.key.as_str())),
            now,
        );
        let Some(shown) = w.shown.take() else { return };
        let outcome = match &shown {
            Shown::Loading { outcome, .. } => outcome.clone(),
            Shown::Card(p, _) => p.outcome.clone(),
        };
        let goal = st
            .items
            .iter()
            .find(|i| i.row.id == outcome)
            .map(|i| i.row.goal.clone());
        let Some(goal) = goal.filter(|_| ready) else {
            // the outcome is gone, or the link ended: nothing to answer
            return;
        };
        let landed = st.selected.as_deref() == Some(outcome.as_str())
            && st.detail.as_ref().is_some_and(|d| d.outcome == outcome);
        let card = st.cards().iter().find(|c| c.resolvable()).cloned();
        w.shown = match (shown, landed) {
            (Shown::Loading { .. }, false) => Some(Shown::Loading { outcome, goal }),
            // its detail landed: the card, or nothing when nothing is pending any more
            (Shown::Loading { .. }, true) => {
                card.map(|c| Shown::Card(Box::new(Popup::new(outcome, c, now)), goal))
            }
            (Shown::Card(p, _), false) => Some(Shown::Card(p, goal)),
            (Shown::Card(mut p, _), true) => card.map(|c| {
                // the answer came back (its reply replaced the "sending" line): the popup may answer again
                if p.ask.sending && st.ask.notice.as_deref() != Some(SENDING) {
                    p.ask.sending = false;
                    p.ask.notice = st.ask.notice.clone();
                }
                if p.card.id != c.id {
                    p = Box::new(Popup::new(outcome, c, now));
                } else {
                    p.update(c, now);
                }
                Shown::Card(p, goal)
            }),
        };
    }

    /// Sessions waiting for a tool permission (the open one, by its dialog, and the others by their status).
    fn tool_keys(&self) -> Vec<String> {
        self.tool_rows_raw().into_iter().map(|(k, _)| k).collect()
    }

    fn tool_rows_raw(&self) -> Vec<(String, String)> {
        self.snap
            .running
            .iter()
            .enumerate()
            .filter(|(_, r)| r.state == RunState::Approval && !r.peer)
            .map(|(i, r)| {
                let key = if r.id.is_empty() {
                    format!("#{i}")
                } else {
                    r.id.clone()
                };
                (key, r.title.clone())
            })
            .collect()
    }

    /// The Approvals list: tool asks and outcome decisions, the longest waiting first.
    pub fn approval_rows(&self, cx: &Context<Self>) -> Vec<Row> {
        let Some(w) = &self.outcomes else {
            return vec![];
        };
        let now = self.now();
        let mut tools: Vec<Row> = self
            .tool_rows_raw()
            .into_iter()
            .map(|(key, subject)| Row {
                since: w.tool_ages.since(&key).unwrap_or(now),
                key,
                kind: Kind::Tool,
                subject,
            })
            .collect();
        tools.extend(w.extra_tools.iter().cloned());
        let st = &w.view.read(cx).state;
        let decisions = if st.screen == Screen::Ready {
            approvals::decision_rows(&st.items, &w.ages, now)
        } else {
            vec![]
        };
        approvals::merge(tools, decisions)
    }

    /// Alt+A: the next row (the oldest first, then around) is opened.
    pub fn next_approval(&mut self, cx: &mut Context<Self>) {
        let rows = self.approval_rows(cx);
        let Some(w) = self.outcomes.as_mut() else {
            return;
        };
        let Some(i) = approvals::next(&rows, w.cursor.as_deref()) else {
            return;
        };
        w.cursor = Some(rows[i].key.clone());
        self.open_approval(&rows[i], cx);
    }

    /// A row was activated (a click, or Alt+A): a session is opened; a decision shows its popup.
    fn open_approval(&mut self, row: &Row, cx: &mut Context<Self>) {
        match row.kind {
            Kind::Tool => {
                if !row.key.starts_with('#')
                    && self.live.as_ref().is_some_and(|l| l.live.sid != row.key)
                {
                    self.open_session(&row.key.clone(), cx);
                }
            }
            Kind::Decision => self.open_decision(&row.key, &row.subject, cx),
        }
        cx.notify();
    }

    /// The popup of outcome `id`. When it is not the one the Outcomes tab has selected, it is selected there (and
    /// the popup says so) and the card is shown when its detail lands: nothing is selected silently.
    fn open_decision(&mut self, id: &str, goal: &str, cx: &mut Context<Self>) {
        let now = self.now();
        let Some(w) = self.outcomes.as_mut() else {
            return;
        };
        let (landed, card) = {
            let st = &w.view.read(cx).state;
            (
                st.selected.as_deref() == Some(id)
                    && st.detail.as_ref().is_some_and(|d| d.outcome == id),
                st.cards().iter().find(|c| c.resolvable()).cloned(),
            )
        };
        w.shown = Some(match (landed, card) {
            (true, Some(c)) => Shown::Card(Box::new(Popup::new(id.into(), c, now)), goal.into()),
            _ => {
                let id = id.to_string();
                w.view.update(cx, |v, cx| v.select_id(&id, cx));
                Shown::Loading {
                    outcome: id,
                    goal: goal.into(),
                }
            }
        });
    }

    /// The keys of this feature, before the engine's own: Alt+0 (the tab), Alt+A (next approval), and the popup's
    /// while it is open. Tool-permission dialogs keep the keyboard: while one is up only the Alt keys act.
    pub(super) fn outcome_key(&mut self, ks: &gpui::Keystroke, cx: &mut Context<Self>) -> bool {
        if self.outcomes.is_none() || !self.shown {
            return false;
        }
        let m = &ks.modifiers;
        if m.alt && !m.control && !m.platform && !m.shift {
            match ks.key.as_str() {
                "0" => {
                    self.select_tab(ChatTab::Outcomes, cx);
                    return true;
                }
                "a" => {
                    self.next_approval(cx);
                    return true;
                }
                _ => {}
            }
        }
        if m.control || m.platform || m.alt {
            return false;
        }
        let dialogs = self
            .live
            .as_ref()
            .is_some_and(|l| !l.live.dialogs.is_empty());
        if dialogs {
            return false;
        }
        let now = self.now();
        let key = ks.key.as_str();
        let held = {
            let Some(w) = self.outcomes.as_mut() else {
                return false;
            };
            let held = key == "enter" && now - w.last_enter < HELD_MS;
            if key == "enter" {
                w.last_enter = now;
            }
            held
        };
        // the popup
        let out = match self.outcomes.as_mut().and_then(|w| w.shown.as_mut()) {
            Some(Shown::Card(p, _)) if !(key == "tab" && m.shift) => p.key(key, held, now),
            // while the card loads only Esc acts
            Some(Shown::Loading { .. }) if key == "escape" => Out::Later,
            Some(Shown::Loading { .. }) => Out::Pass,
            _ => Out::Pass,
        };
        if out != Out::Pass {
            self.popup_out(out, cx);
            return true;
        }
        // the outcome list of the tab, while the prompt is empty
        if self.ts.tab == ChatTab::Outcomes && matches!(key, "up" | "down" | "enter") {
            let empty = self
                .live
                .as_ref()
                .is_none_or(|l| l.prompt.read(cx).value().is_empty());
            if empty {
                return self.list_key(key, cx);
            }
        }
        false
    }

    fn popup_out(&mut self, out: Out, cx: &mut Context<Self>) {
        let Some(w) = self.outcomes.as_mut() else {
            return;
        };
        match out {
            Out::Later => w.shown = None,
            Out::Send(e) => w.view.update(cx, |v, cx| v.send_effect(e, cx)),
            Out::Handled | Out::Pass => {}
        }
        cx.notify();
    }

    fn list_key(&mut self, key: &str, cx: &mut Context<Self>) -> bool {
        let Some(w) = self.outcomes.as_mut() else {
            return false;
        };
        let items = &w.view.read(cx).state.items;
        let n = items.len();
        if n == 0 {
            return false;
        }
        w.list_sel = w.list_sel.min(n - 1);
        match key {
            "up" => w.list_sel = w.list_sel.saturating_sub(1),
            "down" => w.list_sel = (w.list_sel + 1).min(n - 1),
            _ => {
                let id = items[w.list_sel].row.id.clone();
                cx.emit(OpenOutcome(id));
            }
        }
        cx.notify();
        true
    }

    // ------------------------------------------------------------------------------------- drawing

    /// The Approvals panel, or `None`: outcomes off, the panel switched off, or nothing waits.
    pub(super) fn approvals_panel(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        if self.outcomes.is_none() || self.hidden.approvals {
            return None;
        }
        let rows = self.approval_rows(cx);
        if rows.is_empty() {
            return None;
        }
        let now = self.now();
        let n = rows.len();
        let lines = rows.into_iter().enumerate().map(|(i, r)| {
            let (label, colour) = match r.kind {
                Kind::Tool => (r.kind.label(), hsla(t::WARN)),
                Kind::Decision => (r.kind.label(), hsla(t::ACCENT)),
            };
            let row = r.clone();
            div()
                .id(("appr-row", i))
                .debug_selector(move || format!("appr-row-{i}"))
                .flex()
                .items_center()
                .gap(px(8.))
                .py(px(1.))
                .rounded(px(4.))
                .cursor_pointer()
                .hover(|d| d.bg(with_alpha(gpui::white(), 0.05)))
                .on_click(cx.listener(move |this, _, _, cx| this.open_approval(&row, cx)))
                .child(
                    div()
                        .w(px(62.))
                        .flex_none()
                        .text_size(px(11.))
                        .text_color(colour)
                        .child(label),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w(px(0.))
                        .truncate()
                        .text_color(text())
                        .child(r.subject),
                )
                .child(
                    div()
                        .flex_none()
                        .text_color(dim())
                        .child(approvals::age_label(now - r.since)),
                )
        });
        let meta = div()
            .text_color(hsla(t::ACCENT))
            .child(format!("{n}"))
            .into_any_element();
        Some(
            panel(
                Head::new("p-approvals", "Approvals", "shield", Pad::CARD)
                    .max_body(PANEL_ROWS * 21.4 + 8.),
                Some(meta),
                div().flex().flex_col().children(lines),
            )
            .flex_none()
            .into_any_element(),
        )
    }

    /// The outcomes tab: the list, read-only. A click (or Enter) opens the outcome in the Outcomes view.
    pub(super) fn outcomes_tab(&self, cx: &mut Context<Self>) -> AnyElement {
        let body = div()
            .debug_selector(|| "tab-pane".to_string())
            .id("outcomes-pane")
            .flex_1()
            .min_h(px(0.))
            .overflow_y_scroll()
            .py(px(10.))
            .flex()
            .flex_col()
            .gap(px(6.));
        let Some(w) = &self.outcomes else {
            return body.into_any_element();
        };
        let (st, sel) = (&w.view.read(cx).state, w.list_sel);
        if st.screen != Screen::Ready {
            return body
                .child(crate::widgets::panes::note(
                    "Outcomes are not connected. Open the Outcomes view (Cmd/Ctrl+2) to see why.",
                ))
                .into_any_element();
        }
        if st.items.is_empty() {
            return body
                .child(crate::widgets::panes::note("No outcomes yet."))
                .into_any_element();
        }
        let rows = st.items.iter().enumerate().map(|(i, it)| {
            let id = it.row.id.clone();
            div()
                .id(("out-tab-row", i))
                .debug_selector(move || format!("out-tab-row-{i}"))
                .cursor_pointer()
                .flex()
                .flex_col()
                .gap(px(3.))
                .px(px(10.))
                .py(px(6.))
                .rounded(px(8.))
                .border_1()
                .border_color(if i == sel {
                    hsla(t::ACCENT)
                } else {
                    hsla(t::BORDER)
                })
                .hover(|d| d.bg(hsla(t::ELEVATED)))
                .on_click(cx.listener(move |_, _, _, cx| cx.emit(OpenOutcome(id.clone()))))
                .child(
                    div()
                        .text_color(strong())
                        .font_weight(FontWeight::MEDIUM)
                        .truncate()
                        .child(it.row.goal.clone()),
                )
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(6.))
                        .flex_wrap()
                        .child(
                            div()
                                .text_color(render::state_colour(&it.row.state))
                                .child(it.row.state.clone()),
                        )
                        .children(render::needs_badges(&it.row.needs_you))
                        .child(
                            div()
                                .ml_auto()
                                .text_size(px(11.))
                                .text_color(dim())
                                .child(render::budget_line(&it.budget)),
                        ),
                )
        });
        body.children(rows).into_any_element()
    }

    /// The decision popup above the prompt: a cream border (the Synthesis accent), not the amber of a tool
    /// permission. Full brief in the server's order, nothing preselected, two steps (choose, then Enter).
    pub(super) fn decision_popup(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let shown = self.outcomes.as_ref()?.shown.as_ref()?;
        let card = div()
            .debug_selector(|| "decision".to_string())
            .id("decision")
            .min_h(px(0.))
            .max_h(px((self.win_h * 0.5).clamp(160., 460.)))
            .overflow_y_scroll()
            .mx(px(12.))
            .mb(px(8.))
            .flex()
            .flex_col()
            .gap(px(8.))
            .p(px(12.))
            .bg(crate::theme::solid(t::SURFACE))
            .border_2()
            .border_color(hsla(t::ACCENT))
            .rounded(px(10.));
        let head = |goal: &str, sub: String| {
            div()
                .flex()
                .flex_col()
                .gap(px(2.))
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(8.))
                        .child(icon("shield", 14., hsla(t::ACCENT)))
                        .child(
                            div()
                                .text_size(px(15.))
                                .font_weight(FontWeight::SEMIBOLD)
                                .text_color(strong())
                                .truncate()
                                .child(format!("Decision: {goal}")),
                        ),
                )
                .child(div().text_color(dim()).child(sub))
        };
        Some(
            match shown {
                Shown::Loading { goal, .. } => card
                    .child(head(goal, "Loading the decision...".into()))
                    .child(div().text_color(text()).child(
                        "This outcome is now selected in the Outcomes tab too. The decision appears here when it arrives. Esc closes.",
                    )),
                Shown::Card(p, goal) => {
                    let c = &p.card;
                    let mut card = card.child(head(
                        goal,
                        format!(
                            "{} · revision {} · {}{}",
                            c.kind,
                            c.revision,
                            c.state,
                            if c.expires_at.is_empty() {
                                String::new()
                            } else {
                                format!(" · expires {}", c.expires_at)
                            }
                        ),
                    ));
                    // every brief field, in the server's order; Tab shows each one whole
                    for r in &c.rows {
                        let body = if p.full {
                            r.text.clone()
                        } else {
                            r.text.lines().next().unwrap_or("").to_string()
                        };
                        let more = !p.full && r.text.lines().count() > 1;
                        card = card.child(
                            div()
                                .flex()
                                .flex_col()
                                .child(div().text_size(px(11.)).text_color(dim()).child(r.label.clone()))
                                .child(div().text_color(text()).child(if more {
                                    format!("{body} ...")
                                } else {
                                    body
                                })),
                        );
                    }
                    match &p.ask.picker {
                        Some(pk) => {
                            for (i, o) in pk.options.iter().enumerate() {
                                let on = pk.cursor() == Some(i);
                                card = card.child(
                                    div()
                                        .id(("decision-opt", i))
                                        .debug_selector(move || format!("decision-opt-{i}"))
                                        .flex()
                                        .gap(px(8.))
                                        .px(px(8.))
                                        .py(px(2.))
                                        .rounded(px(6.))
                                        .cursor_pointer()
                                        .border_1()
                                        .border_color(if on {
                                            hsla(t::ACCENT)
                                        } else {
                                            hsla(t::BORDER)
                                        })
                                        .when(on, |d| d.bg(hsla(t::ELEVATED)))
                                        .hover(|d| d.bg(hsla(t::ELEVATED)))
                                        .on_click(cx.listener(move |this, _, _, cx| {
                                            if let Some(Shown::Card(p, _)) =
                                                this.outcomes.as_mut().and_then(|w| w.shown.as_mut())
                                            {
                                                p.ask.choose(i);
                                            }
                                            cx.notify();
                                        }))
                                        .child(
                                            div()
                                                .w(px(14.))
                                                .text_color(hsla(t::ACCENT))
                                                .child(if on { "›" } else { "" }),
                                        )
                                        .child(div().text_color(dim()).child(format!("{}.", i + 1)))
                                        .child(div().text_color(strong()).child(o.label.clone())),
                                );
                            }
                        }
                        None => {
                            card = card.child(div().text_color(hsla(t::WARN)).child(format!(
                                "This decision is {}: it cannot be answered here.",
                                c.state
                            )));
                        }
                    }
                    let send: OnClick = Box::new(cx.listener(|this, _, _, cx| {
                        let now = this.now();
                        let out = match this.outcomes.as_mut().and_then(|w| w.shown.as_mut()) {
                            Some(Shown::Card(p, _)) => p.key("enter", false, now),
                            _ => Out::Pass,
                        };
                        this.popup_out(out, cx);
                    }));
                    card.child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(10.))
                            .child(
                                div()
                                    .id("decision-send")
                                    .debug_selector(|| "decision-send".to_string())
                                    .cursor_pointer()
                                    .px(px(10.))
                                    .py(px(3.))
                                    .rounded(px(6.))
                                    .border_1()
                                    .border_color(hsla(t::ACCENT))
                                    .text_color(strong())
                                    .on_click(move |e, w, cx| send(e, w, cx))
                                    .child("Send answer"),
                            )
                            .child(div().text_color(dim()).child(
                                "↑↓ or 1-9 choose, then Enter · Tab full brief · Esc later · Alt+A next",
                            )),
                    )
                    .children(p.ask.notice.clone().map(|n| {
                        div().text_color(hsla(t::ACCENT_LIGHT)).child(n)
                    }))
                }
            }
            .into_any_element(),
        )
    }
}
