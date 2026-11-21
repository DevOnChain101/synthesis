//! The rail: a full-height column. Its first row (the title row's height) holds the traffic-light space, the logo,
//! back, forward and the sidebar toggle; then New, the Super-Agent item with the Needs you count, the Outcomes and the
//! Sessions. One tab stop with a roving cursor (Up, Down, Home, End, Enter); Cmd+B collapses it to icons.

use gpui::{
    Context, ElementId, InteractiveElement as _, IntoElement, ParentElement as _,
    StatefulInteractiveElement as _, Styled as _, Window, div, prelude::*, px,
};
use synthesis_view::outcomes::OutcomeRow;

use super::frames::Frame;
use super::kit::{self, Status};
use super::tokens::{self as t, hsla};
use super::{SuperAgentView, TITLE_ROW_H, ib};
use crate::widgets::chrome::icon;

pub const WIDE: f32 = 248.;
pub const MINI: f32 = 72.;
const TRAFFIC_LIGHTS_W: f32 = 78.;

/// What a rail row opens.
#[derive(Clone, Debug, PartialEq)]
pub enum RailKey {
    New,
    Open(Frame),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Section {
    Top,
    Outcomes,
    Sessions,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Glyph {
    Icon(&'static str),
    Status(Status),
}

#[derive(Clone, Debug, PartialEq)]
pub struct RailRow {
    pub key: RailKey,
    pub section: Section,
    pub label: String,
    pub glyph: Glyph,
    /// The needs-you count pill.
    pub badge: Option<usize>,
}

/// An outcome's glyph: waiting on the person wins, else its state word.
pub fn outcome_status(r: &OutcomeRow) -> Status {
    if r.needs_you.any() {
        return Status::Needs;
    }
    match r.state.as_str() {
        "running" | "active" => Status::Running,
        "blocked" => Status::Blocked,
        "verified" | "done" | "completed" => Status::Verified,
        "failed" => Status::Failed,
        "draft" => Status::Draft,
        "stopped" | "cancelled" => Status::Stopped,
        _ => Status::Idle,
    }
}

/// The focusable rows, in the order the keys walk them. `outcomes`: (id, title, status, needs-you count).
pub fn rows(
    waiting: usize,
    outcomes: &[(String, String, Status, usize)],
    sessions: &[(String, String)],
) -> Vec<RailRow> {
    let row = |key, section, label: &str, glyph, badge| RailRow {
        key,
        section,
        label: label.into(),
        glyph,
        badge,
    };
    let mut v = vec![
        row(RailKey::New, Section::Top, "New", Glyph::Icon("plus"), None),
        row(
            RailKey::Open(Frame::Home),
            Section::Top,
            "Super-Agent",
            Glyph::Icon("super-agent"),
            (waiting > 0).then_some(waiting),
        ),
    ];
    v.extend(outcomes.iter().map(|(id, title, s, n)| {
        row(
            RailKey::Open(Frame::Outcome(id.clone())),
            Section::Outcomes,
            title,
            Glyph::Status(*s),
            (*n > 0).then_some(*n),
        )
    }));
    v.extend(sessions.iter().map(|(id, title)| {
        row(
            RailKey::Open(Frame::Session(id.clone())),
            Section::Sessions,
            title,
            Glyph::Status(Status::Idle),
            None,
        )
    }));
    v
}

/// The cursor after `key` (clamped); `None` when the key is not a cursor key.
pub fn step(cursor: usize, n: usize, key: &str) -> Option<usize> {
    let last = n.saturating_sub(1);
    Some(match key {
        "up" => cursor.saturating_sub(1),
        "down" => (cursor + 1).min(last),
        "home" => 0,
        "end" => last,
        _ => return None,
    })
}

impl SuperAgentView {
    fn rail_rows(&self, cx: &gpui::App) -> Vec<RailRow> {
        let o = self.outcomes.read(cx);
        let outcomes: Vec<_> = o
            .state
            .items
            .iter()
            .map(|i| {
                let goal: String = i.row.goal.chars().take(200).collect();
                (
                    i.row.id.clone(),
                    synthesis_harness::sanitize_for_display(&goal),
                    outcome_status(&i.row),
                    i.row.needs_you.decisions_pending as usize,
                )
            })
            .collect();
        rows(self.waiting(cx), &outcomes, &self.sessions)
    }

    pub(super) fn activate_row(
        &mut self,
        key: &RailKey,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match key {
            RailKey::New => self.new_picker(window, cx),
            RailKey::Open(f) => {
                self.open(f.clone(), cx);
                window.focus(&self.focus, cx);
            }
        }
    }

    /// Keys of the rail while it holds the focus; true when the key was one of them.
    pub(super) fn rail_key(
        &mut self,
        key: &str,
        held: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let rows = self.rail_rows(cx);
        if let Some(c) = step(self.rail_cursor, rows.len(), key) {
            self.rail_cursor = c;
        } else if matches!(key, "enter" | "space" | " ") {
            // a held key (auto-repeat) never opens anything twice
            if let (false, Some(r)) = (held, rows.get(self.rail_cursor)) {
                self.activate_row(&r.key.clone(), window, cx);
            }
        } else {
            return false;
        }
        cx.notify();
        true
    }

    fn rail_row(
        &self,
        i: usize,
        r: &RailRow,
        collapsed: bool,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let current = matches!(&r.key, RailKey::Open(f) if f == self.frames.active());
        let ring = self.rail_focus.is_focused(window) && self.rail_cursor == i;
        let fg = if current { t::TEXT_STRONG } else { t::TEXT };
        let is_new = r.key == RailKey::New;
        let glyph = match &r.glyph {
            Glyph::Icon(n) => icon(n, 16., hsla(if is_new { t::TEXT_STRONG } else { fg })),
            Glyph::Status(s) => kit::text(t::T3, t::W_REGULAR, 0)
                .text_color(hsla(s.colour()))
                .w(px(16.))
                .flex()
                .justify_center()
                .child(s.glyph_and_word().0)
                .into_any_element(),
        };
        let key = r.key.clone();
        let id: ElementId = ("sa-rail", i).into();
        div()
            .id(id)
            .debug_selector(move || format!("sa-rail-{i}"))
            .relative()
            .flex_none()
            .flex()
            .items_center()
            .gap(px(t::S2))
            .h(px(if is_new { 36. } else { 32. }))
            .px(px(if collapsed {
                0.
            } else if is_new {
                t::S4
            } else {
                t::S3
            }))
            .when(collapsed, |d| d.w(px(44.)).justify_center())
            .when(!collapsed, |d| d.w_full())
            .rounded(px(if is_new { t::R_PILL } else { t::R_CONTROL }))
            .text_size(px(t::T3.0))
            .font_weight(if is_new { t::W_MEDIUM } else { t::W_REGULAR })
            .text_color(hsla(fg))
            .cursor_pointer()
            .when(is_new, |d| {
                d.bg(t::primary_fill())
                    .border_1()
                    .border_color(t::primary_rim())
                    .text_color(hsla(t::TEXT_STRONG))
                    .mb(px(t::S3))
            })
            .when(current, |d| d.bg(hsla(t::ELEVATED)).shadow(kit::raised()))
            .when(!current && !is_new, |d| {
                d.hover(|d| d.bg(t::hover()).text_color(hsla(t::TEXT_STRONG)))
            })
            .when(ring, |d| d.shadow(kit::ring()))
            .when(current && !collapsed, |d| {
                // the 3 px accent mark on the rail's left edge
                d.child(
                    div()
                        .absolute()
                        .left(px(-t::S2))
                        .top(px(9.))
                        .bottom(px(9.))
                        .w(px(3.))
                        .rounded_r(px(3.))
                        .bg(hsla(t::ACCENT)),
                )
            })
            .on_click(cx.listener(move |v, _, window, cx| {
                v.rail_cursor = i;
                v.activate_row(&key, window, cx);
            }))
            .child(glyph)
            .when(!collapsed, |d| {
                d.child(
                    div()
                        .flex_1()
                        .min_w(px(0.))
                        .truncate()
                        .child(r.label.clone()),
                )
                .children(is_new.then(|| kit::keycap("sa-rail-new-key", "N")))
            })
            .children(r.badge.map(|n| {
                kit::text(t::T2, t::W_MEDIUM, t::SOFT_BEIGE)
                    .debug_selector(move || format!("sa-rail-{i}-badge"))
                    .flex_none()
                    .flex()
                    .items_center()
                    .justify_center()
                    .min_w(px(20.))
                    .h(px(20.))
                    .px(px(6.))
                    .rounded(px(t::R_PILL))
                    .bg(t::alpha(t::SOFT_BEIGE, 0.16))
                    .when(collapsed, |d| d.absolute().right(px(2.)).top(px(2.)))
                    .child(n.to_string())
            }))
    }

    pub(super) fn rail(
        &mut self,
        collapsed: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let mac = cfg!(target_os = "macos");
        let rows = self.rail_rows(cx);
        let (back, fwd) = (self.can_nav(true, cx), self.can_nav(false, cx));
        let controls = [
            ib(
                "sa-back",
                "nav-back",
                "Back  (Alt+Left)".into(),
                (32., 32.),
                (back, false),
                cx.listener(|v, _, _, cx| v.nav(true, cx)),
            ),
            ib(
                "sa-forward",
                "nav-forward",
                "Forward  (Alt+Right)".into(),
                (32., 32.),
                (fwd, false),
                cx.listener(|v, _, _, cx| v.nav(false, cx)),
            ),
            ib(
                "sa-rail-toggle",
                "rail",
                format!("Sidebar  ({}B)", if mac { "\u{2318}" } else { "Ctrl+" }),
                (32., 32.),
                (true, false),
                cx.listener(|v, _, w, cx| v.toggle_rail(w, cx)),
            ),
        ];
        let logo = crate::brand::mark_img(24.).size(px(24.)).flex_none();
        let lights = div()
            .flex_none()
            .w(px(if mac && !collapsed {
                TRAFFIC_LIGHTS_W
            } else {
                0.
            }))
            .h(px(TITLE_ROW_H));
        let top = if collapsed {
            // icon rail: one column, the lights' row first
            div()
                .flex_none()
                .flex()
                .flex_col()
                .items_center()
                .gap(px(t::S1))
                .child(
                    div()
                        .h(px(TITLE_ROW_H))
                        .w_full()
                        .window_control_area(gpui::WindowControlArea::Drag),
                )
                .child(logo)
                .children(controls)
        } else {
            div()
                .flex_none()
                .flex()
                .items_center()
                .gap(px(t::S1))
                .h(px(TITLE_ROW_H))
                .child(lights)
                .child(logo)
                .child(
                    div()
                        .flex_1()
                        .h_full()
                        .window_control_area(gpui::WindowControlArea::Drag),
                )
                .children(controls)
        };
        let items = |section: Section, this: &Self, window: &Window, cx: &mut Context<Self>| {
            rows.iter()
                .enumerate()
                .filter(|(_, r)| r.section == section)
                .map(|(i, r)| {
                    this.rail_row(i, r, collapsed, window, cx)
                        .into_any_element()
                })
                .collect::<Vec<_>>()
        };
        let head = |label: &'static str| {
            kit::text(t::T2, t::W_MEDIUM, t::TEXT)
                .px(px(t::S3))
                .pt(px(t::S5))
                .pb(px(t::S1))
                .child(label)
        };
        let note = |s: &'static str| {
            kit::text(t::T2, t::W_REGULAR, t::TEXT)
                .px(px(t::S3))
                .py(px(t::S1))
                .child(s)
        };
        let mut body = div()
            .id("sa-rail-body")
            .track_focus(&self.rail_focus)
            .flex_1()
            .min_h(px(0.))
            .overflow_y_scroll()
            .flex()
            .flex_col()
            .gap(px(t::S1))
            .when(collapsed, |d| d.items_center().pt(px(t::S2)))
            .pb(px(t::S2))
            .children(items(Section::Top, self, window, cx));
        for (section, label, none) in [
            (Section::Outcomes, "Outcomes", "None yet"),
            (Section::Sessions, "Sessions", "None yet"),
        ] {
            let list = items(section, self, window, cx);
            if !collapsed {
                body = body.child(head(label));
                if list.is_empty() {
                    body = body.child(note(none));
                }
            }
            body = body.children(list);
        }
        div()
            .debug_selector(|| "sa-rail".to_string())
            .flex_none()
            .w(px(if collapsed { MINI } else { WIDE }))
            .h_full()
            .flex()
            .flex_col()
            .gap(px(t::S1))
            .px(px(if collapsed { 0. } else { t::S2 }))
            .pb(px(t::S2))
            .overflow_hidden()
            .child(top)
            .child(body)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn out(id: &str, n: usize) -> (String, String, Status, usize) {
        (id.into(), format!("goal {id}"), Status::Running, n)
    }

    #[test]
    fn rows_walk_new_home_outcomes_sessions_with_the_count_on_home() {
        let r = rows(
            3,
            &[out("a", 1), out("b", 0)],
            &[("s1".into(), "work".into())],
        );
        let keys: Vec<_> = r.iter().map(|r| r.key.clone()).collect();
        assert_eq!(
            keys,
            vec![
                RailKey::New,
                RailKey::Open(Frame::Home),
                RailKey::Open(Frame::Outcome("a".into())),
                RailKey::Open(Frame::Outcome("b".into())),
                RailKey::Open(Frame::Session("s1".into())),
            ]
        );
        assert_eq!(r[1].badge, Some(3));
        assert_eq!((r[2].badge, r[3].badge), (Some(1), None));
        assert_eq!(rows(0, &[], &[])[1].badge, None, "no count pill at zero");
    }

    #[test]
    fn the_cursor_clamps_and_jumps() {
        assert_eq!(step(0, 5, "up"), Some(0));
        assert_eq!(step(4, 5, "down"), Some(4));
        assert_eq!(step(2, 5, "home"), Some(0));
        assert_eq!(step(2, 5, "end"), Some(4));
        assert_eq!(step(2, 5, "x"), None);
        assert_eq!(step(0, 0, "end"), Some(0));
    }
}
