//! The window chrome both views share, 1:1 with `cli.html`: the top bar (`.topbar`, `.tchip`), the panel
//! (`.panel`, header + body), the tab strip (`.bottomtabs`, `.tab`) and the key hint footer (`.footer`).
//! Colours are the platinum tokens; geometry and type are the design's.

use gpui::{
    AnyElement, App, ClickEvent, Div, Hsla, InteractiveElement as _, IntoElement,
    ParentElement as _, StatefulInteractiveElement as _, Styled as _, Window, div, prelude::*, px,
    svg,
};
use synthesis_tui_render::theme as t;

use crate::{brand, theme::hsla};

pub fn text() -> Hsla {
    hsla(t::TEXT)
}
pub fn strong() -> Hsla {
    hsla(t::TEXT_STRONG)
}
pub fn dim() -> Hsla {
    hsla(t::MUTED)
}
/// `--line` of the design: the 1px rules between header, tabs and footer.
pub fn rule() -> Hsla {
    hsla(t::RULE)
}

/// A click on a row or a button.
pub type OnClick = Box<dyn Fn(&ClickEvent, &mut Window, &mut App) + 'static>;

/// An icon: the design's own shape when `cli.html` has it (`widgets::icons`), else the Lucide one.
pub fn icon(name: &str, size: f32, c: Hsla) -> gpui::AnyElement {
    match super::icons::for_lucide(name).and_then(|d| super::icons::svg_of(d, size, c)) {
        Some(s) => s.flex_none().into_any_element(),
        None => svg()
            .path(format!("icons/{name}.svg"))
            .size(px(size))
            .flex_none()
            .text_color(c)
            .into_any_element(),
    }
}

pub fn dot(c: Hsla, filled: bool) -> Div {
    let d = div().size(px(7.)).flex_none().rounded_full();
    if filled {
        d.bg(c)
    } else {
        d.border_1().border_color(c)
    }
}

/// Text with letter-spacing (`em` of the size): gpui has none, so one fixed-width cell per character, the
/// design's monospace advance (0.6em) plus the tracking, also after the last one. Separate boxes cannot use
/// the text's own width (gpui rounds each box's measured width up to a whole pixel) and lay out in whole
/// pixels, so the cells are integers chosen so that the sum follows the exact spacing.
pub fn tracked(s: &str, size: f32, em: f32, c: Hsla) -> Div {
    div()
        .flex()
        .flex_none()
        .whitespace_nowrap()
        .text_size(px(size))
        .text_color(c)
        .children(s.chars().enumerate().map(|(i, ch)| {
            // whole-pixel cells whose running sum follows i * (advance + tracking)
            let cell = size * (0.6 + em);
            let w = ((i + 1) as f32 * cell).round() - (i as f32 * cell).round();
            div().w(px(w)).flex_none().child(ch.to_string())
        }))
}

/// Header and body paddings of a panel (`.panel>header`, `.panel>.body`).
#[derive(Clone, Copy)]
pub struct Pad {
    /// Header padding: top, bottom, sides.
    pub head: (f32, f32, f32),
    /// Body padding at the bottom (the top is always 2px, the sides 12px).
    pub body_bottom: f32,
    /// Title size: 12px, 11px for the compact Topology header.
    pub title: f32,
}

impl Pad {
    /// `.panel>header{padding:9px 12px 6px}`, body bottom 10.
    pub const NORMAL: Pad = Pad {
        head: (9., 6., 12.),
        body_bottom: 10.,
        title: 12.,
    };
    /// `#view1 .col.l .panel`: header 7/4, body bottom 6.
    pub const LEFT: Pad = Pad {
        head: (7., 4., 12.),
        body_bottom: 6.,
        title: 12.,
    };
    /// `#view1 .status-context .panel`: header 6/3, body bottom 6.
    pub const CARD: Pad = Pad {
        head: (6., 3., 12.),
        body_bottom: 6.,
        title: 12.,
    };
    /// `@media (max-height:749px) .col.r>.panel.grow>header`.
    pub const COMPACT: Pad = Pad {
        head: (4., 2., 10.),
        body_bottom: 10.,
        title: 11.,
    };
}

/// Line box of the header: the title's 12px at 1.55 (the meta may be taller).
const HEAD_LINE: f32 = 18.6;

pub struct Head {
    pub id: &'static str,
    pub title: &'static str,
    pub icon: &'static str,
    pub pad: Pad,
    /// Height of the meta content when it is taller than the title line (the Usage select is 22.6).
    pub meta_h: f32,
    /// `max-height` of the body (the Tools & Skills list: 72), padding included.
    pub max_body: Option<f32>,
    /// Folding: whether the section is folded and what a click on its title does (no handler: not foldable).
    pub fold: Option<(bool, OnClick)>,
}

impl Head {
    pub fn new(id: &'static str, title: &'static str, icon: &'static str, pad: Pad) -> Self {
        Self {
            id,
            title,
            icon,
            pad,
            meta_h: HEAD_LINE,
            max_body: None,
            fold: None,
        }
    }

    /// The header's chevron and title fold the section; the meta on the right keeps its own clicks.
    pub fn fold(mut self, folded: bool, on_toggle: OnClick) -> Self {
        self.fold = Some((folded, on_toggle));
        self
    }

    pub fn max_body(mut self, h: f32) -> Self {
        self.max_body = Some(h);
        self
    }
}

/// A panel: bordered, rounded 8, a header (icon, uppercase tracked title, `meta` on the right) and a
/// scrolling body of `body`. The body is `flex: 1 1 auto`: it sizes to its content and gives way when
/// the column is short. Callers set the panel's own flex (`flex_none`, `flex_initial`, grow, min_h).
pub fn panel(h: Head, meta: Option<AnyElement>, body: impl IntoElement) -> Div {
    panel_scroll(h, meta, body, None)
}

/// [`panel`] whose body scroll position the caller can move (a selected row kept in view).
pub fn panel_scroll(
    h: Head,
    meta: Option<AnyElement>,
    body: impl IntoElement,
    scroll: Option<&gpui::ScrollHandle>,
) -> Div {
    let (pt, pb, side) = h.pad.head;
    let mut b = div()
        .id(h.id)
        .debug_selector(|| format!("{}-body", h.id))
        .flex_auto()
        .min_h(px(0.))
        .px(px(12.))
        .pt(px(2.))
        .overflow_y_scroll();
    if let Some(s) = scroll {
        b = b.track_scroll(s);
    }
    if let Some(m) = h.max_body {
        b = b.max_h(px(m - h.pad.body_bottom));
    }
    let folded = h.fold.as_ref().is_some_and(|f| f.0);
    let (id, title, icon_name, pad_title) = (h.id, h.title, h.icon, h.pad.title);
    let mut label = div()
        .id(format!("{id}-fold"))
        .flex()
        .items_center()
        .gap(px(8.))
        .child(icon(icon_name, 14., hsla(t::ACCENT)))
        .child(tracked(&title.to_uppercase(), pad_title, 0.16, strong()));
    if let Some((f, on_toggle)) = h.fold {
        label = label
            .cursor_pointer()
            .child(icon(
                if f { "chevron-right" } else { "chevron-down" },
                12.,
                dim(),
            ))
            .on_click(move |e, w, cx| on_toggle(e, w, cx));
    }
    let mut head = div()
        .debug_selector(|| format!("{}-head", h.id))
        .flex()
        .flex_none()
        .items_center()
        .gap(px(8.))
        .px(px(side))
        .pt(px(pt))
        .pb(px(pb))
        .h(px(pt + pb + h.meta_h))
        .child(label);
    if let Some(m) = meta {
        head = head.child(
            div()
                .ml_auto()
                .flex()
                .items_center()
                .gap(px(6.))
                .text_size(px(12.))
                .text_color(text())
                .child(m),
        );
    }
    div()
        .debug_selector(|| h.id.to_string())
        .flex()
        .flex_col()
        .min_w(px(0.))
        .overflow_hidden()
        .bg(hsla(t::SURFACE))
        .border_1()
        .border_color(hsla(t::BORDER))
        .rounded(px(8.))
        .child(head)
        .when(!folded, |d| {
            d.child(b.child(body))
                // the bottom padding sits outside the scroll area, or the next row shows through it
                .child(div().h(px(h.pad.body_bottom)).flex_none())
        })
}

/// One `.tchip` of the top bar or `.footer .st` span: optional dot, label, value.
pub struct Chip {
    pub label: String,
    pub value: String,
    pub colour: Hsla,
    pub dot: bool,
    /// The dot's colour when it differs from the text's (the footer's amber "files changed").
    pub dot_colour: Option<Hsla>,
}

impl Chip {
    pub fn new(label: &str, value: impl Into<String>, colour: Hsla) -> Self {
        Self {
            label: label.to_string(),
            value: value.into(),
            colour,
            dot: false,
            dot_colour: None,
        }
    }
    pub fn dotted(mut self) -> Self {
        self.dot = true;
        self
    }
    pub fn dot_colour(mut self, c: Hsla) -> Self {
        self.dot = true;
        self.dot_colour = Some(c);
        self
    }
}

fn chip(c: &Chip) -> Div {
    div()
        .flex()
        .flex_none()
        .items_center()
        .gap(px(6.))
        .px(px(14.))
        .h(px(22.))
        .border_l_1()
        .border_color(rule())
        .whitespace_nowrap()
        .when(c.dot, |d| {
            d.child(dot(c.dot_colour.unwrap_or(c.colour), true))
        })
        .when(!c.label.is_empty(), |d| d.child(c.label.clone()))
        .child(div().text_color(c.colour).child(c.value.clone()))
}

/// `.topbar`: 46px, brand, the chips (clipped, not wrapped, when the window is narrow), a spacer and
/// the right-hand chips (health, clock).
pub fn top_bar(left: &[Chip], right: &[Chip]) -> Div {
    div()
        .debug_selector(|| "header".to_string())
        .h(px(46.))
        .flex_none()
        .flex()
        .items_center()
        .px(px(16.))
        .border_b_1()
        .border_color(rule())
        .child(
            div()
                .flex()
                .flex_none()
                .items_center()
                .gap(px(12.))
                .child(gpui::img(brand::mark()).size(px(32.)).flex_none())
                .child(
                    tracked("SYNTHESIS", 14., 0.32, strong()).font_weight(gpui::FontWeight::BOLD),
                ),
        )
        .child(
            div()
                .flex()
                .ml(px(28.))
                .min_w(px(0.))
                .overflow_hidden()
                .children(left.iter().map(chip)),
        )
        .child(div().flex_1())
        .children(right.iter().map(chip))
}

/// `.footer`: 28px, key hints on the left (a key in strong, then what it does), status chips on the right.
pub fn footer(hints: &[(String, String)], status: &[Chip]) -> Div {
    let hint = |i: usize, (k, d): &(String, String)| {
        div()
            .flex()
            .flex_none()
            .px(px(10.))
            .when(i == 0, |d| d.pl(px(0.)))
            .when(i > 0, |d| d.border_l_1().border_color(rule()))
            .child(
                div()
                    .mr(px(if k.is_empty() { 0. } else { 6. }))
                    .text_color(strong())
                    .child(k.clone()),
            )
            .child(d.clone())
    };
    div()
        .debug_selector(|| "footer".to_string())
        .h(px(28.))
        .flex_none()
        .flex()
        .items_center()
        .px(px(12.))
        .border_t_1()
        .border_color(rule())
        .text_color(text())
        .whitespace_nowrap()
        .child(
            div()
                .flex()
                .flex_1()
                .min_w(px(0.))
                .overflow_hidden()
                .children(hints.iter().enumerate().map(|(i, h)| hint(i, h))),
        )
        .child(
            div()
                .flex()
                .flex_none()
                .children(status.iter().map(|c| chip(c).px(px(10.)))),
        )
}

/// One tab of [`tabs`].
pub struct Tab {
    pub label: &'static str,
    pub icon: &'static str,
    /// A count shown after the label (changed files, commands run).
    pub badge: Option<String>,
}

/// Width of `n` tracked characters (see [`tracked`]).
fn tracked_w(n: usize, size: f32, em: f32) -> f32 {
    (n as f32 * size * (0.6 + em)).round()
}

/// `.bottomtabs`: 32px strip, each tab icon + label (tracked .06em), 12px side padding; the selected one
/// on the elevated colour with a 2px accent line under it. `click(i)` is the handler of tab `i`.
///
/// `avail` is the strip's width. When the tabs together are wider, the design lets the tabs whose label has a
/// space shrink to their longest word and wrap ("file / explorer", clipped to the 31px tab); the others keep
/// their width and the strip overflows (clipped by the panel).
pub fn tabs(
    items: &[Tab],
    selected: usize,
    show_icons: bool,
    avail: f32,
    click: &dyn Fn(usize) -> OnClick,
) -> Div {
    let framed = |it: &Tab| matches!(it.label, "terminal" | "browser");
    let extra = |it: &Tab| {
        24. + if show_icons { 20. } else { 0. }
            + if framed(it) { 2. } else { 0. }
            + it.badge
                .as_ref()
                .map_or(0., |b| 6. + b.chars().count() as f32 * 7.5)
    };
    let natural: Vec<f32> = items
        .iter()
        .map(|it| extra(it) + tracked_w(it.label.chars().count(), 12.5, 0.06))
        .collect();
    let longest = |it: &Tab| {
        it.label
            .split(' ')
            .map(|w| w.chars().count())
            .max()
            .unwrap_or(0)
    };
    let mut width = natural.clone();
    let mut need = natural.iter().sum::<f32>() - avail;
    for (i, it) in items.iter().enumerate() {
        if need <= 0. {
            break;
        }
        if it.label.contains(' ') {
            let min = extra(it) + tracked_w(longest(it), 12.5, 0.06);
            let cut = need.min(natural[i] - min);
            width[i] = natural[i] - cut;
            need -= cut;
        }
    }
    div()
        .debug_selector(|| "tabs".to_string())
        .flex()
        .flex_none()
        .h(px(32.))
        .border_b_1()
        .border_color(rule())
        .bg(black_a(0.18))
        .children(items.iter().enumerate().map(|(i, it)| {
            let on = i == selected;
            let col = if on { strong() } else { text() };
            let wrapped = width[i] < natural[i];
            let label: Div = if wrapped {
                div().flex().flex_col().children(
                    it.label
                        .split(' ')
                        .map(|w| tracked(w, 12.5, 0.06, col).line_height(px(12.5))),
                )
            } else {
                tracked(it.label, 12.5, 0.06, col).line_height(px(12.5))
            };
            div()
                .id(("tab", i))
                .debug_selector(move || format!("tab-{i}"))
                .relative()
                .flex()
                .flex_none()
                .items_center()
                .gap(px(6.))
                .px(px(12.))
                .pb(px(1.))
                .h_full()
                .when(wrapped, |d| d.w(px(width[i])).overflow_hidden())
                .cursor_pointer()
                .when(framed(it), |d| {
                    d.border_l_1().border_r_1().border_color(rule())
                })
                .text_color(col)
                .when(on, |d| {
                    d.bg(hsla(t::ELEVATED)).child(
                        div()
                            .absolute()
                            .left_0()
                            .right_0()
                            .bottom_0()
                            .h(px(2.))
                            .bg(hsla(t::ACCENT)),
                    )
                })
                .on_click(click(i))
                .when(show_icons, |d| d.child(icon(it.icon, 14., col)))
                .child(label)
                .children(it.badge.clone().map(|n| div().text_color(dim()).child(n)))
        }))
}

/// The red of the "needs your approval" signal: with `motion` it blinks (on for 530 ms, dim for 530 ms),
/// without it it is steady.
pub fn attention_red(motion: bool, ms: i64) -> Hsla {
    let on = !motion || (ms.max(0) / 530) % 2 == 0;
    Hsla {
        a: if on { 1. } else { 0.25 },
        ..hsla(t::ERROR)
    }
}

pub fn black_a(a: f32) -> Hsla {
    Hsla { a, ..gpui::black() }
}

pub fn with_alpha(c: Hsla, a: f32) -> Hsla {
    Hsla { a, ..c }
}

/// Speed of the scrolling prompt (px/s) and the pause with the slot empty between two runs.
const MARQUEE_PX_S: f32 = 40.;
const MARQUEE_PAUSE_S: f32 = 1.;

/// One run of the scroll: the text starts with its left edge at the slot's right edge and has left through the
/// left edge after `slot_w + text_w` px.
fn marquee_travel(text_w: f32, slot_w: f32) -> f32 {
    text_w + slot_w
}

/// Seconds of one loop: the run, then the pause.
fn marquee_period(travel: f32) -> f32 {
    travel / MARQUEE_PX_S + MARQUEE_PAUSE_S
}

/// Offset from the slot's right edge at progress `d` (0..1 of one loop): runs left by `travel`, then stays
/// out of sight for the pause (only ever one copy of the text).
fn marquee_left(d: f32, travel: f32) -> f32 {
    let moving = (travel / MARQUEE_PX_S) / marquee_period(travel);
    -(d / moving).min(1.) * travel
}

/// `#chead`: icon, "Synthesis", the status word, the latest prompt (clipped), the context bar and `tail`
/// (folder and change counts). 36.4px: 8px padding around one line, and the 1px rule.
pub fn chat_head(
    status: &str,
    status_colour: Hsla,
    prompt: &str,
    ctx_pct: f32,
    tail: &str,
    width: f32,
    motion: bool,
) -> Div {
    let label = format!("Latest user prompt: {prompt}");
    let tw = label.chars().count() as f32 * 7.5;
    // what else the row holds (icon, "Synthesis", status, rule, context bar, tail, gaps) leaves the slot
    let fixed =
        14. + 7.5 * (9 + status.chars().count() + 1 + tail.chars().count()) as f32 + 54. + 84.;
    let slot_w = (width - fixed).max(0.);
    // `prompt-scroll`: only once a prompt was sent. With motion one copy enters at the slot's right edge, runs
    // left and leaves through its left edge, then the next run starts; without motion it is plain clipped text.
    let prompt_el: gpui::AnyElement = if prompt.is_empty() {
        div().into_any_element()
    } else if motion {
        let travel = marquee_travel(tw, slot_w);
        div()
            .absolute()
            .top_0()
            .left(gpui::relative(1.))
            .child(
                div()
                    .relative()
                    .whitespace_nowrap()
                    .child(label)
                    .with_animation(
                        "chead-marquee",
                        gpui::Animation::new(std::time::Duration::from_secs_f32(marquee_period(
                            travel,
                        )))
                        .repeat(),
                        move |el, d| el.left(px(marquee_left(d, travel))),
                    ),
            )
            .into_any_element()
    } else {
        div().truncate().child(label).into_any_element()
    };
    use gpui::AnimationExt as _;
    div()
        .debug_selector(|| "chead".to_string())
        .flex()
        .flex_none()
        .items_center()
        .gap(px(10.))
        .px(px(12.))
        .py(px(8.))
        .border_b_1()
        .border_color(rule())
        .whitespace_nowrap()
        .child(icon("sparkles", 14., hsla(t::ACCENT)))
        .child(
            div()
                .font_weight(gpui::FontWeight::SEMIBOLD)
                .text_color(hsla(t::ACCENT))
                .child("Synthesis"),
        )
        .child(div().text_color(status_colour).child(status.to_string()))
        .child(div().text_color(dim()).child("|"))
        .child(
            div()
                .relative()
                .flex_1()
                .min_w(px(0.))
                .h(px(19.375))
                .overflow_hidden()
                .text_color(dim())
                .child(prompt_el),
        )
        .child(
            div()
                .w(px(54.))
                .h(px(4.))
                .flex_none()
                .rounded(px(2.))
                .overflow_hidden()
                .bg(with_alpha(gpui::white(), 0.12))
                .child(
                    div()
                        .h_full()
                        .w(gpui::relative(ctx_pct.clamp(0., 100.) / 100.))
                        .rounded(px(2.))
                        .bg(hsla(t::ACCENT)),
                ),
        )
        .child(div().flex_none().text_color(dim()).child(tail.to_string()))
}

#[cfg(test)]
mod attention_tests {
    use super::*;

    #[test]
    fn the_red_blinks_with_motion_and_stays_without() {
        assert_eq!(attention_red(true, 0).a, 1.);
        assert_eq!(attention_red(true, 530).a, 0.25);
        assert_eq!(attention_red(true, 1060).a, 1.);
        assert_eq!(attention_red(false, 530).a, 1.);
    }
}

#[cfg(test)]
mod marquee_tests {
    use super::*;

    #[test]
    fn one_copy_runs_from_the_right_edge_out_through_the_left_then_pauses() {
        let travel = marquee_travel(100., 300.);
        assert_eq!(travel, 400.);
        // starts at the right edge of the slot
        assert_eq!(marquee_left(0., travel), 0.);
        // gone (its end past the left edge) when the run is over, and stays gone for the pause
        let moving = (travel / MARQUEE_PX_S) / marquee_period(travel);
        assert_eq!(marquee_left(moving, travel), -travel);
        assert_eq!(marquee_left(1., travel), -travel);
        // halfway through the run: half the way
        assert!((marquee_left(moving / 2., travel) + travel / 2.).abs() < 1e-3);
        // period = (slot + text) / speed + pause
        assert_eq!(marquee_period(travel), 400. / 40. + 1.);
    }
}
