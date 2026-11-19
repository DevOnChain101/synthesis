//! Drawing of the Super Agent view. Everything shown is a field of a `synthesis_view::outcomes` model; the only
//! decisions made here are layout and colour. Palette: platinum with the accent, no blue or violet.

use gpui::{
    AnyElement, ClickEvent, Context, Div, FontWeight, Hsla, InteractiveElement as _, IntoElement,
    ParentElement as _, Render, StatefulInteractiveElement as _, Styled as _, Window, div,
    prelude::*, px,
};
use synthesis_cli_ui_render::theme as t;
use synthesis_view::outcomes::{
    ActivityTab, AttemptView, BriefRow, BudgetView, CriteriaView, DecisionCard, DiffTab,
    EvidenceTab, InspectorContent, InspectorTab, NeedsYou, OverviewTab, SandboxSource,
    WSL2_REFUSAL, WorkersTab, views::Amount,
};

use super::OutcomesView;
use super::form_view::small_button;
use super::model::{CentreTab, Item, MIGRATE_LINES, OFF_LINES, Pane, Screen};
use crate::live::now_ms;
use crate::theme::{self, hsla};
use crate::widgets::chrome::{self, Chip, dim, rule, strong, text, tracked, with_alpha};
use crate::{brand, widgets::panes::badge};

/// Colour of a state word the server sent. The word is always shown too: colour is never the only signal.
pub fn state_colour(word: &str) -> Hsla {
    hsla(state_rgb(word))
}

pub fn state_rgb(word: &str) -> t::Rgb {
    match word {
        "verified" | "completed" | "succeeded" | "finished" | "approved" => t::SUCCESS,
        "blocked" | "failed" | "terminal_failure" | "lost" | "rejected" => t::ERROR,
        "pending" | "waiting_decision" | "needs_more_info" | "rework_required"
        | "retryable_failure" | "expired" | "invalidated" | "cancel_requested" => t::WARN,
        "running" | "starting" | "verifying" | "leased" | "recovering" => t::ACCENT_LIGHT,
        _ => t::TEXT,
    }
}

/// What a decision card shows, as plain strings: the view draws exactly these, and a test proves that every
/// non-empty field of the brief is in them (`synthesis_view::outcomes::uncovered`).
pub struct CardLayout {
    pub title: String,
    pub state: String,
    pub meta: String,
    pub rows: Vec<BriefRow>,
    pub proof: String,
}

pub fn card_layout(c: &DecisionCard) -> CardLayout {
    CardLayout {
        title: c.kind.clone(),
        state: c.state.clone(),
        meta: format!(
            "decision {} \u{b7} revision {} \u{b7} expires {}",
            c.id,
            c.revision,
            if c.expires_at.is_empty() {
                "unknown"
            } else {
                &c.expires_at
            }
        ),
        rows: c.rows.clone(),
        proof: format!(
            "An answer is sent with revision {} and brief hash {}.",
            c.revision, c.brief_sha256
        ),
    }
}

impl CardLayout {
    /// Everything the card draws, one string (the coverage test reads this).
    #[cfg(test)]
    pub fn text(&self) -> String {
        let mut s = format!("{}\n{}\n{}\n", self.title, self.state, self.meta);
        for r in &self.rows {
            s.push_str(&format!("{}\n{}\n", r.label, r.text));
        }
        s.push_str(&self.proof);
        s
    }
}

pub(super) fn mono(s: impl Into<String>) -> Div {
    div().font_family(brand::MONO_FONT).child(s.into())
}

/// The lines of `s`, one box each (an empty line keeps its height).
fn lines(s: &str) -> Div {
    div().flex().flex_col().children(s.split('\n').map(|l| {
        div().child(if l.is_empty() {
            " ".to_string()
        } else {
            l.to_string()
        })
    }))
}

pub(super) fn heading(s: &str) -> Div {
    div()
        .mt(px(10.))
        .mb(px(4.))
        .font_family(brand::MONO_FONT)
        .child(tracked(&s.to_uppercase(), 11., 0.12, dim()))
}

/// `label  value` rows of a card: the label in a dim column.
pub(super) fn field(label: &str, value: impl IntoElement) -> Div {
    div()
        .flex()
        .gap(px(12.))
        .py(px(2.))
        .child(
            div()
                .w(px(112.))
                .flex_none()
                .text_color(dim())
                .child(label.to_string()),
        )
        .child(div().flex_1().min_w(px(0.)).text_color(text()).child(value))
}

fn money(a: Option<f64>) -> String {
    a.map_or_else(|| "unknown".into(), |v| format!("${v:.2}"))
}

/// "$1.20 of $5.00", or "budget unknown" when the server gave neither (the list's line, here and in the TUI view).
pub(crate) fn budget_line(b: &BudgetView) -> String {
    match (b.cost_usd.spent, b.cost_usd.limit) {
        (None, None) => "budget unknown".to_string(),
        (s, l) => format!("{} of {}", money(s), money(l)),
    }
}

pub(crate) fn needs_badges(n: &NeedsYou) -> Vec<AnyElement> {
    let mut v = vec![];
    if n.decisions_pending > 0 {
        v.push(
            badge(
                format!("{} to answer", n.decisions_pending),
                hsla(t::ACCENT_LIGHT),
            )
            .into_any_element(),
        );
    }
    if n.blocked {
        v.push(badge("blocked".into(), hsla(t::ERROR)).into_any_element());
    }
    if n.verified_awaiting_ack {
        v.push(badge("verified: ack".into(), hsla(t::SUCCESS)).into_any_element());
    }
    v
}

impl OutcomesView {
    /// A click handler: the keys go to this view, then `f` runs.
    pub(super) fn click(
        &self,
        cx: &Context<Self>,
        f: impl Fn(&mut Self, &mut Context<Self>) + 'static,
    ) -> impl Fn(&ClickEvent, &mut Window, &mut gpui::App) + 'static {
        cx.listener(move |this, _: &ClickEvent, w, cx| {
            w.focus(&this.focus, cx);
            f(this, cx);
            cx.notify();
        })
    }

    /// Like [`Self::click`] for a handler that needs the window.
    pub(super) fn click_window(
        &self,
        cx: &Context<Self>,
        f: impl Fn(&mut Self, &mut Window, &mut Context<Self>) + 'static,
    ) -> impl Fn(&ClickEvent, &mut Window, &mut gpui::App) + 'static {
        cx.listener(move |this, _: &ClickEvent, w, cx| {
            w.focus(&this.focus, cx);
            f(this, w, cx);
            cx.notify();
        })
    }

    fn column(&self, pane: Pane, id: &'static str) -> Div {
        div()
            .debug_selector(move || id.to_string())
            .min_w(px(0.))
            .h_full()
            .flex()
            .flex_col()
            .overflow_hidden()
            // flat: the rail and the context column sit on the surface tone, the centre on the window ground; one
            // hairline between the centre and the context column, no outline round a panel
            .when(pane != Pane::Centre, |d| d.bg(theme::fill(t::SURFACE)))
            .when(pane == Pane::Inspector, |d| {
                d.border_l_1().border_color(rule())
            })
    }

    // ------------------------------------------------------------------------------ list

    fn list(&self, cx: &Context<Self>) -> Div {
        let s = &self.state;
        let head = div()
            .flex_none()
            .flex()
            .items_center()
            .gap(px(8.))
            .px(px(12.))
            .pt(px(9.))
            .pb(px(6.))
            .child(div().font_family(brand::MONO_FONT).child(tracked(
                "OUTCOMES",
                12.,
                0.16,
                strong(),
            )))
            .child(
                div()
                    .ml_auto()
                    .text_color(dim())
                    .child(s.items.len().to_string()),
            )
            .child(small_button(
                "out-new",
                "New outcome",
                self.click(cx, |this, cx| {
                    let e = this.state.open_new();
                    this.run(e, cx);
                }),
            ));
        let rows = s
            .items
            .iter()
            .enumerate()
            .map(|(i, it)| self.list_row(i, it, cx));
        let empty = s.items.is_empty().then(|| {
            div()
                .px(px(4.))
                .text_color(dim())
                .child("No outcomes yet. Create one with `synt outcome create`.")
        });
        self.column(Pane::List, "out-left").child(head).child(
            div()
                .id("out-list")
                .flex_1()
                .min_h(px(0.))
                .overflow_y_scroll()
                .track_scroll(&self.scroll[0])
                .px(px(8.))
                .pb(px(8.))
                .flex()
                .flex_col()
                .gap(px(6.))
                .children(rows)
                .children(empty),
        )
    }

    fn list_row(&self, i: usize, it: &Item, cx: &Context<Self>) -> impl IntoElement {
        let on = self.state.selected.as_deref() == Some(it.row.id.as_str());
        let budget = budget_line(&it.budget);
        div()
            .id(("out", i))
            .debug_selector(move || format!("out-row-{i}"))
            .cursor_pointer()
            .flex()
            .flex_col()
            .flex_none()
            .gap(px(4.))
            .px(px(10.))
            .py(px(8.))
            .rounded(px(8.))
            // the accent border is the attention signal: an outcome that needs the owner
            .border_1()
            .border_color(if it.row.needs_you.any() {
                hsla(t::ACCENT)
            } else {
                gpui::transparent_black()
            })
            .when(on, |d| d.bg(hsla(t::ELEVATED)))
            .hover(|d| d.bg(hsla(t::ELEVATED)))
            .on_click(self.click(cx, move |this, cx| {
                this.state.pane = Pane::List;
                let e = this.state.select(i);
                this.run(e, cx);
            }))
            .child(
                div()
                    .text_color(strong())
                    .font_weight(FontWeight::MEDIUM)
                    .line_clamp(2)
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
                            .text_color(state_colour(&it.row.state))
                            .child(it.row.state.clone()),
                    )
                    .children(needs_badges(&it.row.needs_you)),
            )
            .child(div().text_size(px(11.)).text_color(dim()).child(budget))
    }

    // ----------------------------------------------------------------------------- centre

    fn centre(&self, cx: &Context<Self>) -> Div {
        let s = &self.state;
        let pending = s.selected_item().map(|i| i.row.needs_you.decisions_pending);
        let strip = tab_strip(
            CentreTab::ALL.iter().map(|t| t.label()),
            CentreTab::ALL
                .iter()
                .position(|t| *t == s.centre)
                .unwrap_or(0),
            "out-ctab",
            // the count of decisions that wait is attention: the accent
            |i| {
                (CentreTab::ALL[i] == CentreTab::Decisions)
                    .then_some(pending)
                    .flatten()
                    .filter(|n| *n > 0)
                    .map(|n| n.to_string())
            },
            &|i| {
                Box::new(self.click(cx, move |this, _| {
                    this.state.pane = Pane::Centre;
                    this.state.centre = CentreTab::ALL[i];
                }))
            },
        )
        .font_family(brand::MONO_FONT);
        let body = match s.centre {
            CentreTab::Goal => self.goal(cx),
            CentreTab::Decisions => self.decisions(cx),
            CentreTab::History => self.history(),
        };
        let h = &self.scroll[1];
        let more = s.centre == CentreTab::Decisions
            && f32::from(h.max_offset().y) + f32::from(h.offset().y) > 1.;
        self.column(Pane::Centre, "out-centre").child(strip).child(
            div()
                .id("out-centre-body")
                .debug_selector(|| "out-centre-body".to_string())
                .flex_1()
                .min_h(px(0.))
                .overflow_y_scroll()
                .track_scroll(&self.scroll[1])
                .p(px(14.))
                .child(body),
        )
        .children(more.then(|| {
            div()
                .flex_none()
                .px(px(14.))
                .py(px(4.))
                .border_t_1()
                .border_color(rule())
                .text_color(hsla(t::WARN))
                .child("\u{2193} more below: scroll (PageDown) to read all of it and reach the answers")
        }))
        // a decision that waits stays in sight on every tab
        .children(self.decision_popup(cx))
    }

    fn goal(&self, cx: &Context<Self>) -> Div {
        let Some(it) = self.state.selected_item() else {
            return div().text_color(dim()).child("No outcome selected.");
        };
        let mut d = div()
            .flex()
            .flex_col()
            .child(
                div()
                    .text_size(px(15.))
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(strong())
                    .child(it.row.goal.clone()),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .mt(px(4.))
                    .flex_wrap()
                    .child(mono(it.row.id.clone()).text_color(dim()))
                    .child(
                        div()
                            .text_color(state_colour(&it.row.state))
                            .child(it.row.state.clone()),
                    )
                    .child(
                        div()
                            .text_color(dim())
                            .child(format!("revision {}", it.row.revision)),
                    )
                    .children(needs_badges(&it.row.needs_you)),
            );
        match self.state.detail.as_ref().map(|x| &x.contract) {
            Some(Ok(c)) => d = d.child(contract_block(c)),
            Some(Err(e)) => {
                d = d.child(
                    div()
                        .mt(px(10.))
                        .text_color(hsla(t::WARN))
                        .child(format!("The contract cannot be shown: {e}")),
                )
            }
            None => d = d.child(div().mt(px(10.)).text_color(dim()).child("Loading...")),
        }
        let waiting = matches!(
            self.state.detail.as_ref().map(|d| &d.contract),
            Some(Ok(c)) if c.approved == Some(false)
        );
        d.child(
            div()
                .flex()
                .flex_wrap()
                .gap(px(8.))
                .mt(px(14.))
                .child(small_button(
                    "out-revise",
                    "Revise contract",
                    self.click(cx, |this, cx| {
                        let e = this.state.open_revise();
                        this.run(e, cx);
                    }),
                ))
                .children(waiting.then(|| {
                    small_button(
                        "out-review",
                        "Review and approve",
                        self.click(cx, |this, cx| {
                            let e = this.state.review_draft();
                            this.run(e, cx);
                        }),
                    )
                })),
        )
    }

    fn history(&self) -> Div {
        match self.state.detail.as_ref().map(|d| &d.history) {
            Some(InspectorContent::Activity(a)) => event_lines(a),
            Some(InspectorContent::Unavailable { reason, .. }) => div()
                .text_color(hsla(t::WARN))
                .child(format!("History is not available: {reason}")),
            _ => div().text_color(dim()).child("Loading..."),
        }
    }

    fn decisions(&self, cx: &Context<Self>) -> Div {
        let s = &self.state;
        let cards = match s.detail.as_ref().map(|d| &d.cards) {
            None => return div().text_color(dim()).child("Loading..."),
            // fail closed: a list the models refused is not shown in part and not answerable
            Some(Err(e)) => {
                return div().text_color(hsla(t::WARN)).child(format!(
                    "The decisions cannot be shown, so none can be answered here: {e}"
                ));
            }
            Some(Ok(c)) => c,
        };
        if cards.is_empty() {
            return div()
                .text_color(dim())
                .child("No decisions for this outcome.");
        }
        div()
            .flex()
            .flex_col()
            .gap(px(10.))
            .children(cards.iter().enumerate().map(|(i, c)| {
                if i == s.active {
                    self.card(c, cx).into_any_element()
                } else {
                    collapsed(
                        i,
                        c,
                        self.click(cx, move |this, _| {
                            this.state.pane = Pane::Centre;
                            this.state.show_card(i, now_ms());
                        }),
                    )
                    .into_any_element()
                }
            }))
    }

    /// The expanded decision: every non-empty field of the brief in the server's order, then the answers.
    fn card(&self, c: &DecisionCard, cx: &Context<Self>) -> Div {
        let s = &self.state;
        let lay = card_layout(c);
        let mut d = div()
            .debug_selector(|| "out-card".to_string())
            .flex()
            .flex_col()
            .gap(px(6.))
            .p(px(12.))
            .rounded(px(10.))
            .border_1()
            // attention: the decision that waits for the owner's answer
            .border_color(if s.ask.picker.is_some() {
                hsla(t::ACCENT)
            } else {
                gpui::transparent_black()
            })
            .bg(hsla(t::ELEVATED))
            .child(
                div()
                    .flex()
                    .items_start()
                    .gap(px(10.))
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.))
                            .text_size(px(15.))
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(strong())
                            .child(lay.title.clone()),
                    )
                    .child(badge(lay.state.clone(), state_colour(&lay.state))),
            )
            .child(mono(lay.meta.clone()).text_size(px(11.)).text_color(dim()));
        for r in &lay.rows {
            d = d.child(field(&r.label, lines(&r.text)));
        }
        d = d.child(
            mono(lay.proof.clone())
                .text_size(px(11.))
                .text_color(dim())
                .mt(px(4.)),
        );
        let Some(p) = &s.ask.picker else {
            return d.child(div().text_color(hsla(t::WARN)).child(format!(
                "This decision cannot be answered: it is {}.",
                c.state
            )));
        };
        d = d.child(
            div()
                .mt(px(6.))
                .text_color(text())
                .child("Choose one. Nothing is chosen for you."),
        );
        let alts = c.alternatives.len();
        let option = |i: usize| {
            let o = &p.options[i];
            let on = p.cursor() == Some(i);
            let ring = if on { hsla(t::ACCENT) } else { hsla(t::RULE) };
            div()
                .id(("out-opt", i))
                .debug_selector(move || format!("out-opt-{i}"))
                .cursor_pointer()
                .flex()
                .items_center()
                .gap(px(10.))
                .px(px(10.))
                .py(px(7.))
                .rounded(px(8.))
                .border_1()
                .border_color(ring)
                .bg(if on {
                    hsla(t::ELEVATED)
                } else {
                    with_alpha(hsla(t::SURFACE), 0.5)
                })
                .hover(|d| d.bg(hsla(t::ELEVATED)))
                .on_click(self.click(cx, move |this, _| {
                    this.state.pane = Pane::Centre;
                    this.state.choose(i);
                }))
                .child(chrome::dot(ring, on).size(px(12.)))
                .child(div().text_color(dim()).child(format!("{}", i + 1)))
                .child(mono(o.id.clone()).text_color(strong()))
                .when(o.label != o.id, |d| {
                    d.child(div().text_color(text()).child(o.label.clone()))
                })
        };
        d = d.children((0..alts).map(option));
        d = d.child(
            div()
                .flex()
                .flex_wrap()
                .gap(px(8.))
                .children((alts..p.options.len()).map(option)),
        );
        let ready = s.can_send(now_ms());
        d.child(
            div()
                .flex()
                .items_center()
                .gap(px(10.))
                .mt(px(6.))
                .child(
                    div()
                        .flex_1()
                        .min_w(px(0.))
                        .text_color(if s.ask.notice.is_some() {
                            hsla(t::WARN)
                        } else {
                            dim()
                        })
                        .child(s.ask.notice.clone().unwrap_or_else(|| {
                            "Step 2: press Enter or Send answer. Esc clears the choice.".into()
                        })),
                )
                .child(send_button(
                    ready,
                    self.click(cx, |this, cx| {
                        let e = this.state.send(now_ms());
                        this.run(e, cx);
                    }),
                )),
        )
    }

    // --------------------------------------------------------------------------- inspector

    fn inspector(&self, cx: &Context<Self>) -> Div {
        let s = &self.state;
        let strip = tab_strip(
            InspectorTab::ALL.iter().map(|t| t.label()),
            InspectorTab::ALL
                .iter()
                .position(|t| *t == s.itab)
                .unwrap_or(0),
            "out-itab",
            |_| None,
            &|i| {
                Box::new(self.click(cx, move |this, cx| {
                    this.state.pane = Pane::Inspector;
                    let e = this.state.set_itab(InspectorTab::ALL[i]);
                    this.run(e, cx);
                }))
            },
        );
        let body = match &s.inspector {
            Some(c) => inspector_body(c),
            None if s.selected.is_none() => div().text_color(dim()).child("No outcome selected."),
            None => div().text_color(dim()).child("Loading..."),
        };
        self.column(Pane::Inspector, "out-right")
            .child(strip)
            .child(
                div()
                    .id("out-right-body")
                    .flex_1()
                    .min_h(px(0.))
                    .overflow_y_scroll()
                    .track_scroll(&self.scroll[2])
                    .p(px(12.))
                    .child(body),
            )
    }

    // ----------------------------------------------------------------------------- frame

    fn hints(&self) -> Vec<(String, String)> {
        let h = |k: &str, d: &str| (k.to_string(), d.to_string());
        let s = &self.state;
        if s.form.is_some() {
            return vec![
                h("Tab", "next field"),
                h("Shift+Tab", "previous"),
                h("Enter", "on a button"),
                h("Esc", "close"),
            ];
        }
        if s.guide.is_some() {
            return vec![
                h("Tab", "next button"),
                h("Enter", "on a button"),
                h("Esc", "cancel"),
            ];
        }
        if matches!(s.screen, Screen::Off | Screen::NotReady(_)) {
            return vec![h("Enter", "continue"), h("r", "check again")];
        }
        if s.screen != Screen::Ready {
            return vec![h("r", "check again")];
        }
        let mut v = vec![h("Tab", "next column")];
        match (s.pane, s.centre) {
            (Pane::List, _) => v.extend([h("\u{2191}\u{2193}", "outcome"), h("Enter", "open")]),
            (Pane::Centre, CentreTab::Decisions) => v.extend([
                h("\u{2190}\u{2192}", "tabs"),
                h("\u{2191}\u{2193} 1-9", "choose"),
                h("Enter", "send"),
                h("Esc", "clear"),
                h("[ ]", "card"),
            ]),
            (Pane::Centre, _) => v.extend([
                h("\u{2190}\u{2192}", "tabs"),
                h("\u{2191}\u{2193}", "scroll"),
            ]),
            (Pane::Inspector, _) => v.extend([
                h("\u{2190}\u{2192} 1-5", "tab"),
                h("\u{2191}\u{2193}", "scroll"),
            ]),
        }
        v.extend([
            h("n", "new outcome"),
            h("v", "revise contract"),
            h("r", "refresh"),
        ]);
        v
    }

    fn message(&self, cx: &Context<Self>) -> Div {
        let (title, body): (&str, Vec<String>) = match &self.state.screen {
            Screen::Off => (
                "Super Agent is off",
                OFF_LINES.iter().map(|l| l.to_string()).collect(),
            ),
            Screen::Windows => (
                "Super Agent needs WSL2 on Windows",
                vec![WSL2_REFUSAL.to_string()],
            ),
            Screen::NotReady(e) => (
                "Super Agent needs one more step",
                MIGRATE_LINES
                    .iter()
                    .map(|l| l.to_string())
                    .chain([synthesis_harness::sanitize_for_display(e)])
                    .collect(),
            ),
            Screen::Unreachable(e) => (
                "The orchestrator cannot be reached",
                vec![
                    synthesis_harness::sanitize_for_display(e),
                    "Press r to try again.".into(),
                ],
            ),
            _ => ("Connecting to the orchestrator", vec![]),
        };
        let body: Vec<String> = body
            .into_iter()
            .chain(
                self.state
                    .notice
                    .iter()
                    .map(|n| synthesis_harness::sanitize_for_display(n)),
            )
            .collect();
        div()
            .size_full()
            .flex()
            .items_center()
            .justify_center()
            .child(
                div()
                    .debug_selector(|| "out-message".to_string())
                    .max_w(px(640.))
                    .flex()
                    .flex_col()
                    .gap(px(8.))
                    .p(px(20.))
                    .rounded(px(10.))
                    .bg(theme::fill(t::SURFACE))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(10.))
                            .child(brand::mark_img(28.).size(px(28.)))
                            .child(
                                div()
                                    .text_size(px(16.))
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .text_color(strong())
                                    .child(title.to_string()),
                            ),
                    )
                    .children(body.into_iter().map(|l| div().text_color(text()).child(l)))
                    .children(self.guide_open_button(cx)),
            )
    }
}

impl Render for OutcomesView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.sync_form_ui(window, cx);
        self.win_h = f32::from(window.viewport_size().height);
        self.sync_popup(window, cx);
        let vw = f32::from(window.viewport_size().width);
        let s = &self.state;
        let ready = s.screen == Screen::Ready;
        let (lw, rw) = ((vw * 0.19).clamp(210., 290.), (vw * 0.30).clamp(300., 430.));
        let chips = [if ready {
            Chip::new(
                "super agent",
                if self.env.is_some() {
                    "on"
                } else {
                    "sample data"
                },
                hsla(t::SUCCESS),
            )
            .dotted()
        } else {
            Chip::new("super agent", "not connected", hsla(t::WARN)).dotted()
        }];
        let main = if ready {
            div()
                .size_full()
                .flex()
                .children(
                    self.rail_open
                        .then(|| div().w(px(lw)).flex_none().h_full().child(self.list(cx))),
                )
                .child(div().flex_1().min_w(px(0.)).h_full().child(self.centre(cx)))
                .child(
                    div()
                        .w(px(rw))
                        .flex_none()
                        .h_full()
                        .child(self.inspector(cx)),
                )
        } else {
            self.message(cx)
        };
        div()
            .debug_selector(|| "outcomes".to_string())
            .id("outcomes")
            .relative()
            .track_focus(&self.focus)
            .key_context("Outcomes")
            .size_full()
            .flex()
            .flex_col()
            .text_size(px(13.))
            .text_color(text())
            .on_key_down(cx.listener(|this, ev: &gpui::KeyDownEvent, w, cx| {
                if this.on_key(ev, w, cx) {
                    cx.stop_propagation();
                }
            }))
            .child(div().flex_1().min_h(px(0.)).child(main))
            .child(chrome::footer(&self.hints(), &chips).border_color(rule()))
            .children(self.form_overlay(window, cx))
            .children(self.guide_overlay(cx))
    }
}

// -------------------------------------------------------------------------- stateless parts

fn collapsed(
    i: usize,
    c: &DecisionCard,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut gpui::App) + 'static,
) -> impl IntoElement {
    div()
        .id(("out-card", i))
        .cursor_pointer()
        .flex()
        .items_center()
        .gap(px(10.))
        .px(px(12.))
        .py(px(9.))
        .rounded(px(10.))
        .bg(with_alpha(hsla(t::ELEVATED), 0.5))
        .hover(|d| d.bg(hsla(t::ELEVATED)))
        .on_click(on_click)
        .child(
            div()
                .flex_1()
                .min_w(px(0.))
                .text_color(strong())
                .child(c.kind.clone()),
        )
        .child(mono(c.id.clone()).text_size(px(11.)).text_color(dim()))
        .child(badge(c.state.clone(), state_colour(&c.state)))
}

/// A flat tab strip: no box, the active tab in the strong colour with a 2px underline; `badge(i)` is a count in the
/// accent (what waits for the owner).
fn tab_strip<'a>(
    labels: impl Iterator<Item = &'a str>,
    selected: usize,
    id: &'static str,
    badge: impl Fn(usize) -> Option<String>,
    click: &dyn Fn(usize) -> chrome::OnClick,
) -> Div {
    div()
        .flex_none()
        .flex()
        .h(px(34.))
        .px(px(6.))
        .children(labels.enumerate().map(|(i, label)| {
            let on = i == selected;
            div()
                .id((id, i))
                .relative()
                .flex()
                .items_center()
                .gap(px(6.))
                .px(px(9.))
                .h_full()
                .cursor_pointer()
                .text_size(px(12.))
                .text_color(if on { strong() } else { dim() })
                .when(!on, |d| d.hover(|d| d.text_color(text())))
                .when(on, |d| {
                    d.child(
                        div()
                            .absolute()
                            .left(px(9.))
                            .right(px(9.))
                            .bottom_0()
                            .h(px(2.))
                            .bg(strong()),
                    )
                })
                .on_click(click(i))
                .child(label.to_string())
                .children(badge(i).map(|n| div().text_color(hsla(t::ACCENT_LIGHT)).child(n)))
        }))
}

fn send_button(
    ready: bool,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut gpui::App) + 'static,
) -> impl IntoElement {
    div()
        .id("out-send")
        .debug_selector(|| "out-send".to_string())
        .cursor_pointer()
        .flex_none()
        .px(px(14.))
        .py(px(6.))
        .rounded(px(8.))
        .border_1()
        .font_weight(FontWeight::MEDIUM)
        .when(ready, |d| {
            d.bg(hsla(t::ACCENT))
                .border_color(hsla(t::ACCENT))
                .text_color(hsla(t::ACCENT_INK))
        })
        .when(!ready, |d| d.border_color(hsla(t::RULE)).text_color(dim()))
        .on_click(on_click)
        .child("Send answer")
}

fn yn(b: Option<bool>) -> &'static str {
    match b {
        Some(true) => "yes",
        Some(false) => "no",
        None => "unknown",
    }
}

fn extras(rows: &[BriefRow]) -> impl Iterator<Item = Div> + '_ {
    rows.iter().map(|r| {
        div()
            .mt(px(3.))
            .text_color(dim())
            .child(format!("{}: {}", r.label, r.text))
    })
}

pub(super) fn contract_block(c: &CriteriaView) -> Div {
    let mut d = div()
        .flex()
        .flex_col()
        .child(heading("Contract"))
        .child(div().text_color(text()).child(format!(
            "version {} \u{b7} approved {}",
            c.version.map_or_else(|| "unknown".into(), |v| v.to_string()),
            yn(c.approved)
        )))
        .child(mono(c.hash.clone()).text_size(px(11.)).text_color(dim()))
        .child(heading("Success criteria"));
    for k in &c.criteria {
        d = d.child(
            div()
                .py(px(3.))
                .child(
                    div()
                        .flex()
                        .gap(px(8.))
                        .child(mono(k.id.clone()).text_color(strong()))
                        .child(
                            div()
                                .flex_1()
                                .min_w(px(0.))
                                .text_color(text())
                                .child(k.text.clone()),
                        ),
                )
                .child(
                    div()
                        .ml(px(28.))
                        .text_size(px(11.))
                        .text_color(dim())
                        .child(format!(
                            "mandatory {} \u{b7} manual {} \u{b7} trust {} \u{b7} checks {}",
                            yn(k.mandatory),
                            yn(k.manual),
                            k.required_trust,
                            if k.check_ids.is_empty() {
                                "none".to_string()
                            } else {
                                k.check_ids.join(", ")
                            }
                        )),
                )
                .children(extras(&k.extra)),
        );
    }
    if !c.checks.is_empty() {
        d = d.child(heading("Independent checks"));
        d = d.children(
            c.checks
                .iter()
                .map(|k| mono(k.clone()).text_size(px(12.)).text_color(text())),
        );
    }
    d.children(extras(&c.extra))
}

fn event_lines(a: &ActivityTab) -> Div {
    if a.lines.is_empty() {
        return div().text_color(dim()).child("No events yet.");
    }
    div().flex().flex_col().gap(px(2.)).children(
        a.lines
            .iter()
            .map(|l| mono(l.clone()).text_size(px(11.)).text_color(text())),
    )
}

fn inspector_body(c: &InspectorContent) -> Div {
    match c {
        InspectorContent::Overview(o) => overview(o),
        InspectorContent::Workers(w) => workers(w),
        InspectorContent::Activity(a) => event_lines(a),
        InspectorContent::Diff(d) => diff(d),
        InspectorContent::Evidence(e) => evidence(e),
        InspectorContent::Unavailable { tab, reason } => div()
            .text_color(hsla(t::WARN))
            .child(format!("{} is not available: {reason}", tab.label())),
    }
}

fn amount(name: &str, a: &Amount, f: impl Fn(Option<f64>) -> String) -> Div {
    let mut d = div()
        .flex()
        .flex_col()
        .py(px(2.))
        .child(div().text_color(strong()).child(name.to_string()))
        .child(div().text_color(text()).child(format!(
            "spent {} \u{b7} limit {} \u{b7} remaining {}",
            f(a.spent),
            f(a.limit),
            f(a.remaining)
        )));
    if a.remaining_disagrees() {
        d = d.child(
            div()
                .text_color(hsla(t::WARN))
                .child("server-reported: does not match limit and spent"),
        );
    }
    d
}

fn budget_block(b: &BudgetView) -> Div {
    let num = |v: Option<f64>| v.map_or_else(|| "unknown".into(), |v| format!("{v}"));
    div()
        .flex()
        .flex_col()
        .child(heading("Budget (as the server reports it)"))
        .child(amount("cost", &b.cost_usd, money))
        .child(amount("wall time (s)", &b.wall_secs, num))
        .child(amount("tokens", &b.tokens, num))
        .child(div().py(px(2.)).text_color(text()).child(format!(
            "reserved for running attempts: {}",
            money(b.reserved_usd)
        )))
}

fn overview(o: &OverviewTab) -> Div {
    let r = &o.outcome;
    let n = &r.needs_you;
    div()
        .flex()
        .flex_col()
        .child(
            div()
                .flex()
                .items_center()
                .gap(px(8.))
                .child(mono(r.id.clone()).text_color(dim()))
                .child(
                    div()
                        .text_color(state_colour(&r.state))
                        .child(r.state.clone()),
                )
                .child(
                    div()
                        .text_color(dim())
                        .child(format!("revision {}", r.revision)),
                ),
        )
        .child(heading("Needs you"))
        .child(if n.any() {
            div()
                .flex()
                .flex_wrap()
                .gap(px(6.))
                .children(needs_badges(n))
        } else {
            div().text_color(dim()).child("Nothing waits for you.")
        })
        .child(contract_block(&o.criteria))
        .child(budget_block(&o.budget))
}

fn item_row(w: &synthesis_view::outcomes::WorkItemView) -> Div {
    div()
        .flex()
        .items_start()
        .gap(px(8.))
        .ml(px(w.layer.unwrap_or(0) as f32 * 14.))
        .py(px(2.))
        .child(chrome::dot(state_colour(&w.state), w.state != "created").mt(px(5.)))
        .child(
            div()
                .flex_1()
                .min_w(px(0.))
                .child(
                    div()
                        .flex()
                        .flex_wrap()
                        .gap(px(6.))
                        .child(mono(w.key.clone()).text_color(strong()))
                        .child(div().text_color(text()).child(w.kind.clone()))
                        .child(
                            div()
                                .text_color(state_colour(&w.state))
                                .child(w.state.clone()),
                        ),
                )
                .child(div().text_size(px(11.)).text_color(dim()).child(format!(
                        "retries used {} \u{b7} layer {} \u{b7} requires {}",
                        w.retries_used
                            .map_or_else(|| "unknown".into(), |n| n.to_string()),
                        w.layer.map_or_else(|| "unknown".into(), |n| n.to_string()),
                        if w.requires.is_empty() {
                            "nothing".to_string()
                        } else {
                            w.requires.join(", ")
                        }
                    )))
                .children(extras(&w.extra)),
        )
}

fn attempt(a: &AttemptView) -> Div {
    let sandbox = match a.sandbox.source {
        SandboxSource::ServerReported => {
            format!("sandbox: {}, {}", a.sandbox.backend, a.sandbox.guarantee)
        }
        SandboxSource::NotProvided => "sandbox: unknown (not provided)".into(),
        SandboxSource::Unreadable => "sandbox: unreadable".into(),
    };
    let known = |s: &str| {
        if s.is_empty() {
            "unknown".to_string()
        } else {
            s.to_string()
        }
    };
    div()
        .flex()
        .flex_col()
        .gap(px(2.))
        .p(px(8.))
        .mb(px(6.))
        .rounded(px(8.))
        .bg(hsla(t::ELEVATED))
        .child(
            div()
                .flex()
                .flex_wrap()
                .gap(px(8.))
                .child(mono(a.id.clone()).text_color(strong()))
                .child(div().text_color(dim()).child(format!(
                    "of {} \u{b7} attempt {}",
                    a.work_item_id,
                    a.ordinal.map_or_else(|| "unknown".into(), |n| n.to_string())
                )))
                .child(div().text_color(state_colour(&a.state)).child(a.state.clone())),
        )
        .child(div().text_size(px(11.)).text_color(text()).child(format!("task {}", known(&a.task_status))))
        .child(mono(format!("session {}", known(&a.session_id))).text_size(px(11.)).text_color(dim()))
        .child(mono(format!("commit {}", known(&a.subject_commit))).text_size(px(11.)).text_color(dim()))
        .child(div().text_size(px(11.)).text_color(dim()).child(format!(
            "started {} \u{b7} ended {}",
            known(&a.started_at),
            known(&a.ended_at)
        )))
        .child(div().text_color(text()).child(sandbox))
        .when(a.sandbox.source == SandboxSource::ServerReported, |d| {
            d.child(
                div()
                    .text_size(px(11.))
                    .text_color(hsla(t::WARN))
                    .child("server-reported, not verified: the owner resolved it at launch and this app did not check it"),
            )
        })
        .children(extras(&a.sandbox.extra))
        .children(extras(&a.extra))
}

fn workers(w: &WorkersTab) -> Div {
    let mut d = div().flex().flex_col().child(heading(&format!(
        "Plan version {}",
        w.plan
            .plan_version
            .map_or_else(|| "unknown".into(), |v| v.to_string())
    )));
    if w.plan.cycle {
        d = d.child(
            div()
                .text_color(hsla(t::WARN))
                .child("dependency cycle: layers not shown"),
        );
    }
    // by layer when the server's edges give one, else the server's order
    let layered: Vec<_> = w.plan.layers().into_iter().flatten().collect();
    let all: Vec<_> = if layered.len() == w.plan.items.len() {
        layered
    } else {
        w.plan.items.iter().collect()
    };
    d = d.children(all.into_iter().map(item_row));
    for (i, r) in &w.plan.unknown_requires {
        d = d.child(div().text_color(hsla(t::WARN)).child(format!(
            "unknown requirement: {} requires {r}",
            w.plan.items[*i].key
        )));
    }
    if w.plan.unknown_requires_more > 0 {
        d = d.child(div().text_color(hsla(t::WARN)).child(format!(
            "{} more unknown requirements not shown",
            w.plan.unknown_requires_more
        )));
    }
    d = d.children(extras(&w.plan.extra));
    d = d.child(heading("Attempts"));
    if w.attempts.is_empty() {
        return d.child(div().text_color(dim()).child("No attempts yet."));
    }
    d.children(w.attempts.iter().map(attempt))
}

fn diff(d: &DiffTab) -> Div {
    let mut out = div()
        .flex()
        .flex_col()
        .child(
            mono(format!("{} -> {}", d.base_commit, d.commit))
                .text_size(px(11.))
                .text_color(dim()),
        )
        .child(div().mt(px(4.)).text_color(text()).child(format!(
            "{} files{}",
            d.total_files,
            if d.total_files > d.files.len() as u64 {
                " (list cut by the server)"
            } else {
                ""
            }
        )));
    for f in &d.files {
        let note = if f.secret {
            Some("secret-shaped path: the server sent the name only")
        } else if f.binary {
            Some("binary file: counts only")
        } else {
            None
        };
        let count = |n: Option<u64>| n.map_or_else(|| "?".to_string(), |n| n.to_string());
        let mut fd = div()
            .flex()
            .flex_col()
            .mt(px(10.))
            .child(
                div()
                    .flex()
                    .gap(px(8.))
                    .child(div().text_color(dim()).child(f.status.clone()))
                    .child(
                        mono(f.path.clone())
                            .flex_1()
                            .min_w(px(0.))
                            .text_color(strong()),
                    )
                    .child(
                        div()
                            .text_color(hsla(t::DIFF_ADD))
                            .child(format!("+{}", count(f.added))),
                    )
                    .child(
                        div()
                            .text_color(hsla(t::DIFF_REMOVE))
                            .child(format!("-{}", count(f.removed))),
                    ),
            )
            .children(note.map(|n| div().text_color(hsla(t::WARN)).text_size(px(11.)).child(n)));
        if let Some(p) = &f.patch {
            let total = p.split('\n').count();
            fd = fd.child(
                div()
                    .mt(px(4.))
                    .p(px(6.))
                    .rounded(px(6.))
                    .bg(hsla(t::BG))
                    .children(p.split('\n').take(PATCH_LINES).map(|l| {
                        let c = match l.chars().next() {
                            Some('+') => hsla(t::DIFF_ADD),
                            Some('-') => hsla(t::DIFF_REMOVE),
                            Some('@') => hsla(t::ACCENT_LIGHT),
                            _ => text(),
                        };
                        mono(if l.is_empty() {
                            " ".to_string()
                        } else {
                            l.to_string()
                        })
                        .text_size(px(11.))
                        .text_color(c)
                        .whitespace_nowrap()
                    }))
                    .when(total > PATCH_LINES, |d| {
                        d.child(div().text_color(hsla(t::WARN)).child(format!(
                            "{} more lines not shown here (the branch has them: git diff)",
                            total - PATCH_LINES
                        )))
                    }),
            );
        }
        if f.patch_truncated {
            fd = fd.child(
                div()
                    .text_color(hsla(t::WARN))
                    .text_size(px(11.))
                    .child("patch cut by the server"),
            );
        }
        out = out.child(fd);
    }
    out
}

/// Lines of one patch drawn at most (the server caps a patch at 16 KiB, about this many lines).
const PATCH_LINES: usize = 400;

fn evidence(e: &EvidenceTab) -> Div {
    if e.rows.is_empty() {
        return div().text_color(dim()).child("No evidence recorded yet.");
    }
    let known = |s: &str| {
        if s.is_empty() {
            "unknown".to_string()
        } else {
            s.to_string()
        }
    };
    div().flex().flex_col().gap(px(8.)).children(e.rows.iter().map(|r| {
        div()
            .flex()
            .flex_col()
            .gap(px(2.))
            .p(px(8.))
            .rounded(px(8.))
            .bg(hsla(t::ELEVATED))
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .gap(px(8.))
                    .child(mono(known(&r.check_id)).text_color(strong()))
                    .child(div().text_color(text()).child(format!("result as recorded: {}", r.result)))
                    .child(div().text_color(dim()).child(r.trust_class.clone())),
            )
            .child(div().text_size(px(11.)).text_color(dim()).child(format!(
                "{} \u{b7} of {} \u{b7} by {} \u{b7} exit {} \u{b7} criteria {} \u{b7} flags {}",
                r.id,
                known(&r.work_item_id),
                known(&r.producer),
                r.exit_code.map_or_else(|| "unknown".into(), |n| n.to_string()),
                if r.criteria_ids.is_empty() { "none".to_string() } else { r.criteria_ids.join(", ") },
                if r.flags.is_empty() { "none".to_string() } else { r.flags.join(", ") }
            )))
            .child(mono(format!("commit {}", known(&r.subject_commit))).text_size(px(11.)).text_color(dim()))
            .child(div().text_size(px(11.)).text_color(dim()).child(format!(
                "captured {} \u{b7} event-backed {}",
                known(&r.captured_at),
                yn(r.event_backed)
            )))
            .children(extras(&r.extra))
    }))
}
