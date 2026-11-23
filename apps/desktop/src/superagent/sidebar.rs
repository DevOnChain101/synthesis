//! The right sidebar: Details, Terminal, Browser (phase C2, spec 5.3). One sidebar per window; its open state,
//! segment and width are the window's. The Terminal and Browser panes are the TUI view's own (`TuiView::embed_pane`),
//! one pair per session, kept by `SessionHost`; the person's shell and page never leave those panes: nothing here
//! reads a file, sends an event or touches the conductor (guard test below).

use std::rc::Rc;

use gpui::{
    AnyElement, App, Context, InteractiveElement as _, IntoElement, MouseButton,
    ParentElement as _, Styled as _, Window, div, prelude::*, px,
};

use super::SuperAgentView;
use super::frames::Frame;
use super::kit;
use super::tokens::{self as t, hsla};
use crate::tui_view::ChatTab;

pub const DEFAULT_W: f32 = 340.;
pub const MIN_W: f32 = 280.;
pub const MAX_W: f32 = 640.;
pub const STEP: f32 = 16.;
/// Below this window width the sidebar floats over the panel instead of sharing the row.
pub const OVERLAY_BELOW: f32 = 1100.;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Segment {
    Details,
    Terminal,
    Browser,
}

const ALL: [Segment; 3] = [Segment::Details, Segment::Terminal, Segment::Browser];

impl Segment {
    pub fn label(self) -> &'static str {
        match self {
            Self::Details => "Details",
            Self::Terminal => "Terminal",
            Self::Browser => "Browser",
        }
    }

    fn tab(self) -> Option<ChatTab> {
        match self {
            Self::Details => None,
            Self::Terminal => Some(ChatTab::Terminal),
            Self::Browser => Some(ChatTab::Browser),
        }
    }
}

/// Which segments the front frame offers (the availability table of spec 5.3).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Avail {
    pub details: bool,
    pub terminal: bool,
    pub browser: bool,
}

impl Avail {
    pub fn has(self, s: Segment) -> bool {
        match s {
            Segment::Details => self.details,
            Segment::Terminal => self.terminal,
            Segment::Browser => self.browser,
        }
    }

    pub fn first(self) -> Option<Segment> {
        ALL.into_iter().find(|s| self.has(*s))
    }

    fn list(self) -> Vec<Segment> {
        ALL.into_iter().filter(|s| self.has(*s)).collect()
    }
}

/// `remote`: the session runs in WSL, which has a shell through `wsl.exe` and no browser.
pub fn avail(f: &Frame, remote: bool) -> Avail {
    match f {
        Frame::Session(_) => Avail {
            details: true,
            terminal: true,
            browser: !remote,
        },
        Frame::Outcome(_) | Frame::Worker(..) => Avail {
            details: true,
            ..Avail::default()
        },
        Frame::Home | Frame::NewGoal(_) | Frame::Picker => Avail::default(),
    }
}

/// Why `s` is dimmed in front frame `f` (the tooltip).
pub fn why_not(f: &Frame, s: Segment) -> &'static str {
    match (f, s) {
        (Frame::Home, _) => "the home has no sidebar",
        (Frame::Session(_), Segment::Browser) => "no browser when the session runs in WSL",
        (Frame::Outcome(_), _) => "an outcome has Details only",
        (Frame::Worker(..), _) => {
            "a worker has Details only: a shell in a running attempt would change what is verified"
        }
        (_, Segment::Details) => "nothing to show for this tab",
        _ => "open a session of your own for this",
    }
}

/// The window's sidebar state.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Side {
    pub open: bool,
    pub seg: Segment,
    pub width: f32,
}

impl Default for Side {
    fn default() -> Self {
        Self {
            open: false,
            seg: Segment::Details,
            width: DEFAULT_W,
        }
    }
}

impl Side {
    /// The widest the sidebar may be in a window `win` px wide: 640 or 60 % of it, the smaller (never below 280).
    pub fn max_width(win: f32) -> f32 {
        MAX_W.min(win * 0.6).max(MIN_W)
    }

    pub fn set_width(&mut self, w: f32, win: f32) {
        self.width = w.clamp(MIN_W, Self::max_width(win));
    }

    /// The segment on screen: the chosen one, else the first the frame offers; `None`: closed or nothing to show.
    pub fn shown(&self, a: Avail) -> Option<Segment> {
        if !self.open {
            return None;
        }
        if a.has(self.seg) {
            Some(self.seg)
        } else {
            a.first()
        }
    }

    /// A title-bar icon or shortcut: open on `s`, or close when it is the one on screen. `false`: `s` is not offered.
    pub fn toggle(&mut self, s: Segment, a: Avail) -> bool {
        if !a.has(s) {
            return false;
        }
        if self.shown(a) == Some(s) {
            self.open = false;
        } else {
            (self.open, self.seg) = (true, s);
        }
        true
    }

    /// The segment header: the person picked `s` (ignored when not offered).
    pub fn pick(&mut self, s: Segment, a: Avail) {
        if a.has(s) {
            self.seg = s;
        }
    }
}

impl SuperAgentView {
    /// The id of the front frame when it is a hosted session.
    fn front_session(&self, cx: &App) -> Option<String> {
        match self.frames.active() {
            Frame::Session(id) if self.host.read(cx).ids().contains(id) => Some(id.clone()),
            _ => None,
        }
    }

    pub(crate) fn side_avail(&self, cx: &App) -> Avail {
        let f = self.frames.active();
        let hosted = self.front_session(cx);
        let remote = hosted
            .as_ref()
            .is_some_and(|id| self.host.read(cx).remote(id, cx));
        match (f, hosted) {
            // an ended session has no shell or browser to show, only its facts
            (Frame::Session(_), None) => Avail {
                details: true,
                ..Avail::default()
            },
            _ => avail(f, remote),
        }
    }

    pub(crate) fn side_shown(&self, cx: &App) -> Option<Segment> {
        self.side.shown(self.side_avail(cx))
    }

    /// Tells the hosted sessions which pane the sidebar shows: only the front session, only while this view is on
    /// screen, gets its Terminal or Browser pane (and with it the keys, the casting browser); the others hide theirs.
    pub(crate) fn sync_side(&mut self, cx: &mut Context<Self>) {
        let want = self
            .side_shown(cx)
            .and_then(Segment::tab)
            .filter(|_| self.visible);
        let front = self.front_session(cx).zip(want);
        self.host.update(cx, |h, cx| {
            h.set_sides(front.as_ref().map(|(id, k)| (id.as_str(), *k)), cx)
        });
        cx.notify();
    }

    /// A title-bar icon or shortcut.
    pub(crate) fn toggle_side(&mut self, s: Segment, cx: &mut Context<Self>) {
        let a = self.side_avail(cx);
        if self.side.toggle(s, a) {
            self.notice = None;
        } else {
            self.notice = Some(format!(
                "{} is not available here: {}.",
                s.label(),
                why_not(self.frames.active(), s)
            ));
        }
        self.sync_side(cx);
    }

    fn pick_segment(&mut self, i: usize, cx: &mut Context<Self>) {
        let a = self.side_avail(cx);
        if let Some(s) = a.list().get(i) {
            self.side.pick(*s, a);
        }
        self.sync_side(cx);
    }

    /// Closing the last inner tab of the shown pane closes the sidebar.
    pub(crate) fn close_side_if_empty(&mut self, cx: &mut Context<Self>) {
        let (Some(seg), Some(id)) = (self.side_shown(cx), self.front_session(cx)) else {
            return;
        };
        if seg
            .tab()
            .is_some_and(|k| self.host.read(cx).pane_closed(&id, k, cx))
        {
            self.side.open = false;
            self.sync_side(cx);
        }
    }

    /// Left widens, Right narrows (the grip is the sidebar's left edge), in steps of 16.
    pub(crate) fn resize_side(&mut self, by: f32, window: &Window, cx: &mut Context<Self>) {
        let w = self.side.width + by;
        self.side
            .set_width(w, f32::from(window.viewport_size().width));
        cx.notify();
    }

    /// `x`: the pointer while the grip is dragged (window px); the sidebar's right edge is 8 px inside the window.
    pub(crate) fn drag_side(&mut self, x: f32, window: &Window, cx: &mut Context<Self>) {
        let win = f32::from(window.viewport_size().width);
        self.side.set_width(win - t::S2 - x, win);
        cx.notify();
    }

    /// The sidebar (header, body, grip) when it has something to show; `overlay`: it floats over the panel.
    pub(super) fn sidebar(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let a = self.side_avail(cx);
        // not on screen: drawing a pane would hand it the keys again (`embed_pane` marks its side)
        let seg = self.side.shown(a).filter(|_| self.visible)?;
        let win = f32::from(window.viewport_size().width);
        let overlay = win < OVERLAY_BELOW;
        let labels: Vec<_> = a.list();
        let at = labels.iter().position(|s| *s == seg).unwrap_or(0);
        let me = cx.entity();
        let header = kit::segmented(
            "sa-side-seg",
            &labels.iter().map(|s| s.label()).collect::<Vec<_>>(),
            at,
            Some(Rc::new(move |i, _, app: &mut App| {
                me.update(app, |v, cx| v.pick_segment(i, cx))
            })),
        );
        let body = match (seg.tab(), self.front_session(cx)) {
            (Some(k), Some(id)) => self
                .host
                .update(cx, |h, cx| h.pane(&id, k, cx))
                .unwrap_or_else(|| div().into_any_element()),
            _ => self.details(cx),
        };
        let grip = div()
            .id("sa-side-grip")
            .debug_selector(|| "sa-side-grip".to_string())
            .track_focus(&self.grip_focus)
            .flex_none()
            .w(px(t::S2))
            .h_full()
            .cursor_col_resize()
            .hover(|d| d.bg(hsla(t::ACCENT).opacity(0.35)))
            .focus(|d| d.bg(hsla(t::ACCENT).opacity(0.6)))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|v, _, window, cx| {
                    v.grip_drag = true;
                    window.focus(&v.grip_focus, cx);
                }),
            );
        let panel = div()
            .debug_selector(|| "sa-side".to_string())
            .w(px(self.side.width))
            .flex_none()
            .h_full()
            .flex()
            .flex_col()
            .gap(px(t::S3))
            .p(px(t::S3))
            .overflow_hidden()
            .rounded(px(t::R_PANEL))
            .bg(hsla(if overlay { t::ELEVATED } else { t::SURFACE }))
            .border_1()
            .border_color(t::stroke())
            .when(overlay, |d| d.shadow(kit::raised()))
            .child(div().flex().child(header))
            .child(
                div()
                    .id("sa-side-body")
                    .flex_1()
                    .min_h(px(0.))
                    .flex()
                    .flex_col()
                    .overflow_hidden()
                    .child(body),
            );
        let row = div().h_full().flex().flex_none().child(grip).child(panel);
        Some(if overlay {
            div()
                .absolute()
                .top_0()
                .right_0()
                .bottom(px(t::S2))
                .pr(px(t::S2))
                .child(row)
                .into_any_element()
        } else {
            row.pb(px(t::S2)).pr(px(t::S2)).into_any_element()
        })
    }

    /// Read-only facts of the front frame that are really known; no model, cost or mode (the window does not report
    /// them for a session), so none is shown.
    fn details(&self, cx: &App) -> AnyElement {
        let row = |k: &'static str, v: String| {
            div()
                .flex()
                .flex_col()
                .gap(px(t::S1))
                .child(kit::text(t::T1, t::W_MEDIUM, t::TEXT).child(k))
                .child(kit::text(t::T3, t::W_REGULAR, t::TEXT_STRONG).child(v))
        };
        let clean = |s: &str| synthesis_harness::sanitize_for_display(s);
        let mut rows = Vec::new();
        match self.frames.active() {
            Frame::Session(id) => {
                if let Some((_, f)) = self.folders.iter().find(|(i, _)| i == id) {
                    rows.push(row("Folder", clean(&f.to_string_lossy())));
                }
                if let Some(i) = self.host.read(cx).info(id, cx) {
                    let status = if i.needs > 0 {
                        "Waiting for you"
                    } else if i.busy {
                        "Running"
                    } else {
                        "Idle"
                    };
                    rows.push(row("Status", status.into()));
                }
            }
            Frame::Outcome(id) => {
                let o = self.outcomes.read(cx);
                if let Some(i) = o.state.items.iter().find(|i| i.row.id == *id) {
                    rows.push(row("Goal", clean(&i.row.goal)));
                    rows.push(row("State", clean(&i.row.state)));
                }
            }
            _ => {}
        }
        let empty = rows.is_empty();
        div()
            .debug_selector(|| "sa-details".to_string())
            .flex()
            .flex_col()
            .gap(px(t::S4))
            .children(rows)
            .children(empty.then(|| {
                kit::text(t::T2, t::W_REGULAR, t::TEXT)
                    .child("No details are available for this yet.")
            }))
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s() -> Frame {
        Frame::Session("s1".into())
    }

    #[test]
    fn the_availability_table() {
        let all = |a: Avail| (a.details, a.terminal, a.browser);
        assert_eq!(all(avail(&Frame::Home, false)), (false, false, false));
        assert_eq!(all(avail(&s(), false)), (true, true, true));
        // WSL: a shell through wsl.exe, no browser
        assert_eq!(all(avail(&s(), true)), (true, true, false));
        assert_eq!(
            all(avail(&Frame::Outcome("o".into()), false)),
            (true, false, false)
        );
        assert_eq!(
            all(avail(&Frame::Worker("o".into(), "w".into()), false)),
            (true, false, false)
        );
        assert_eq!(all(avail(&Frame::Picker, false)), (false, false, false));
        let g = crate::superagent::frames::GoalKind::Writes;
        assert_eq!(avail(&Frame::NewGoal(g), false), Avail::default());
    }

    #[test]
    fn width_is_clamped_between_280_and_640_or_60_percent() {
        let mut d = Side::default();
        assert_eq!((d.open, d.seg, d.width), (false, Segment::Details, 340.));
        d.set_width(100., 1600.);
        assert_eq!(d.width, MIN_W);
        d.set_width(900., 1600.);
        assert_eq!(d.width, MAX_W);
        // 60 % of a 1000 px window is 600
        d.set_width(900., 1000.);
        assert_eq!(d.width, 600.);
        // a tiny window never pushes the maximum under the minimum
        d.set_width(900., 300.);
        assert_eq!(d.width, MIN_W);
    }

    #[test]
    fn icons_toggle_and_an_unavailable_one_does_nothing() {
        let a = avail(&s(), false);
        let mut d = Side::default();
        assert!(d.toggle(Segment::Terminal, a));
        assert_eq!(d.shown(a), Some(Segment::Terminal));
        // another icon only moves the segment
        assert!(d.toggle(Segment::Browser, a));
        assert_eq!((d.open, d.shown(a)), (true, Some(Segment::Browser)));
        // the same icon again closes
        assert!(d.toggle(Segment::Browser, a));
        assert_eq!(d.shown(a), None);
        // not offered: no change
        let out = avail(&Frame::Outcome("o".into()), false);
        let before = d;
        assert!(!d.toggle(Segment::Terminal, out));
        assert_eq!(d, before);
    }

    #[test]
    fn an_unavailable_segment_falls_back_to_the_first_offered_or_hides() {
        let mut d = Side::default();
        d.toggle(Segment::Terminal, avail(&s(), false));
        assert_eq!(
            d.shown(avail(&Frame::Outcome("o".into()), false)),
            Some(Segment::Details)
        );
        assert_eq!(d.shown(avail(&Frame::Home, false)), None);
        // the choice is kept for the next session
        assert_eq!(d.seg, Segment::Terminal);
        assert_eq!(d.shown(avail(&s(), false)), Some(Segment::Terminal));
        // WSL: the browser segment is gone, the shell stays
        d.pick(Segment::Browser, avail(&s(), true));
        assert_eq!(d.seg, Segment::Terminal);
    }

    /// T15 / spec 5.3 (4): the sidebar module sends nothing to a model, an event, a log, the conductor or the
    /// store, and reads no file. The panes it hosts are the TUI view's; this file only places them.
    #[test]
    fn the_sidebar_source_reads_no_file_and_reaches_no_model_path() {
        let src = include_str!("sidebar.rs");
        let code = src[..src.find("#[cfg(test)]").unwrap()]
            .lines()
            .filter(|l| !l.trim_start().starts_with("//"))
            .collect::<String>();
        for bad in [
            "std::fs",
            "fs::",
            "File::",
            "read_to_string",
            "Command",
            "Env::",
            "from_process",
            "Redactor",
            "PersonApproved",
            "conductor",
            "Conductor",
            "port::",
            "events",
            "emit(",
            "send(",
            "snapshot(",
            "write_to_clipboard",
            "read_from_clipboard",
        ] {
            assert!(!code.contains(bad), "sidebar.rs must not use `{bad}`");
        }
    }
}
