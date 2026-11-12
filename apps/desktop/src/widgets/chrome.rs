//! The window chrome both views share, 1:1 with `cli.html`: the top bar (`.topbar`, `.tchip`), the panel
//! (`.panel`, header + body), the tab strip (`.bottomtabs`, `.tab`) and the key hint footer (`.footer`).
//! Colours are the platinum tokens; geometry and type are the design's.

use gpui::{
    AnyElement, App, ClickEvent, Div, Hsla, InteractiveElement as _, IntoElement,
    ParentElement as _, StatefulInteractiveElement as _, Styled as _, Window, div, prelude::*, px,
    svg,
};
use synthesis_cli_ui_render::theme as t;

use crate::{brand, theme::hsla};

pub fn text() -> Hsla {
    hsla(t::TEXT)
}
pub fn strong() -> Hsla {
    hsla(t::TEXT_STRONG)
}
/// Dim text of the TUI view: `MUTED` lifted a little (#8E8F93) so it reaches 4.5:1 on the elevated ground of
/// selected rows too (`MUTED` itself is 4.06 there); on BG and SURFACE it is brighter than the palette's.
pub const DIM: t::Rgb = [142, 143, 147];
pub fn dim() -> Hsla {
    hsla(DIM)
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
            .rounded(px(4.))
            .hover(|d| d.bg(with_alpha(gpui::white(), 0.05)))
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
    // a body cut at the panel's bottom (by the column or its max height) fades out over the last rows instead of
    // ending in half a line
    let clipped =
        scroll.is_some_and(|s| f32::from(s.max_offset().y) + f32::from(s.offset().y) > 1.);
    let (from, to) = (
        crate::theme::fill(t::SURFACE),
        Hsla {
            a: 0.,
            ..crate::theme::fill(t::SURFACE)
        },
    );
    div()
        .debug_selector(|| h.id.to_string())
        .relative()
        .flex()
        .flex_col()
        .min_w(px(0.))
        .overflow_hidden()
        .bg(crate::theme::fill(t::SURFACE))
        .border_1()
        .border_color(hsla(t::BORDER))
        .rounded(px(8.))
        .child(head)
        .when(!folded, |d| {
            d.child(b.child(body))
                // the bottom padding sits outside the scroll area, or the next row shows through it
                .child(div().h(px(h.pad.body_bottom)).flex_none())
                .when(clipped, |d| {
                    d.child(
                        div()
                            .absolute()
                            .left_0()
                            .right_0()
                            .bottom(px(h.pad.body_bottom))
                            .h(px(17.))
                            .flex()
                            .flex_col()
                            .child(div().flex_1().bg(gpui::linear_gradient(
                                180.,
                                gpui::linear_color_stop(to, 0.),
                                gpui::linear_color_stop(from, 1.),
                            )))
                            // the cut edge itself is fully covered: no sliver of the next row shows
                            .child(div().h(px(3.)).flex_none().bg(from)),
                    )
                })
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

/// Width a chip takes in the monospace face (7.5 px a character at 12.5 px): 14 px padding each side, the
/// 1 px rule, and 6 px between dot, label and value.
pub fn chip_w(c: &Chip) -> f32 {
    let parts = [
        c.dot.then_some(7.),
        (!c.label.is_empty()).then(|| c.label.chars().count() as f32 * 7.5),
        Some(c.value.chars().count() as f32 * 7.5),
    ];
    let parts: Vec<f32> = parts.into_iter().flatten().collect();
    29. + parts.iter().sum::<f32>() + 6. * (parts.len() - 1) as f32
}

/// The chips that fit `avail` px whole; when some do not, the leading ones and a closing `...` chip, so no chip is
/// ever cut mid-glyph. (`cli.html` clips them; that only shows below the widths the design was measured at.)
pub fn fit_chips(chips: Vec<Chip>, avail: f32) -> Vec<Chip> {
    if chips.iter().map(chip_w).sum::<f32>() <= avail {
        return chips;
    }
    let ellipsis = Chip::new("", "\u{2026}", dim());
    let room = avail - chip_w(&ellipsis);
    let mut used = 0.;
    let mut out: Vec<Chip> = chips
        .into_iter()
        .take_while(|c| {
            used += chip_w(c);
            used <= room
        })
        .collect();
    out.push(ellipsis);
    out
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
                .when(!on, |d| d.hover(|d| d.bg(with_alpha(gpui::white(), 0.05))))
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
    blink(hsla(t::ERROR), motion, ms)
}

/// `c` blinking: full for 530 ms, dim for 530 ms; steady without `motion`.
pub fn blink(c: Hsla, motion: bool, ms: i64) -> Hsla {
    let on = !motion || (ms.max(0) / 530) % 2 == 0;
    Hsla {
        a: if on { 1. } else { 0.25 },
        ..c
    }
}

pub fn black_a(a: f32) -> Hsla {
    Hsla { a, ..gpui::black() }
}

pub fn with_alpha(c: Hsla, a: f32) -> Hsla {
    Hsla { a, ..c }
}

/// Frames per second of every looping animation (spinner, blink, scrolling prompt). Their motion needs
/// far fewer than a display's 60 to 120: a frame redraws the whole window.
pub const ANIM_FPS: f32 = 30.;

/// Asks for one more frame of the view being rendered in `1 / ANIM_FPS` s instead of at the next vsync.
/// `slot` is the view's own field for the pending timer (one at a time).
pub fn next_frame<T: 'static>(
    slot: fn(&mut T) -> &mut Option<gpui::Task<()>>,
    this: &mut T,
    cx: &mut gpui::Context<T>,
) {
    if slot(this).is_some() {
        return;
    }
    *slot(this) = Some(cx.spawn(async move |this, cx| {
        cx.background_executor()
            .timer(std::time::Duration::from_secs_f32(1. / ANIM_FPS))
            .await;
        this.update(cx, |t, cx| {
            *slot(t) = None;
            cx.notify();
        })
        .ok();
    }));
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

/// The two header rows (`.topbar` and `#chead`) and the least width each of their items keeps, px. Every width
/// they reserve is read from `HEADER_MIN`; the chips and the status/path text add their own (7.5px a character).
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum HeaderItem {
    TopPad,
    TopBrand,
    TopGap,
    TopSlack,
    DrawerButton,
    ChatIcon,
    ChatName,
    ChatRule,
    ChatCtxBar,
    ChatPad,
    /// The prompt marquee's slot (about 20 characters).
    PromptSlot,
}

pub const HEADER_MIN: [(HeaderItem, f32); 11] = [
    (HeaderItem::TopPad, 32.),
    (HeaderItem::TopBrand, 160.),
    (HeaderItem::TopGap, 28.),
    (HeaderItem::TopSlack, 8.),
    (HeaderItem::DrawerButton, 32.),
    (HeaderItem::ChatIcon, 14.),
    (HeaderItem::ChatName, 67.5),
    (HeaderItem::ChatRule, 7.5),
    (HeaderItem::ChatCtxBar, 54.),
    (HeaderItem::ChatPad, 84.),
    (HeaderItem::PromptSlot, 150.),
];

pub fn header_min(item: HeaderItem) -> f32 {
    HEADER_MIN
        .iter()
        .find(|(i, _)| *i == item)
        .map_or(0., |(_, w)| *w)
}

/// What `.topbar` holds besides the chips: padding, brand mark and name, the gap after them, the slack and
/// the drawer buttons (`buttons` of them shown).
pub fn top_fixed(buttons: usize) -> f32 {
    use HeaderItem::*;
    [TopPad, TopBrand, TopGap, TopSlack]
        .into_iter()
        .map(header_min)
        .sum::<f32>()
        + header_min(DrawerButton) * buttons as f32
}

/// The header tail that still leaves the prompt slot its minimum: `path | stats`, then `path`, then nothing.
/// The lower-priority stats go first as the row narrows, then the path.
pub fn header_tail(path: &str, stats: &str, status: &str, width: f32) -> String {
    let slot = |tail: &str| width - header_fixed(status, tail);
    [format!("{path} | {stats}"), path.to_string()]
        .into_iter()
        .find(|t| slot(t) >= header_min(HeaderItem::PromptSlot))
        .unwrap_or_default()
}

/// What the header row holds besides the prompt slot (icon, "Synthesis", status, rule, context bar, tail, gaps).
fn header_fixed(status: &str, tail: &str) -> f32 {
    use HeaderItem::*;
    [ChatIcon, ChatName, ChatRule, ChatCtxBar, ChatPad]
        .into_iter()
        .map(header_min)
        .sum::<f32>()
        + 7.5 * (status.chars().count() + tail.chars().count()) as f32
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
    let slot_w = (width - header_fixed(status, tail)).max(0.);
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
                        .repeat()
                        .with_max_fps(ANIM_FPS),
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
mod contrast_tests {
    use super::*;
    use synthesis_cli_ui_render::theme::measure::contrast;

    /// WCAG AA for body text (4.5:1): the text colours the view draws, on every ground they sit on.
    #[test]
    fn body_text_reaches_4_5_to_1_on_every_ground() {
        let grounds = [
            ("BG", t::BG),
            ("SURFACE", t::SURFACE),
            ("ELEVATED", t::ELEVATED),
        ];
        let texts = [
            ("dim", DIM),
            ("TEXT", t::TEXT),
            ("TEXT_STRONG", t::TEXT_STRONG),
            ("ERROR (attention)", t::ERROR),
            ("WARN", t::WARN),
            ("ACCENT", t::ACCENT),
        ];
        for (tn, tc) in texts {
            for (gn, gc) in grounds {
                let r = contrast(tc, gc);
                assert!(r >= 4.5, "{tn} on {gn}: {r:.2}");
            }
        }
    }
}

#[cfg(test)]
mod fit_chips_tests {
    use super::*;

    fn chips() -> Vec<Chip> {
        vec![
            Chip::new("", "1 live", dim()).dotted(),
            Chip::new("session:", "Shopping", dim()),
            Chip::new("model:", "claude-sonnet", dim()),
        ]
    }

    #[test]
    fn whole_chips_stay_or_go_and_a_dropped_tail_ends_in_an_ellipsis() {
        let all: f32 = chips().iter().map(chip_w).sum();
        assert_eq!(
            fit_chips(chips(), all).len(),
            3,
            "everything fits: untouched"
        );
        for avail in [0., 50., 100., 200., all - 1.] {
            let out = fit_chips(chips(), avail);
            let used: f32 = out.iter().map(chip_w).sum();
            assert_eq!(out.last().unwrap().value, "\u{2026}", "{avail}");
            assert!(
                out.len() <= 3 && (used <= avail || out.len() == 1),
                "{avail}: {used}"
            );
        }
        // a chip is kept whole or dropped, in order
        let out = fit_chips(chips(), chip_w(&chips()[0]) + chip_w(&chips()[1]) + 40.);
        assert_eq!(out[1].label, "session:");
    }
}

#[cfg(test)]
mod header_min_tests {
    use super::*;
    use crate::tui_view::layout::{GAP, PAD, cols};

    /// Every item of the table, at the three window widths: each fits its row, and the rows' fixed parts
    /// plus the least the chips and the prompt slot keep fit the row.
    #[test]
    fn every_header_item_fits_its_row_at_900_1100_1400() {
        for w in [900., 1100., 1400.] {
            let c = cols(w);
            let side = |on: bool, x: f32| if on { x + GAP } else { 0. };
            let centre = w - 2. * PAD - side(c.left_inline, c.left) - side(c.right_inline, c.right);
            let buttons = usize::from(!c.left_inline) + usize::from(!c.right_inline);
            for (item, min) in HEADER_MIN {
                let row = match item {
                    HeaderItem::TopPad
                    | HeaderItem::TopBrand
                    | HeaderItem::TopGap
                    | HeaderItem::TopSlack
                    | HeaderItem::DrawerButton => w,
                    _ => centre,
                };
                assert!(min > 0. && min < row, "{item:?} at {w}");
            }
            // top bar: the right chips (health, clock) and the closing `...` chip stay
            let right = chip_w(&Chip::new("", "all systems ok", dim()).dotted())
                + chip_w(&Chip::new("", "Tue Nov 10  06:47:07", dim()));
            let dots = chip_w(&Chip::new("", "\u{2026}", dim()));
            assert!(top_fixed(buttons) + right + dots <= w, "top bar at {w}");
            // chat head: "ready", no tail, the prompt slot
            assert!(
                header_fixed("ready", "") + header_min(HeaderItem::PromptSlot) <= centre,
                "chat head at {w}"
            );
        }
    }
}

#[cfg(test)]
mod header_tail_tests {
    use super::*;

    #[test]
    fn stats_then_path_drop_as_the_row_narrows() {
        let (p, st) = ("~/Vibe/synthesis", "12 files | 3.2k LOC");
        assert_eq!(header_tail(p, st, "ready", 1100.), format!("{p} | {st}"));
        assert_eq!(header_tail(p, st, "ready", 620.), p);
        assert_eq!(header_tail(p, st, "ready", 400.), "");
    }
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

#[cfg(test)]
mod next_frame_tests {
    use super::*;
    use gpui::{Context, Render, TestAppContext};
    use std::time::Duration;

    struct Spin {
        renders: u32,
        tick: Option<gpui::Task<()>>,
        vsync: bool,
    }

    impl Render for Spin {
        fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            self.renders += 1;
            if self.vsync {
                window.request_animation_frame();
            } else {
                next_frame(|s| &mut s.tick, self, cx);
            }
            div()
        }
    }

    /// Renders in one second of 120 Hz frames: a vsync loop draws every one, `next_frame` about `ANIM_FPS`.
    fn renders_in_a_second(cx: &mut TestAppContext, vsync: bool) -> u32 {
        let (v, cx) = cx.add_window_view(|_, _| Spin {
            renders: 0,
            tick: None,
            vsync,
        });
        for _ in 0..120 {
            cx.executor().advance_clock(Duration::from_micros(8333));
            cx.run_until_parked();
            cx.update(|window, cx| {
                window.simulate_next_frame(cx);
            });
        }
        v.read_with(cx, |s, _| s.renders)
    }

    #[gpui::test]
    fn a_looping_animation_draws_at_anim_fps_not_at_vsync(cx: &mut TestAppContext) {
        let throttled = renders_in_a_second(cx, false);
        let vsync = renders_in_a_second(cx, true);
        println!("renders in 1 s at 120 Hz: vsync loop {vsync}, next_frame {throttled}");
        assert!(vsync >= 100, "{vsync}");
        assert!(
            (25..=35).contains(&throttled),
            "{throttled} renders in a second"
        );
    }
}
