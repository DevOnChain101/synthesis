//! Super Agent (docs/specs/super-agent-s1.md). `SuperAgentView` is what `View::SuperAgent` shows (phase B1): the
//! mock's chrome (rail, title-row tabs, panel) with real navigation, and placeholder frame bodies that the later
//! phases fill. Every later module is declared here as a stub so a phase adds files only inside its own directory.
//! `OutcomesView` stays the data and link owner; the old outcomes UI is not mounted (phase E deletes it).

pub mod chat;
pub mod frames;
pub mod home;
pub mod kit;
pub mod material;
mod picker;
pub mod rail;
pub mod session;
pub mod sidebar;
pub mod tabstrip;
pub mod tokens;

use gpui::{
    AnyElement, AnyView, App, ClickEvent, Context, Div, Entity, EventEmitter, FocusHandle,
    Focusable, InteractiveElement as _, IntoElement, KeyDownEvent, ParentElement as _, Render,
    ScrollHandle, Stateful, StatefulInteractiveElement as _, Styled as _, Window, div, prelude::*,
    px,
};

use crate::outcomes_view::OutcomesView;
use crate::ui::{TITLEBAR_H, Tip, View};
use crate::widgets::chrome::icon;
use frames::{Frame, Frames, GoalKind};
use kit::ButtonKind;
use tokens::{self as t, hsla};

/// The rail and the title row start at the window's top: the traffic lights sit in this 48 px row (the mock's grid).
pub const TITLE_ROW_H: f32 = 48.;
/// Below this width the rail is icon-only unless the person toggled it.
const MINI_BELOW: f32 = 900.;
pub const CAP_NOTE: &str = "At most 6 session tabs can be open. Close one first.";

/// The person asked for another view (the mode buttons, the gear): the shell switches.
pub struct SwitchView(pub View);

impl EventEmitter<SwitchView> for SuperAgentView {}

pub struct SuperAgentView {
    outcomes: Entity<OutcomesView>,
    pub(crate) frames: Frames,
    focus: FocusHandle,
    /// The rail's one tab stop; the row under `rail_cursor` has the ring while it holds the focus.
    rail_focus: FocusHandle,
    rail_cursor: usize,
    /// `Some`: the person toggled the rail (collapsed or not); `None`: it follows the window width.
    rail_user: Option<bool>,
    /// Sessions opened in this window (id, title), oldest first; closing the tab keeps the rail row.
    pub(crate) sessions: Vec<(String, String)>,
    /// Why the last action did not go on; cleared by the next one.
    notice: Option<String>,
    /// The mode button counts what waits for the person only in a live app (never the sample).
    pub(crate) count_waiting: bool,
    tab_scroll: ScrollHandle,
}

impl SuperAgentView {
    pub fn new(
        outcomes: Entity<OutcomesView>,
        count_waiting: bool,
        cx: &mut Context<Self>,
    ) -> Self {
        cx.observe(&outcomes, |_, _, cx| cx.notify()).detach();
        Self {
            outcomes,
            frames: Frames::default(),
            focus: cx.focus_handle(),
            rail_focus: cx.focus_handle().tab_stop(true),
            rail_cursor: 1,
            rail_user: None,
            sessions: Vec::new(),
            notice: None,
            count_waiting,
            tab_scroll: ScrollHandle::new(),
        }
    }

    /// The view was shown: the keys come here.
    pub fn shown(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        window.focus(&self.focus, cx);
    }

    pub(crate) fn waiting(&self, cx: &App) -> usize {
        if self.count_waiting {
            self.outcomes.read(cx).waiting_count()
        } else {
            0
        }
    }

    fn collapsed(&self, window: &Window) -> bool {
        self.rail_user
            .unwrap_or(f32::from(window.viewport_size().width) < MINI_BELOW)
    }

    // ------------------------------------------------------------------------------------------ frames

    /// The frames changed: the active tab is brought into view.
    fn changed(&mut self, cx: &mut Context<Self>) {
        self.notice = None;
        self.tab_scroll.scroll_to_item(self.frames.active_index());
        cx.notify();
    }

    pub fn open(&mut self, f: Frame, cx: &mut Context<Self>) {
        let r = self.frames.open(f);
        self.changed(cx);
        if r.is_err() {
            self.notice = Some(CAP_NOTE.into());
        }
    }

    pub(crate) fn focus_tab(&mut self, i: usize, cx: &mut Context<Self>) {
        self.frames.focus(i);
        self.changed(cx);
    }

    pub(crate) fn close_tab(&mut self, i: usize, cx: &mut Context<Self>) {
        self.frames.close(i);
        self.changed(cx);
    }

    pub(crate) fn reorder(&mut self, from: usize, to: usize, cx: &mut Context<Self>) {
        self.frames.reorder(from, to);
        self.changed(cx);
    }

    fn alive(outcomes: &Entity<OutcomesView>, cx: &App) -> impl Fn(&Frame) -> bool + use<> {
        let items: Vec<String> = outcomes
            .read(cx)
            .state
            .items
            .iter()
            .map(|i| i.row.id.clone())
            .collect();
        move |f| match f {
            Frame::Outcome(id) | Frame::Worker(id, _) => items.contains(id),
            _ => true,
        }
    }

    pub(crate) fn can_nav(&self, back: bool, cx: &App) -> bool {
        let alive = Self::alive(&self.outcomes, cx);
        if back {
            self.frames.can_back(alive)
        } else {
            self.frames.can_forward(alive)
        }
    }

    pub fn nav(&mut self, back: bool, cx: &mut Context<Self>) {
        let alive = Self::alive(&self.outcomes, cx);
        let _ = if back {
            self.frames.back(alive)
        } else {
            self.frames.forward(alive)
        };
        self.changed(cx);
    }

    /// A new session of the kind picker started in `p` (a folder the person chose or a scratch folder): a session
    /// frame opens in this view. Until the chat is hosted here (phase C1) the frame is a placeholder; the folder is
    /// its id.
    pub fn open_session_folder(&mut self, p: &std::path::Path, cx: &mut Context<Self>) {
        let id = p.to_string_lossy().into_owned();
        let title = p
            .file_name()
            .map_or_else(|| id.clone(), |n| n.to_string_lossy().into_owned());
        if !self.sessions.iter().any(|(i, _)| *i == id) {
            self.sessions.push((id.clone(), title));
        }
        self.open(Frame::Session(id), cx);
    }

    fn toggle_rail(&mut self, window: &Window, cx: &mut Context<Self>) {
        self.rail_user = Some(!self.collapsed(window));
        cx.notify();
    }

    pub(crate) fn title(&self, f: &Frame, cx: &App) -> String {
        let clean = |s: &str| {
            synthesis_harness::sanitize_for_display(&s.chars().take(200).collect::<String>())
        };
        match f {
            Frame::Home => "Super-Agent".into(),
            Frame::Outcome(id) => self
                .outcomes
                .read(cx)
                .state
                .items
                .iter()
                .find(|i| i.row.id == *id)
                .map_or_else(|| clean(id), |i| clean(&i.row.goal)),
            Frame::Worker(o, i) => clean(&format!("{o} {i}")),
            Frame::Session(id) => self
                .sessions
                .iter()
                .find(|(i, _)| i == id)
                .map_or_else(|| "Session".into(), |(_, t)| clean(t)),
            Frame::NewGoal(GoalKind::Writes) => "New goal".into(),
            Frame::NewGoal(GoalKind::ReadOnly) => "New read-only goal".into(),
            Frame::Picker => "New".into(),
        }
    }

    // ------------------------------------------------------------------------------------------ keys

    /// Shortcuts of the mock (DESIGN-NOTES 10.5). Cmd+, and Cmd+1/2 belong to the shell. Bare keys act only while the
    /// view or the rail holds the focus, so they never steal typing from a chat hosted inside.
    pub(crate) fn on_key(
        &mut self,
        ev: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let (k, m) = (&ev.keystroke, &ev.keystroke.modifiers);
        let key = k.key.as_str();
        if self.picker_open(cx) {
            return self.picker_key(ev, window, cx);
        }
        let bare = !(m.control || m.alt || m.platform || m.shift);
        // before the Cmd/Ctrl chords: on Windows and Linux Ctrl is the secondary key, and Ctrl+Tab is not one of them
        if m.control && !m.alt && !m.platform && key == "tab" {
            self.frames.cycle(if m.shift { -1 } else { 1 });
            self.changed(cx);
            return true;
        }
        if m.secondary(cx) && !m.alt {
            match key {
                "t" => self.new_picker(window, cx),
                "w" => self.close_tab(self.frames.active_index(), cx),
                "b" => self.toggle_rail(window, cx),
                _ => return false,
            }
            return true;
        }
        if m.alt && !m.control && !m.platform && !m.shift && matches!(key, "left" | "right") {
            self.nav(key == "left", cx);
            return true;
        }
        let mine = self.focus.is_focused(window) || self.rail_focus.is_focused(window);
        if !bare || !mine {
            return false;
        }
        match key {
            "n" => self.new_picker(window, cx),
            "escape" => self.nav(true, cx),
            _ if self.rail_focus.is_focused(window) => {
                return self.rail_key(key, ev.is_held, window, cx);
            }
            _ => return false,
        }
        true
    }
}

impl Focusable for SuperAgentView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

/// An icon button of the chrome: a round plate, 40 % when disabled, a tooltip that says what and why.
pub(super) fn ib(
    sel: &'static str,
    glyph: &'static str,
    tip: String,
    (w, h): (f32, f32),
    (enabled, pressed): (bool, bool),
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> Stateful<Div> {
    let fg = if pressed { t::TEXT_STRONG } else { t::TEXT };
    div()
        .id(sel)
        .debug_selector(move || sel.to_string())
        .flex_none()
        .w(px(w))
        .h(px(h))
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(t::R_PILL))
        .text_color(hsla(fg))
        .when(pressed, |d| d.bg(hsla(t::ELEVATED)).shadow(kit::raised()))
        .when(enabled, |d| {
            d.cursor_pointer()
                .hover(|d| d.bg(t::hover()).text_color(hsla(t::TEXT_STRONG)))
                .on_click(on_click)
        })
        .when(!enabled, |d| d.opacity(0.4))
        .tooltip(move |_, cx| -> AnyView { cx.new(|_| Tip(tip.clone())).into() })
        .child(icon(glyph, 16., hsla(fg)))
}

impl SuperAgentView {
    /// The body of a frame. Home and the others are placeholders until their phases (B2, B3, C1, C3).
    fn frame_body(&self, f: &Frame, cx: &mut Context<Self>) -> AnyElement {
        let col = || {
            div()
                .size_full()
                .flex()
                .flex_col()
                .items_center()
                .justify_center()
                .gap(px(t::S1))
                .px(px(t::S5))
        };
        match f {
            Frame::Home => col()
                .debug_selector(|| "sa-home".to_string())
                .child(
                    kit::text(t::T5, t::W_SEMIBOLD, t::TEXT_STRONG)
                        .child("What should we work on?"),
                )
                .child(
                    div()
                        .max_w(px(480.))
                        .pb(px(t::S3))
                        .text_size(px(t::T3.0))
                        .line_height(px(t::T3.1))
                        .text_color(hsla(t::TEXT))
                        .child("Start with a chat, or give Super-Agent a goal. Anything that needs a decision will wait for you on the home screen."),
                )
                .child(kit::button(
                    "sa-home-new",
                    "New",
                    ButtonKind::Primary,
                    false,
                    Some(Box::new(cx.listener(|v, _, w, cx| v.new_picker(w, cx)))),
                ))
                .into_any_element(),
            f => col()
                .debug_selector(|| "sa-frame".to_string())
                .child(
                    kit::card("sa-frame-card", false)
                        .child(kit::text(t::T4, t::W_SEMIBOLD, t::TEXT_STRONG).child(self.title(f, cx)))
                        .child(
                            kit::text(t::T2, t::W_REGULAR, t::TEXT)
                                .child("This frame is filled in a later phase."),
                        ),
                )
                .into_any_element(),
        }
    }

    fn panel(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let notice = self.notice.clone();
        div()
            .debug_selector(|| "sa-panel".to_string())
            .flex_1()
            .min_w(px(0.))
            .flex()
            .flex_col()
            .overflow_hidden()
            .rounded(px(t::R_PANEL))
            .bg(hsla(t::BG))
            .border_1()
            .border_color(t::stroke())
            .children(notice.map(|n| {
                kit::text(t::T2, t::W_REGULAR, t::TEXT_STRONG)
                    .debug_selector(|| "sa-notice".to_string())
                    .px(px(t::S5))
                    .py(px(t::S2))
                    .bg(hsla(t::ELEVATED))
                    .child(n)
            }))
            .child(
                div()
                    .flex_1()
                    .min_h(px(0.))
                    .child(self.frame_body(self.frames.active(), cx)),
            )
    }
}

impl Render for SuperAgentView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let collapsed = self.collapsed(window);
        let picker = self.picker_overlay(cx);
        div()
            .id("superagent")
            .debug_selector(|| "superagent".to_string())
            .track_focus(&self.focus)
            .on_key_down(cx.listener(|v, ev: &KeyDownEvent, window, cx| {
                if v.on_key(ev, window, cx) {
                    cx.stop_propagation();
                }
            }))
            .size_full()
            .flex()
            .relative()
            .bg(hsla(t::SURFACE))
            .text_color(hsla(t::TEXT_STRONG))
            .child(self.rail(collapsed, window, cx))
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.))
                    .flex()
                    .flex_col()
                    .child(self.title_row(window, cx))
                    .child(
                        div()
                            .flex_1()
                            .min_h(px(0.))
                            .flex()
                            .pr(px(t::S2))
                            .pb(px(t::S2))
                            .child(self.panel(cx)),
                    ),
            )
            .children(picker)
    }
}

/// The traffic lights of the mac window sit in the first row of the rail: its centre line.
pub fn traffic_light_y(super_agent: bool) -> f32 {
    (if super_agent { TITLE_ROW_H } else { TITLEBAR_H } - 12.) / 2.
}

#[cfg(test)]
mod tests;
