//! Drawing of the guided enable flow (`model::Guide` is its state): the disclosure dialog over the tab. The text is the build's own `ENABLE_NOTICE`, line for line,
//! never reworded here. Nothing starts focused; the buttons are drawn from the model's focus.

use gpui::{
    AnyElement, ClickEvent, Context, FontWeight, InteractiveElement as _, IntoElement,
    ParentElement as _, StatefulInteractiveElement as _, Styled as _, Window, div, prelude::*, px,
};
use synthesis_app::outcome_cmd::ENABLE_NOTICE;
use synthesis_cli_ui_render::theme as t;

use super::OutcomesView;
use super::model::{Btn, Step};
use crate::theme::{self, hsla};
use crate::widgets::chrome::{black_a, dim, rule, strong, text};

/// The title of the dialog and the label of its confirm button, per step.
pub(super) fn labels(step: Step) -> (&'static str, &'static str) {
    match step {
        Step::Setting => ("Turn on Super Agent", "Turn on Super Agent"),
        Step::Migrate => ("Migrate the session store", "Migrate the store"),
    }
}

impl OutcomesView {
    /// A click handler: the keys go to this view, then `f` runs.
    fn click(
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

    pub(super) fn guide_overlay(&self, cx: &Context<Self>) -> Option<AnyElement> {
        let g = self.state.guide.as_ref()?;
        let refused = self.state.gate_refusal.is_some();
        let (title, yes) = labels(g.step);
        let ring = |b: Btn| g.focus == Some(b);
        let confirm = div()
            .id("out-guide-confirm")
            .debug_selector(|| "out-guide-confirm".to_string())
            .cursor_pointer()
            .flex_none()
            .px(px(14.))
            .py(px(6.))
            .rounded(px(8.))
            .border_1()
            .font_weight(FontWeight::MEDIUM)
            .when(!g.busy, |d| {
                d.bg(hsla(t::ACCENT))
                    .border_color(hsla(if ring(Btn::Confirm) {
                        t::ACCENT_LIGHT
                    } else {
                        t::ACCENT
                    }))
                    .text_color(hsla(t::ACCENT_INK))
            })
            .when(g.busy, |d| d.border_color(hsla(t::RULE)).text_color(dim()))
            .when(ring(Btn::Confirm), |d| d.border_2())
            .on_click(self.click(cx, |this, cx| {
                let e = this.state.confirm_guide();
                this.run(e, cx);
            }))
            .child(if g.busy { "Working..." } else { yes });
        let cancel = div()
            .id("out-guide-cancel")
            .debug_selector(|| "out-guide-cancel".to_string())
            .cursor_pointer()
            .flex_none()
            .px(px(14.))
            .py(px(6.))
            .rounded(px(8.))
            .border_1()
            .border_color(hsla(if ring(Btn::Cancel) {
                t::ACCENT_LIGHT
            } else {
                t::BORDER
            }))
            .when(ring(Btn::Cancel), |d| d.border_2())
            .text_color(text())
            .on_click(self.click(cx, |this, _| this.state.close_guide()))
            .child("Cancel");
        let panel = div()
            .debug_selector(|| "out-guide".to_string())
            .w(px(680.))
            .max_w_full()
            .max_h_full()
            .flex()
            .flex_col()
            .overflow_hidden()
            .rounded(px(10.))
            .border_1()
            .border_color(hsla(t::BORDER))
            .bg(theme::fill(t::SURFACE))
            .child(
                div()
                    .flex_none()
                    .px(px(14.))
                    .py(px(9.))
                    .border_b_1()
                    .border_color(rule())
                    .text_size(px(15.))
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(strong())
                    .child(title),
            )
            .child(
                div()
                    .id("out-guide-text")
                    .debug_selector(|| "out-guide-text".to_string())
                    .flex_1()
                    .min_h(px(0.))
                    .overflow_y_scroll()
                    .flex()
                    .flex_col()
                    .gap(px(8.))
                    .px(px(14.))
                    .py(px(10.))
                    .text_color(text())
                    .children(
                        ENABLE_NOTICE
                            .lines()
                            .filter(|_| !refused)
                            .map(|l| div().child(l.to_string())),
                    ),
            )
            .children(g.error.as_ref().map(|e| {
                div()
                    .debug_selector(|| "out-guide-error".to_string())
                    .flex_none()
                    .px(px(14.))
                    .py(px(8.))
                    .border_t_1()
                    .border_color(rule())
                    .text_color(hsla(t::WARN))
                    .child(synthesis_harness::sanitize_for_display(e))
            }))
            .child(
                div()
                    .flex_none()
                    .flex()
                    .items_center()
                    .gap(px(10.))
                    .px(px(14.))
                    .py(px(9.))
                    .border_t_1()
                    .border_color(rule())
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.))
                            .text_size(px(11.))
                            .text_color(dim())
                            .child(
                                "Nothing is changed until you confirm. Tab moves between the buttons, Enter acts on the focused one, Esc cancels. Your confirmation is not remembered.",
                            ),
                    )
                    .child(cancel)
                    .children((!refused).then_some(confirm)),
            );
        Some(
            div()
                .debug_selector(|| "out-guide-scrim".to_string())
                .absolute()
                .inset_0()
                .occlude()
                .flex()
                .items_center()
                .justify_center()
                .p(px(24.))
                .bg(black_a(0.55))
                .child(panel)
                .into_any_element(),
        )
    }
}
