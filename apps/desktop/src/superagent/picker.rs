//! The kind picker as the mock's dialog (w 480, p 8, r 12 opaque): New session, New goal (writes), New goal
//! (read-only). The model is `superagent::outcomes::kind_picker` (state in `OutcomesView.state.picker`): nothing is
//! preselected, 1 to 3 or the arrows choose, Enter does nothing until a row is chosen, Esc closes.

use gpui::{
    AnyElement, Context, InteractiveElement as _, IntoElement, KeyDownEvent, ParentElement as _,
    StatefulInteractiveElement as _, Styled as _, Window, div, prelude::*, px,
};

use super::frames::{Frame, GoalKind};
use super::tokens::{self as t, hsla};
use super::{SuperAgentView, kit};
use crate::superagent::outcomes::kind_picker::{Kind, ROWS, refusal};

impl SuperAgentView {
    pub(crate) fn picker_open(&self, cx: &gpui::App) -> bool {
        self.outcomes.read(cx).state.picker.is_some()
    }

    /// N, Cmd+T and the New buttons: the picker opens with nothing chosen and the keys come to this view.
    pub fn new_picker(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.notice = None;
        self.outcomes.update(cx, |o, _| o.state.open_picker());
        window.focus(&self.focus, cx);
        cx.notify();
    }

    fn close_picker(&mut self, cx: &mut Context<Self>) {
        self.outcomes.update(cx, |o, _| o.state.picker = None);
        cx.notify();
    }

    fn picker_notice(&mut self, text: &str, cx: &mut Context<Self>) {
        self.outcomes.update(cx, |o, _| {
            if let Some(p) = o.state.picker.as_mut() {
                p.notice = Some(text.into());
            }
        });
        cx.notify();
    }

    /// Enter on the chosen row (or the folder button). With no row chosen it does nothing.
    pub(crate) fn picker_activate(&mut self, cx: &mut Context<Self>) {
        let (cursor, why) = {
            let st = &self.outcomes.read(cx).state;
            let cursor = st.picker.as_ref().and_then(|p| p.cursor);
            let why = cursor.and_then(|i| refusal(ROWS[i], &st.screen, st.read_only_goal));
            (cursor, why)
        };
        let Some(i) = cursor else { return };
        if let Some(why) = why {
            return self.picker_notice(why, cx);
        }
        match ROWS[i] {
            Kind::Session => {
                self.close_picker(cx);
                // a scratch folder (or the folder dialog for a remote engine); `OpenFolder` comes back to the shell
                self.outcomes.update(cx, |o, cx| o.new_session(cx));
            }
            k => {
                self.close_picker(cx);
                let g = if k == Kind::Goal {
                    GoalKind::Writes
                } else {
                    GoalKind::ReadOnly
                };
                self.open(Frame::NewGoal(g), cx);
            }
        }
    }

    fn session_in_folder(&mut self, cx: &mut Context<Self>) {
        self.outcomes.update(cx, |o, cx| o.session_in_folder(cx));
    }

    /// The picker is modal: every plain key is its own, chords go on to the shell.
    pub(super) fn picker_key(
        &mut self,
        ev: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let m = &ev.keystroke.modifiers;
        if m.control || m.alt || m.platform {
            return false;
        }
        match ev.keystroke.key.as_str() {
            // a held key (auto-repeat) never starts anything
            "enter" if ev.is_held => {}
            "enter" => self.picker_activate(cx),
            key => {
                self.outcomes.update(cx, |o, _| {
                    o.state.picker_key(key);
                });
                if !self.picker_open(cx) {
                    window.focus(&self.focus, cx);
                }
                cx.notify();
            }
        }
        true
    }

    pub(super) fn picker_overlay(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let st = &self.outcomes.read(cx).state;
        let p = st.picker.as_ref()?;
        let rows: Vec<_> = ROWS
            .iter()
            .enumerate()
            .map(|(i, k)| {
                let off = refusal(*k, &st.screen, st.read_only_goal);
                let on = p.cursor == Some(i);
                div()
                    .id(("sa-kind", i))
                    .debug_selector(move || format!("sa-kind-{i}"))
                    .flex()
                    .items_center()
                    .gap(px(t::S4))
                    .px(px(t::S4))
                    .py(px(t::S3))
                    .rounded(px(t::R_CONTROL))
                    .when(on, |d| d.bg(hsla(t::ELEVATED)))
                    .when(off.is_none(), |d| {
                        d.cursor_pointer().hover(|d| d.bg(t::hover()))
                    })
                    .on_click(cx.listener(move |v, _, _, cx| {
                        v.outcomes.update(cx, |o, _| o.state.pick(i));
                        cx.notify();
                    }))
                    .child(kit::keycap("sa-kind-key", (i + 1).to_string()))
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.))
                            .flex()
                            .flex_col()
                            .child(
                                kit::text(
                                    t::T3,
                                    t::W_MEDIUM,
                                    if off.is_some() {
                                        t::TEXT
                                    } else {
                                        t::TEXT_STRONG
                                    },
                                )
                                .child(k.label()),
                            )
                            .child(
                                kit::text(t::T2, t::W_REGULAR, t::TEXT)
                                    .whitespace_normal()
                                    .child(off.unwrap_or(k.blurb())),
                            ),
                    )
                    .children((*k == Kind::Session).then(|| {
                        kit::button(
                            "sa-kind-folder",
                            "Choose folder...",
                            kit::ButtonKind::Ghost,
                            false,
                            Some(Box::new(cx.listener(|v, _, _, cx| {
                                cx.stop_propagation();
                                v.session_in_folder(cx)
                            }))),
                        )
                    }))
            })
            .collect();
        let notice = p.notice.clone();
        let panel = div()
            .debug_selector(|| "sa-kind-picker".to_string())
            .w(px(480.))
            .max_w_full()
            .flex()
            .flex_col()
            .gap(px(t::S1))
            .p(px(t::S2))
            .rounded(px(t::R_PANEL))
            .bg(hsla(t::ELEVATED))
            .border_1()
            .border_color(t::stroke())
            .child(
                kit::text(t::T4, t::W_SEMIBOLD, t::TEXT_STRONG)
                    .px(px(t::S4))
                    .pt(px(t::S2))
                    .pb(px(t::S1))
                    .child("New"),
            )
            .children(rows)
            .children(notice.map(|n| {
                kit::text(t::T2, t::W_REGULAR, t::SOFT_BEIGE)
                    .debug_selector(|| "sa-kind-notice".to_string())
                    .whitespace_normal()
                    .px(px(t::S4))
                    .child(synthesis_harness::sanitize_for_display(&n))
            }))
            .child(
                kit::text(t::T1, t::W_REGULAR, t::TEXT)
                    .px(px(t::S4))
                    .pb(px(t::S2))
                    .child(
                        "Up/Down, 1-3 or a click choose, Enter starts the chosen row, Esc closes.",
                    ),
            );
        Some(
            div()
                .id("sa-kind-scrim")
                .debug_selector(|| "sa-kind-scrim".to_string())
                .absolute()
                .inset_0()
                .occlude()
                .flex()
                .items_center()
                .justify_center()
                .p(px(t::S5))
                .bg(t::scrim())
                .child(panel)
                .into_any_element(),
        )
    }
}
