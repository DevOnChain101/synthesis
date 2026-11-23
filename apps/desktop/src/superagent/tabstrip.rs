//! The title row right of the rail: the open frames as tabs (status glyph, title, close, drag to reorder; the
//! Super-Agent tab pinned), `+`, then at the right the mode buttons, the tool dock and the settings gear.

use gpui::{
    AnyElement, Context, InteractiveElement as _, IntoElement, ParentElement as _, Render,
    StatefulInteractiveElement as _, Styled as _, Window, WindowControlArea, div, prelude::*, px,
};

use super::frames::{Frame, MAX_SESSIONS};
use super::kit::{self, Status};
use super::rail::outcome_status;
use super::sidebar::{Segment, why_not};
use super::tokens::{self as t, hsla};
use super::{SuperAgentView, SwitchView, TITLE_ROW_H, ib};
use crate::ui::View;
use crate::widgets::chrome::icon;

/// What a tab drag carries: the tab's position.
#[derive(Clone)]
struct DragTab(usize);

/// The picture that follows the pointer while a tab is dragged.
struct DragGhost(String);

impl Render for DragGhost {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        kit::text(t::T2, t::W_MEDIUM, t::TEXT_STRONG)
            .px(px(t::S3))
            .h(px(32.))
            .flex()
            .items_center()
            .rounded(px(t::R_CONTROL))
            .bg(hsla(t::ELEVATED))
            .shadow(kit::raised())
            .child(self.0.clone())
    }
}

impl SuperAgentView {
    fn tab_glyph(&self, f: &Frame, cx: &gpui::App) -> AnyElement {
        let status = match f {
            Frame::Home => {
                return icon("super-agent", 16., hsla(t::TEXT_STRONG));
            }
            Frame::Outcome(id) | Frame::Worker(id, _) => self
                .outcomes
                .read(cx)
                .state
                .items
                .iter()
                .find(|i| i.row.id == *id)
                .map_or(Status::Idle, |i| outcome_status(&i.row)),
            Frame::Session(id) => match self.host.read(cx).info(id, cx) {
                Some(i) if i.needs > 0 => Status::Needs,
                Some(i) if i.busy => Status::Running,
                _ => Status::Idle,
            },
            Frame::NewGoal(_) | Frame::Picker => Status::Draft,
        };
        kit::text(t::T3, t::W_REGULAR, 0)
            .text_color(hsla(status.colour()))
            .w(px(16.))
            .flex()
            .justify_center()
            .child(status.glyph_and_word().0)
            .into_any_element()
    }

    fn tab(&self, i: usize, f: &Frame, cx: &mut Context<Self>) -> impl IntoElement {
        let on = i == self.frames.active_index();
        let title = self.title(f, cx);
        let pinned = i == 0;
        let ghost = title.clone();
        let tab = div()
            .id(("sa-tab", i))
            .debug_selector(move || format!("sa-tab-{i}"))
            .group("sa-tab")
            .flex()
            .items_center()
            .flex_basis(px(188.))
            .flex_shrink(1.)
            .min_w(px(124.))
            .h(px(32.))
            .gap(px(t::S2))
            .pl(px(t::S3))
            .rounded(px(t::R_CONTROL))
            .text_size(px(t::T2.0))
            .line_height(px(t::T2.1))
            .font_weight(t::W_MEDIUM)
            .whitespace_nowrap()
            .text_color(hsla(if on { t::TEXT_STRONG } else { t::TEXT }))
            .cursor_pointer()
            .map(|d| {
                if on {
                    d.bg(hsla(t::ELEVATED)).shadow(kit::raised())
                } else {
                    d.hover(|d| d.bg(t::hover()).text_color(hsla(t::TEXT_STRONG)))
                }
            })
            .on_click(cx.listener(move |v, _, _, cx| v.focus_tab(i, cx)))
            .child(self.tab_glyph(f, cx))
            .child(div().flex_1().min_w(px(0.)).truncate().child(title))
            .child(if pinned {
                div().w(px(t::S2)).flex_none().into_any_element()
            } else {
                div()
                    .id(("sa-tab-x", i))
                    .debug_selector(move || format!("sa-tab-{i}-x"))
                    .flex_none()
                    .size(px(20.))
                    .mr(px(t::S2))
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded(px(t::R_PILL))
                    .when(!on, |d| {
                        d.invisible().group_hover("sa-tab", |d| d.visible())
                    })
                    .hover(|d| d.bg(t::hover()).text_color(hsla(t::TEXT_STRONG)))
                    .on_click(cx.listener(move |v, _, _, cx| {
                        cx.stop_propagation();
                        v.close_tab(i, cx)
                    }))
                    .child("\u{d7}")
                    .into_any_element()
            });
        // the home neither moves nor is a drop target
        if pinned {
            tab.into_any_element()
        } else {
            tab.on_drag(DragTab(i), move |_, _, _, cx| {
                let t = ghost.clone();
                cx.new(|_| DragGhost(t))
            })
            .drag_over::<DragTab>(|d, _, _, _| d.bg(t::hover()))
            .on_drop(cx.listener(move |v, d: &DragTab, _, cx| v.reorder(d.0, i, cx)))
            .into_any_element()
        }
    }

    fn mode_button(
        &self,
        view: View,
        sel: &'static str,
        glyph: &'static str,
        tip: String,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let on = view == View::SuperAgent;
        let waiting = if on { self.waiting(cx) } else { 0 };
        ib(
            sel,
            glyph,
            tip,
            (32., 28.),
            (!on, on),
            cx.listener(move |_, _, _, cx| cx.emit(SwitchView(view))),
        )
        .relative()
        .children((waiting > 0).then(|| {
            kit::text(t::T1, t::W_MEDIUM, t::BG)
                .debug_selector(|| "sa-waiting-badge".to_string())
                .absolute()
                .top(px(-4.))
                .right(px(-4.))
                .min_w(px(16.))
                .h(px(16.))
                .px(px(4.))
                .flex()
                .items_center()
                .justify_center()
                .rounded(px(t::R_PILL))
                .bg(hsla(t::SOFT_BEIGE))
                .child(if waiting > 99 {
                    "99+".into()
                } else {
                    waiting.to_string()
                })
        }))
    }

    pub(super) fn title_row(
        &mut self,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let mac = cfg!(target_os = "macos");
        let mod_key = if mac { "\u{2318}" } else { "Ctrl+" };
        let tabs: Vec<_> = self
            .frames
            .tabs()
            .to_vec()
            .iter()
            .enumerate()
            .map(|(i, f)| self.tab(i, f, cx).into_any_element())
            .collect();
        let capped = self.frames.session_count() >= MAX_SESSIONS;
        let plus = ib(
            "sa-newtab",
            "plus",
            if capped {
                super::CAP_NOTE.into()
            } else {
                format!("New  ({mod_key}T)")
            },
            (32., 32.),
            (!capped, false),
            cx.listener(|v, _, w, cx| v.new_picker(w, cx)),
        );
        let a = self.side_avail(cx);
        let shown = self.side_shown(cx);
        let front = self.frames.active().clone();
        let tool = |seg: Segment, sel, glyph, keys: &str| {
            let tip = if a.has(seg) {
                format!("{}{keys}", seg.label())
            } else {
                format!("{}{keys}: {}", seg.label(), why_not(&front, seg))
            };
            ib(
                sel,
                glyph,
                tip,
                (30., 28.),
                (a.has(seg), shown == Some(seg)),
                cx.listener(move |v, _, _, cx| v.toggle_side(seg, cx)),
            )
        };
        let dock = div()
            .debug_selector(|| "sa-dock".to_string())
            .flex_none()
            .flex()
            .gap(px(2.))
            .p(px(3.))
            .rounded(px(t::R_PILL))
            .bg(t::well())
            .border_1()
            .border_color(t::stroke())
            .child(tool(
                Segment::Terminal,
                "sa-dock-terminal",
                "square-terminal",
                "  (Ctrl+`)",
            ))
            .child(tool(Segment::Browser, "sa-dock-browser", "globe", ""))
            .child(tool(
                Segment::Details,
                "sa-dock-details",
                "file",
                &format!("  ({mod_key}I)"),
            ));
        let modes = div()
            .flex_none()
            .flex()
            .gap(px(2.))
            .p(px(3.))
            .rounded(px(t::R_PILL))
            .bg(t::well())
            .border_1()
            .border_color(t::stroke())
            .child(self.mode_button(
                View::Tui,
                "sa-mode-tui",
                "tui",
                format!("TUI  ({mod_key}1)"),
                cx,
            ))
            .child(self.mode_button(
                View::SuperAgent,
                "sa-mode-super-agent",
                "super-agent",
                format!("Super Agent  ({mod_key}2)"),
                cx,
            ));
        div()
            .debug_selector(|| "sa-title-row".to_string())
            .flex_none()
            .h(px(TITLE_ROW_H))
            .flex()
            .items_center()
            .gap(px(t::S2))
            .pr(px(if mac { t::S3 } else { 0. }))
            .child(
                div()
                    .id("sa-tabstrip")
                    .flex()
                    .items_center()
                    .gap(px(t::S1))
                    .min_w(px(0.))
                    .overflow_x_scroll()
                    .track_scroll(&self.tab_scroll)
                    .children(tabs),
            )
            .child(plus)
            .child(
                // the drag region: the bar between the tabs and the buttons
                div()
                    .flex_1()
                    .h_full()
                    .min_w(px(t::S2))
                    .window_control_area(WindowControlArea::Drag),
            )
            .child(modes)
            .child(dock)
            .child(ib(
                "sa-gear",
                "gear",
                format!("Settings  ({mod_key},)"),
                (32., 32.),
                (true, false),
                cx.listener(|_, _, _, cx| cx.emit(SwitchView(View::Settings))),
            ))
            .when(!mac, |d| {
                d.child(div().w(px(t::S2)))
                    .child(crate::ui::Shell::window_button(WindowControlArea::Min))
                    .child(crate::ui::Shell::window_button(WindowControlArea::Max))
                    .child(crate::ui::Shell::window_button(WindowControlArea::Close))
            })
    }
}
