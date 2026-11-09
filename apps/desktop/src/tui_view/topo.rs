//! The Topology graph: node cards around the core and the wires between them. `geometry` is pure
//! (rects and wire paths from the body size, rules of `cli.html` `layoutTopo`/`drawLinks` modes A, B
//! and T); `paint` draws it inside a GPUI canvas. Mode L of the design (tall and narrow) falls back to T.

use std::collections::HashMap;

use std::collections::BTreeMap;

use gpui::{
    AnyElement, App, Bounds, IntoElement, ParentElement as _, PathBuilder, Pixels, Point,
    Styled as _, Window, canvas, div, fill, point, prelude::*, px, size,
};
use synthesis_tui_render::theme as t;
use synthesis_view::pulse::{Act, Node, toward_core};

use super::MONO;
use super::sample::{Core, NodeState, Pos, Snapshot, TopoNode};
use crate::widgets::chrome::{icon, tracked};
use crate::{brand, theme::hsla};

#[derive(Clone, Copy, Debug, PartialEq, Default)]
pub struct R {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

impl R {
    pub fn cx(&self) -> f32 {
        self.x + self.w / 2.
    }
    pub fn cy(&self) -> f32 {
        self.y + self.h / 2.
    }
    pub fn r(&self) -> f32 {
        self.x + self.w
    }
    pub fn b(&self) -> f32 {
        self.y + self.h
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// Normal 3x3: two lines per card.
    A,
    /// Compact 3x3: one line per card.
    B,
    /// Trunk list: core bar on top, one row per node.
    T,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Seg {
    M(f32, f32),
    L(f32, f32),
    /// Quadratic curve: control point, end point.
    Q(f32, f32, f32, f32),
}

#[derive(Clone, Debug)]
pub struct Geo {
    pub mode: Mode,
    pub core: R,
    pub nodes: Vec<(Pos, R)>,
    pub wires: Vec<Vec<Seg>>,
    /// Wire ends (drawn as a dot).
    pub dots: Vec<(f32, f32)>,
    /// T mode below 232px: rows without their detail line.
    pub tmin: bool,
}

impl Geo {
    pub fn rect(&self, p: Pos) -> R {
        self.nodes
            .iter()
            .find(|(q, _)| *q == p)
            .map(|x| x.1)
            .unwrap()
    }
}

/// Width of the core's column relative to a side column.
const CORE_COL: f32 = 1.25;

const GRID: [[Pos; 3]; 3] = [
    [Pos::User, Pos::Hooks, Pos::Guards],
    [Pos::Sub, Pos::Hooks, Pos::Skills], // the middle cell is the core
    [Pos::Mcp, Pos::Tools, Pos::Mem],
];

/// Row order of the T list (`order:` rules).
const T_ORDER: [Pos; 8] = [
    Pos::User,
    Pos::Guards,
    Pos::Sub,
    Pos::Skills,
    Pos::Hooks,
    Pos::Tools,
    Pos::Mcp,
    Pos::Mem,
];

pub fn mode_for(aw: f32, ah: f32) -> Mode {
    if ah >= 314. && aw >= 380. {
        Mode::A
    } else if ah >= 200. && aw >= 330. {
        Mode::B
    } else {
        Mode::T
    }
}

/// Elbow leaving (sx,sy) vertically, jogging at the middle row, entering (ex,ey) vertically.
fn vertical(sx: f32, sy: f32, ex: f32, ey: f32) -> Vec<Seg> {
    if (sx - ex).abs() < 1. {
        return vec![Seg::M(sx, sy), Seg::L(sx, ey)];
    }
    let my = (sy + ey) / 2.;
    let (dx, dy) = ((ex - sx).signum(), (ey - sy).signum());
    let r = 10f32.min((ex - sx).abs() / 2.).min((ey - sy).abs() / 2.5);
    vec![
        Seg::M(sx, sy),
        Seg::L(sx, my - dy * r),
        Seg::Q(sx, my, sx + dx * r, my),
        Seg::L(ex - dx * r, my),
        Seg::Q(ex, my, ex, my + dy * r),
        Seg::L(ex, ey),
    ]
}

fn horizontal(sx: f32, sy: f32, ex: f32, ey: f32) -> Vec<Seg> {
    if (sy - ey).abs() < 1. {
        return vec![Seg::M(sx, sy), Seg::L(ex, sy)];
    }
    let mx = (sx + ex) / 2.;
    let (dx, dy) = ((ex - sx).signum(), (ey - sy).signum());
    let r = 10f32.min((ex - sx).abs() / 4.).min((ey - sy).abs() / 2.);
    vec![
        Seg::M(sx, sy),
        Seg::L(mx - dx * r, sy),
        Seg::Q(mx, sy, mx, sy + dy * r),
        Seg::L(mx, ey - dy * r),
        Seg::Q(mx, ey, mx + dx * r, ey),
        Seg::L(ex, ey),
    ]
}

/// Rects and wires for a body of `aw` x `ah` px.
pub fn geometry(aw: f32, ah: f32) -> Geo {
    let mode = mode_for(aw, ah);
    if mode == Mode::T {
        return geometry_t(aw, ah);
    }
    let a = mode == Mode::A;
    let (px_, py, cg, rg) = if a {
        (2f32, 6f32, 18f32, 22f32)
    } else {
        (2f32, 4f32, 10f32, 12f32)
    };
    let w = aw - 2. * px_;
    // owner request: eight equal cards and a square core that is bigger than they are. The middle column is
    // CORE_COL times as wide as the side ones (the cards of that column stay centred in it at the common
    // width); the core's side is that width, the cards take their height from what is left.
    let col = (w - 2. * cg) / (2. + CORE_COL);
    let mid = col * CORE_COL;
    let col_x = [px_, px_ + col + cg, px_ + 2. * cg + col + mid];
    // the cards give height to the core before it is smaller than they are wide
    let (cmin, cmax) = if a { (70., 130.) } else { (34., 70.) };
    let avail = ah - 2. * py;
    let side = mid
        .min(if a { 240. } else { 200. })
        .min(avail - 2. * cmin - 2. * rg)
        .max(cmin);
    let card_h = ((avail - side - 2. * rg) / 2.).clamp(cmin, cmax);
    // spare height is kept out of the wires: the gaps grow a little, the rest centres the grid
    let rg = rg.max((avail - side - 2. * card_h) / 2.).min(rg * 2.5);
    let top = py + ((avail - side - 2. * card_h - 2. * rg) / 2.).max(0.);
    let row_y = [top, top + card_h + rg, top + card_h + rg + side + rg];
    let mut nodes = Vec::new();
    for (ri, row) in GRID.iter().enumerate() {
        for (ci, p) in row.iter().enumerate() {
            if ri == 1 && ci == 1 {
                continue;
            }
            let x = if ci == 1 {
                col_x[1] + (mid - col) / 2.
            } else {
                col_x[ci]
            };
            let y = match ri {
                1 => row_y[1] + (side - card_h) / 2.,
                _ => row_y[ri],
            };
            nodes.push((
                *p,
                R {
                    x,
                    y,
                    w: col,
                    h: card_h,
                },
            ));
        }
    }
    let core = R {
        x: col_x[1] + (mid - side) / 2.,
        y: row_y[1],
        w: side,
        h: side,
    };
    let g = Geo {
        mode,
        core,
        nodes,
        wires: vec![],
        dots: vec![],
        tmin: false,
    };
    // Wires attach at the middle of the node's edge facing the core and, on the core, at the middle of
    // a side or at +-24% of its width on the top/bottom edge: symmetric about the core's centre line.
    let off = core.w * 0.24;
    let (cl, cr, cu, cd) = (core.cx() - off, core.cx() + off, core.cx(), core.cx());
    let _ = (cu, cd);
    let (u, h, gd) = (g.rect(Pos::User), g.rect(Pos::Hooks), g.rect(Pos::Guards));
    let (sub, sk) = (g.rect(Pos::Sub), g.rect(Pos::Skills));
    let (m, tl, me) = (g.rect(Pos::Mcp), g.rect(Pos::Tools), g.rect(Pos::Mem));
    let links = [
        (
            vertical(u.cx(), u.b(), cl, core.y),
            (u.cx(), u.b()),
            (cl, core.y),
        ),
        (
            vertical(h.cx(), h.b(), core.cx(), core.y),
            (h.cx(), h.b()),
            (core.cx(), core.y),
        ),
        (
            vertical(gd.cx(), gd.b(), cr, core.y),
            (gd.cx(), gd.b()),
            (cr, core.y),
        ),
        (
            horizontal(sub.r(), sub.cy(), core.x, core.cy()),
            (sub.r(), sub.cy()),
            (core.x, core.cy()),
        ),
        (
            horizontal(sk.x, sk.cy(), core.r(), core.cy()),
            (sk.x, sk.cy()),
            (core.r(), core.cy()),
        ),
        (
            vertical(m.cx(), m.y, cl, core.b()),
            (m.cx(), m.y),
            (cl, core.b()),
        ),
        (
            vertical(tl.cx(), tl.y, core.cx(), core.b()),
            (tl.cx(), tl.y),
            (core.cx(), core.b()),
        ),
        (
            vertical(me.cx(), me.y, cr, core.b()),
            (me.cx(), me.y),
            (cr, core.b()),
        ),
    ];
    let mut wires = Vec::new();
    let mut dots = Vec::new();
    for (wire, a, b) in links {
        wires.push(wire);
        dots.push(a);
        dots.push(b);
    }
    Geo { wires, dots, ..g }
}

fn geometry_t(aw: f32, ah: f32) -> Geo {
    let tmin = ah < 232.;
    let (core_h, core_gap, row_h): (f32, f32, f32) = if tmin {
        (20., 8., 15.)
    } else {
        (24., 10., 19.)
    };
    // `max-width:100%` of the 22px-padded list clips the core's stretched width (the -20px margin) to it
    let core = R {
        x: 2.,
        y: 2.,
        w: aw - 24.,
        h: core_h,
    };
    let x = 22.;
    // the column's `gap:2px` comes on top of the core's bottom margin
    let mut y = core.b() + core_gap + 2.;
    // owner request: the rows share the height left (up to 34 px each) instead of leaving the panel empty
    let row_h = row_h.max(((ah - y - 2. - 7. * 2.) / 8.).min(34.));
    let mut nodes = Vec::new();
    for p in T_ORDER {
        nodes.push((
            p,
            R {
                x,
                y,
                w: aw - 2. - x,
                h: row_h,
            },
        ));
        y += row_h + 2.;
    }
    let tx = core.x + 10.;
    let r = 6f32.min(4f32.max(row_h / 2. - 1.));
    let mut wires = Vec::new();
    let mut dots = vec![(tx, core.b())];
    for (_, n) in &nodes {
        wires.push(vec![
            Seg::M(tx, n.cy() - r),
            Seg::Q(tx, n.cy(), tx + r, n.cy()),
            Seg::L(n.x, n.cy()),
        ]);
        dots.push((n.x, n.cy()));
    }
    let last = nodes.last().unwrap().1.cy();
    wires.push(vec![Seg::M(tx, core.b()), Seg::L(tx, last - r)]);
    Geo {
        mode: Mode::T,
        core,
        nodes,
        wires,
        dots,
        tmin,
    }
}

fn node_colour(p: Pos) -> t::Rgb {
    match p {
        Pos::User => t::ACCENT,
        Pos::Hooks => t::WARN,
        Pos::Guards => t::ERROR,
        Pos::Sub => t::BEIGE,
        Pos::Skills => t::ACCENT_LIGHT,
        Pos::Mcp => t::INFO,
        Pos::Tools => t::SILVER,
        Pos::Mem => t::CYAN,
    }
}

pub(super) use crate::widgets::chrome::with_alpha;

fn node_icon(p: Pos) -> &'static str {
    match p {
        Pos::User => "user",
        Pos::Hooks => "webhook",
        Pos::Guards => "shield",
        Pos::Sub => "users",
        Pos::Skills => "layers",
        Pos::Mcp => "link",
        Pos::Tools => "wrench",
        Pos::Mem => "database",
    }
}

fn node_of(p: Pos) -> Node {
    match p {
        Pos::User => Node::User,
        Pos::Hooks => Node::Hooks,
        Pos::Guards => Node::Guards,
        Pos::Sub => Node::Subagents,
        Pos::Skills => Node::Skills,
        Pos::Mcp => Node::Mcp,
        Pos::Tools => Node::Tools,
        Pos::Mem => Node::Memory,
    }
}

/// One blink is 1060 ms (twice the CLI's 530 ms half-period), as a smooth wave in 0..=1.
fn wave(now: i64) -> f32 {
    0.5 + 0.5 * (now.rem_euclid(1060) as f32 / 1060. * std::f32::consts::TAU).sin()
}

/// How strongly a node shows its activity, 0..=1. With motion it breathes and fades out over the hold;
/// without, it is a steady full level while active (a static border, no animation).
pub fn level(a: Act, now: i64, motion: bool) -> f32 {
    if !a.on() {
        0.
    } else if motion {
        a.strength * (0.55 + 0.45 * wave(now))
    } else {
        1.
    }
}

/// Dash pattern of the travelling pulse, in px, and its speed in px per ms.
const DASH: f32 = 7.;
const GAP_: f32 = 8.;
const SPEED: f32 = 0.07;

fn flatten(wire: &[Seg]) -> Vec<(f32, f32)> {
    let mut v: Vec<(f32, f32)> = vec![];
    for seg in wire {
        match *seg {
            Seg::M(x, y) | Seg::L(x, y) => v.push((x, y)),
            Seg::Q(cx, cy, x, y) => {
                let (x0, y0) = *v.last().unwrap_or(&(x, y));
                for i in 1..=8 {
                    let u = i as f32 / 8.;
                    let w = 1. - u;
                    v.push((
                        w * w * x0 + 2. * w * u * cx + u * u * x,
                        w * w * y0 + 2. * w * u * cy + u * u * y,
                    ));
                }
            }
        }
    }
    v
}

/// The part of `poly` between arc lengths `a` and `b`.
fn slice(poly: &[(f32, f32)], a: f32, b: f32) -> Vec<(f32, f32)> {
    let (mut out, mut at) = (vec![], 0f32);
    for w in poly.windows(2) {
        let (p, q) = (w[0], w[1]);
        let len = ((q.0 - p.0).powi(2) + (q.1 - p.1).powi(2)).sqrt();
        if len > 0. && at + len > a && at < b {
            let pt = |s: f32| {
                let u = ((s - at) / len).clamp(0., 1.);
                (p.0 + (q.0 - p.0) * u, p.1 + (q.1 - p.1) * u)
            };
            if out.is_empty() {
                out.push(pt(a));
            }
            out.push(pt(b));
        }
        at += len;
    }
    out
}

/// The dashes of a pulse running along `poly` (in the direction of the path) at time `now`.
fn dashes(poly: &[(f32, f32)], now: i64) -> Vec<Vec<(f32, f32)>> {
    let total: f32 = poly
        .windows(2)
        .map(|w| ((w[1].0 - w[0].0).powi(2) + (w[1].1 - w[0].1).powi(2)).sqrt())
        .sum();
    let period = DASH + GAP_;
    let off = (now.rem_euclid(100_000) as f32 * SPEED) % period;
    let mut v = vec![];
    let mut s = off - period;
    while s < total {
        let (a, b) = (s.max(0.), (s + DASH).min(total));
        if b > a {
            v.push(slice(poly, a, b));
        }
        s += period;
    }
    v
}

/// The wire of node `i` as a path running from the core to the node. A, B: the wires are drawn node -> core.
fn core_to_node(geo: &Geo, i: usize) -> Vec<(f32, f32)> {
    let mut p = flatten(&geo.wires[i]);
    if geo.mode == Mode::T {
        // the trunk comes down from the core bar to the branch
        p.insert(0, geo.dots[0]);
    } else {
        p.reverse();
    }
    p
}

/// The focus glow around an active card or the core, like the logo's halo.
fn glow(c: gpui::Hsla, lvl: f32) -> gpui::BoxShadow {
    gpui::BoxShadow {
        color: with_alpha(c, 0.3 * lvl),
        offset: point(px(0.), px(0.)),
        blur_radius: px(4. + 10. * lvl),
        spread_radius: px(0.),
        inset: false,
    }
}

/// What a node card holds in its box, derived from the box alone: the cut title and how many detail lines
/// show (and wrap) so the content always fits inside the border and padding. Painter and tests share it.
#[derive(Clone, Copy, Debug)]
pub struct CardFit {
    pub title_size: f32,
    pub line_size: f32,
    pub icon: f32,
    /// Characters of the (upper-case) title that fit; longer ones are cut with an ellipsis, 0 hides it.
    pub name_max: usize,
    /// Detail lines shown, and how many rows each may wrap to.
    pub lines: usize,
    pub clamp: usize,
}

/// Border plus padding on the two sides of a card's content, horizontally.
fn card_inset(mode: Mode) -> f32 {
    match mode {
        Mode::A => 2. + 20.,
        Mode::B => 2. + 14.,
        Mode::T => 2. + 12.,
    }
}

pub fn card_fit(mode: Mode, tmin: bool, w: f32, h: f32) -> CardFit {
    let a = mode == Mode::A;
    let (title_size, line_size) = if a { (12., 12.5) } else { (10.5, 10.5) };
    let icon = if a {
        16.
    } else if mode == Mode::B {
        12.
    } else {
        11.
    };
    let nlines = match mode {
        Mode::A => 2,
        Mode::B => 1,
        Mode::T => usize::from(!tmin),
    };
    let lh = line_size * if a { 1.45 } else { 1.35 };
    let room = (w - card_inset(mode)).max(0.);
    // icon and gap; a T row also reserves the 6px marker dot and its gap
    let head = icon + if mode == Mode::T { 6. + 5. + 5. } else { 6. };
    let name_max = ((room - head) / (title_size * 0.63)).floor().max(0.) as usize;
    if mode == Mode::T {
        // the detail goes after the title with an 8px gap and shows only if a few characters of it fit
        return CardFit {
            title_size,
            line_size,
            icon,
            name_max,
            lines: nlines,
            clamp: 1,
        };
    }
    // the title row, its gap, then each detail line, inside the border
    let title_h = icon.max(title_size * 1.45);
    let pb = if a { 6. } else { 2. };
    let fit = ((h - 2. - title_h - pb) / lh).floor().max(0.) as usize;
    let used = if a { 9. + 6. + 24. } else { 14. + 2. + 10. };
    let lines = nlines.min(fit);
    CardFit {
        title_size,
        line_size,
        icon,
        name_max,
        lines,
        clamp: if h - used >= nlines as f32 * 2. * lh {
            2
        } else {
            1
        },
    }
}

/// The title as shown: "USER & PROMPT" gives way to "USER" before it is cut with an ellipsis.
fn card_name(title: &str, max: usize) -> String {
    let mut name = title.to_uppercase();
    if name.chars().count() > max
        && let Some((head, _)) = name.split_once(" & ")
    {
        name = head.to_string();
    }
    if name.chars().count() > max {
        name = match max {
            0 => String::new(),
            _ => name.chars().take(max - 1).collect::<String>() + "\u{2026}",
        };
    }
    name
}

/// Width and height in px of what the card lays out (detail lines truncate, so they add no minimum width).
pub fn card_content(mode: Mode, f: &CardFit, name_chars: usize) -> (f32, f32) {
    let head = f.icon + if mode == Mode::T { 6. + 5. + 5. } else { 6. };
    let w = head + name_chars as f32 * f.title_size * 0.63;
    let a = mode == Mode::A;
    let lh = f.line_size * if a { 1.45 } else { 1.35 };
    let title_h = f
        .icon
        .max(f.title_size * if mode == Mode::T { 1.35 } else { 1.45 });
    let h = if mode == Mode::T {
        title_h
    } else {
        title_h + if a { 6. } else { 2. } + f.lines as f32 * lh + 2.
    };
    (w, h)
}

fn card(n: &TopoNode, r: R, mode: Mode, tmin: bool, lvl: f32) -> AnyElement {
    let base = hsla(node_colour(n.pos));
    let col = if n.state == NodeState::Blocked {
        hsla(t::ERROR)
    } else {
        base
    };
    let a = mode == Mode::A;
    let fit = card_fit(mode, tmin, r.w, r.h);
    let (title_size, line_size, ic) = (fit.title_size, fit.line_size, fit.icon);
    let mut d = div()
        .w(px(r.w))
        .h(px(r.h))
        .flex()
        .overflow_hidden()
        .font_family(MONO)
        .text_color(hsla(t::TEXT));
    let name = card_name(&n.title, fit.name_max);
    let title = div()
        .flex()
        .items_center()
        .gap(px(if a {
            6.
        } else if mode == Mode::B {
            4.
        } else {
            5.
        }))
        // narrow rows: a marker dot beats with the activity
        .when(lvl > 0. && mode == Mode::T, |d| {
            d.child(
                div()
                    .size(px(6.))
                    .flex_none()
                    .rounded_full()
                    .bg(with_alpha(hsla(t::PEARL), 0.35 + 0.65 * lvl)),
            )
        })
        .child(icon(node_icon(n.pos), ic, col))
        .child(tracked(&name, title_size, 0.03, col));
    // detail lines wrap to two rows when the height has room, else they are cut to one
    let clamp = fit.clamp;
    // a T row shows its detail only when a few characters of it fit after the title
    let t_room =
        r.w - card_inset(Mode::T) - (card_content(mode, &fit, name.chars().count()).0) - 8.;
    let nlines = if mode == Mode::T && t_room < 3. * line_size * 0.6 {
        0
    } else {
        fit.lines
    };
    let lines = n.lines.iter().take(nlines).map(|l| {
        div()
            // one long word (an identifier) is cut with an ellipsis rather than broken in the middle
            .when(clamp == 1 || !l.contains(' '), |d| d.truncate())
            .when(clamp > 1 && l.contains(' '), |d| d.line_clamp(clamp))
            .text_size(px(line_size))
            .line_height(px(line_size * if a { 1.45 } else { 1.35 }))
            .child(l.clone())
    });
    d = if mode == Mode::T {
        d.items_center()
            .gap(px(8.))
            .px(px(6.))
            .border_l_2()
            .border_color(col)
            .rounded_r(px(4.))
            .bg(with_alpha(col, 0.06 + 0.14 * lvl))
            .when(lvl > 0., |d| d.shadow(vec![glow(col, lvl)]))
            .child(title)
            .children(lines.take(1))
    } else {
        let mut d = d
            .flex_col()
            .justify_center()
            .px(px(if a { 10. } else { 7. }))
            .border_1()
            .border_color(with_alpha(
                col,
                if lvl > 0. {
                    0.8 + 0.2 * lvl
                } else if n.state == NodeState::Idle {
                    0.55
                } else {
                    0.8
                },
            ))
            .rounded(px(if a { 10. } else { 8. }))
            .when(lvl > 0., |d| d.shadow(vec![glow(col, lvl)]))
            .child(div().pb(px(if a { 6. } else { 2. })).child(title));
        for l in lines {
            d = d.child(l);
        }
        d
    };
    d.into_any_element()
}

/// What the core card holds in a box of `w` x `h` px, derived from the box alone and degrading by HIDING parts
/// (tag, then sub line, then title, then everything but the logo) so the content always fits the room inside the
/// border and the padding, whatever the panel size. `large` is the design's own rule for showing the sub line
/// and the tag. Painter and tests use this one function.
#[derive(Clone, Copy, Debug)]
pub struct CoreLayout {
    pub logo: f32,
    /// Title font size; `None`: the title is hidden.
    pub title: Option<f32>,
    pub track: f32,
    pub sub: bool,
    pub tag: bool,
    /// Padding on each side, inside the border.
    pub pad: f32,
    pub border: f32,
}

/// Characters of the core's title, sub line and tag.
pub type CoreChars = (usize, usize, usize);

pub fn core_layout(mode: Mode, w: f32, h: f32, large: bool, ch: CoreChars) -> CoreLayout {
    let (w, h) = (w.max(0.), h.max(0.));
    if mode == Mode::T {
        let (border, pad) = (1., 10f32.min(w * 0.1));
        let room = (w - 2. * (border + pad)).max(0.);
        let base = CoreLayout {
            logo: 14f32.min((h - 2. * border).max(0.)),
            title: Some(10.5),
            track: 0.14,
            sub: false,
            tag: false,
            pad,
            border,
        };
        let tag = CoreLayout {
            tag: w >= 300.,
            ..base
        };
        return [tag, CoreLayout { tag: false, ..base }]
            .into_iter()
            .find(|l| core_content(mode, l, ch).0 <= room)
            .unwrap_or(CoreLayout {
                title: None,
                logo: base.logo.min(room),
                ..base
            });
    }
    let side = w.min(h);
    let (border, pad) = (2., (side * 0.05).min(8.));
    let room = (side - 2. * (border + pad)).max(0.);
    let a = mode == Mode::A;
    let base = CoreLayout {
        logo: if a { 0.4 * side } else { 0.38 * side }.min(if a { 76. } else { 44. }),
        title: Some(if !a {
            if side >= 130. { 11. } else { 9.5 }
        } else if side >= 170. {
            14.
        } else if side >= 140. {
            12.
        } else {
            10.5
        }),
        track: if !a {
            0.
        } else if side >= 170. {
            0.2
        } else {
            0.12
        },
        sub: a && large && side >= 150.,
        tag: a && large && side >= 190.,
        pad,
        border,
    };
    let fits = |l: &CoreLayout| {
        let (cw, chh) = core_content(mode, l, ch);
        cw <= room && chh <= room
    };
    // a tiny side shows only the logo, scaled to the room
    let logo_only = CoreLayout {
        title: None,
        sub: false,
        tag: false,
        logo: base.logo.min(room),
        ..base
    };
    if side < 60. {
        return CoreLayout {
            logo: room,
            ..logo_only
        };
    }
    [
        base,
        CoreLayout { tag: false, ..base },
        CoreLayout {
            tag: false,
            sub: false,
            ..base
        },
    ]
    .into_iter()
    .find(fits)
    .unwrap_or(logo_only)
}

/// Width and height in px of the content `l` lays out in the core card (what the painter builds).
pub fn core_content(mode: Mode, l: &CoreLayout, (tn, sn, gn): CoreChars) -> (f32, f32) {
    let line = |fs: f32| fs * 1.3;
    let text_w = |fs: f32, track: f32, n: usize| n as f32 * fs * (0.6 + track);
    if mode == Mode::T {
        let mut w = l.logo;
        if let Some(f) = l.title {
            w += 8. + text_w(f, l.track, tn);
        }
        if l.tag {
            w += 8. + text_w(10., 0., gn);
        }
        return (w, l.logo.max(l.title.map_or(0., |f| line(f))));
    }
    let (mut w, mut h) = (l.logo, l.logo);
    if let Some(f) = l.title {
        h += line(f) + if mode == Mode::A { 4. } else { 0. };
        w = w.max(text_w(f, l.track, tn));
    }
    if l.sub {
        h += line(12.);
        w = w.max(text_w(12., 0.25, sn));
    }
    if l.tag {
        h += 6. + line(12.);
        w = w.max(text_w(12., 0., gn));
    }
    (w, h)
}

fn core_card(c: &Core, r: R, mode: Mode, large: bool, lvl: f32) -> AnyElement {
    let ink = hsla(t::TEXT_STRONG);
    let logo = |s: f32| {
        // a soft platinum halo behind the mark (two layers); it stays inside the card, which clips
        let halo = |a: f32, blur: f32| gpui::BoxShadow {
            color: with_alpha(hsla(t::ACCENT_GLOW), a),
            offset: point(px(0.), px(0.)),
            blur_radius: px(blur),
            spread_radius: px(0.),
            inset: false,
        };
        div()
            .size(px(s))
            .flex_none()
            .rounded_full()
            .shadow(vec![
                halo(0.28 + 0.5 * lvl, s * 0.35),
                halo(0.14 + 0.3 * lvl, s * 0.2),
            ])
            .child(gpui::img(brand::mark()).size(px(s)))
            .into_any_element()
    };
    let ch = (
        c.title.chars().count(),
        c.sub.chars().count(),
        c.tag.chars().count(),
    );
    let f = core_layout(mode, r.w, r.h, large, ch);
    let d = div()
        .w(px(r.w))
        .h(px(r.h))
        .flex()
        .overflow_hidden()
        .font_family(MONO)
        .items_center()
        .justify_center()
        .bg(hsla(t::BG))
        .border_color(hsla(t::ACCENT_LIGHT))
        .text_color(hsla(t::TEXT))
        .when(lvl > 0., |d| {
            d.shadow(vec![glow(hsla(t::ACCENT_GLOW), lvl)])
        });
    match mode {
        Mode::T => d
            .gap(px(8.))
            .px(px(f.pad))
            .border_1()
            .rounded(px(6.))
            // the bar's content starts at the left padding like a list row; a lone logo is centred
            .when(f.title.is_some(), |d| d.justify_start())
            .child(logo(f.logo))
            .when_some(f.title, |d, s| d.child(tracked(&c.title, s, f.track, ink)))
            .when(f.tag, |d| {
                d.child(div().ml_auto().text_size(px(10.)).child(c.tag.clone()))
            }),
        _ => {
            let mut d = d
                .flex_col()
                .p(px(f.pad))
                .border_2()
                .rounded(px(12.))
                .child(logo(f.logo));
            if let Some(s) = f.title {
                d = d.child(
                    div()
                        .when(mode == Mode::A, |d| d.mt(px(4.)))
                        .child(tracked(&c.title, s, f.track, ink)),
                );
            }
            if f.sub {
                d = d.child(tracked(&c.sub, 12., 0.25, ink));
            }
            if f.tag {
                d = d.child(div().mt(px(6.)).text_size(px(12.)).child(c.tag.clone()));
            }
            d
        }
    }
    .into_any_element()
}

struct Prepared {
    geo: Geo,
    els: Vec<(R, AnyElement)>,
}

fn pt(o: Point<Pixels>, x: f32, y: f32) -> Point<Pixels> {
    point(o.x + px(x), o.y + px(y))
}

/// The graph as a canvas filling its parent (the panel body).
pub fn graph(s: &Snapshot, acts: &BTreeMap<Node, Act>, now: i64, motion: bool) -> impl IntoElement {
    let s = s.clone();
    let (acts, acts2) = (acts.clone(), acts.clone());
    let act = |m: &BTreeMap<Node, Act>, n: Node| m.get(&n).copied().unwrap_or(Act::IDLE);
    canvas(
        move |b: Bounds<Pixels>, window: &mut Window, cx: &mut App| {
            let geo = geometry(f32::from(b.size.width), f32::from(b.size.height));
            let by: HashMap<Pos, &TopoNode> = s.topology.iter().map(|n| (n.pos, n)).collect();
            // `data-core`: the sub line and the tag show when a third of the body is 150px or more
            let large = (f32::from(b.size.height) - 56.) / 3. >= 150.;
            let lvl = |n: Node| level(act(&acts, n), now, motion);
            // the core swells with its own turns and, less, with every flow that passes through it
            let busiest = geo
                .nodes
                .iter()
                .map(|(p, _)| lvl(node_of(*p)))
                .fold(0., f32::max);
            let core_lvl = lvl(Node::Core).max(0.6 * busiest);
            let mut els = vec![(
                geo.core,
                core_card(&s.core, geo.core, geo.mode, large, core_lvl),
            )];
            for (p, r) in &geo.nodes {
                if let Some(n) = by.get(p) {
                    els.push((*r, card(n, *r, geo.mode, geo.tmin, lvl(node_of(*p)))));
                }
            }
            for (r, e) in &mut els {
                let o = pt(b.origin, r.x, r.y);
                e.prepaint_as_root(o, size(px(r.w), px(r.h)).into(), window, cx);
            }
            Prepared { geo, els }
        },
        move |b, mut p: Prepared, window, cx| {
            let o = b.origin;
            let line = with_alpha(hsla(t::SILVER), 0.5);
            for (i, wire) in p.geo.wires.iter().enumerate() {
                // wire i serves node i; the T trunk (the last wire) is lit by the branches
                let node = p.geo.nodes.get(i).map(|(pos, _)| *pos);
                let a = node.map_or(Act::IDLE, |pos| act(&acts2, node_of(pos)));
                let hot = node.filter(|_| a.on());
                let mut pb = PathBuilder::stroke(px(if hot.is_some() { 1.75 } else { 1.25 }));
                for seg in wire {
                    match *seg {
                        Seg::M(x, y) => pb.move_to(pt(o, x, y)),
                        Seg::L(x, y) => pb.line_to(pt(o, x, y)),
                        Seg::Q(cx_, cy, x, y) => pb.curve_to(pt(o, x, y), pt(o, cx_, cy)),
                    }
                }
                if let Ok(path) = pb.build() {
                    let lv = level(a, now, motion);
                    window.paint_path(
                        path,
                        match hot {
                            Some(pos) => with_alpha(hsla(node_colour(pos)), 0.45 + 0.5 * lv),
                            None => line,
                        },
                    );
                }
                // a bright dashed segment runs along the wire: core -> node for a request, back for the result
                // (the prompt goes the other way); with motion off the lit wire above is all there is
                if let (Some(pos), true) = (hot, motion) {
                    let mut poly = core_to_node(&p.geo, i);
                    if toward_core(node_of(pos), a.phase) {
                        poly.reverse();
                    }
                    let c = with_alpha(hsla(node_colour(pos)), 0.35 + 0.65 * a.strength);
                    for d in dashes(&poly, now) {
                        let mut pb = PathBuilder::stroke(px(2.5));
                        for (k, (x, y)) in d.iter().enumerate() {
                            if k == 0 {
                                pb.move_to(pt(o, *x, *y));
                            } else {
                                pb.line_to(pt(o, *x, *y));
                            }
                        }
                        if let Ok(path) = pb.build() {
                            window.paint_path(path, c);
                        }
                    }
                }
            }
            for (x, y) in &p.geo.dots {
                for (rad, c) in [
                    (6., with_alpha(hsla(t::SILVER), 0.18)),
                    (2.5, hsla(t::PEARL)),
                ] {
                    window.paint_quad(
                        fill(
                            Bounds::new(pt(o, x - rad, y - rad), size(px(2. * rad), px(2. * rad))),
                            c,
                        )
                        .corner_radii(px(rad)),
                    );
                }
            }
            for (_, e) in &mut p.els {
                e.paint(window, cx);
            }
        },
    )
    .absolute()
    .size_full()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ends(w: &[Seg]) -> ((f32, f32), (f32, f32)) {
        let st = match w[0] {
            Seg::M(x, y) => (x, y),
            _ => panic!("wire must start with M"),
        };
        let en = match *w.last().unwrap() {
            Seg::L(x, y) | Seg::Q(_, _, x, y) | Seg::M(x, y) => (x, y),
        };
        (st, en)
    }

    fn on_edge_mid(r: R, p: (f32, f32), horizontal_edge: bool) -> bool {
        if horizontal_edge {
            (p.0 - r.cx()).abs() < 0.01 && ((p.1 - r.y).abs() < 0.01 || (p.1 - r.b()).abs() < 0.01)
        } else {
            (p.1 - r.cy()).abs() < 0.01 && ((p.0 - r.x).abs() < 0.01 || (p.0 - r.r()).abs() < 0.01)
        }
    }

    #[test]
    fn level_is_steady_without_motion_and_fades_with_it() {
        use synthesis_view::pulse::Phase;
        let a = |strength| Act {
            phase: Phase::Result,
            strength,
        };
        assert_eq!(level(Act::IDLE, 0, true), 0.);
        for now in [0, 200, 530, 900] {
            assert_eq!(
                level(a(0.2), now, false),
                1.,
                "motion off: static at full level"
            );
        }
        assert!(level(a(0.2), 300, true) < level(a(1.), 300, true));
        let seen: Vec<f32> = (0..1060)
            .step_by(53)
            .map(|n| level(a(1.), n, true))
            .collect();
        assert!(
            seen.iter().any(|l| *l > 0.9) && seen.iter().any(|l| *l < 0.65),
            "it beats"
        );
    }

    #[test]
    fn dashes_travel_along_the_wire_in_the_asked_direction() {
        let poly = vec![(0., 0.), (100., 0.)];
        let first = |now| {
            dashes(&poly, now)
                .iter()
                .map(|d| d[0].0)
                .fold(f32::MAX, f32::min)
        };
        assert!(first(0) < 0.1, "a dash starts at the beginning");
        let (a, b) = (dashes(&poly, 0), dashes(&poly, 40));
        assert_ne!(a, b, "the pattern moves");
        assert!(
            a.iter()
                .flatten()
                .all(|p| (0. ..=100.).contains(&p.0) && p.1 == 0.)
        );
        // the dashes advance toward the end of the path they are given
        let mid = |d: &Vec<Vec<(f32, f32)>>| d.iter().map(|s| s[0].0).fold(0f32, f32::max);
        assert!(mid(&b) > mid(&a) - 0.001);
    }

    #[test]
    fn the_flow_path_runs_from_the_core_to_the_node_in_every_mode() {
        for (aw, ah) in [(476., 340.), (476., 260.), (236., 330.)] {
            let g = geometry(aw, ah);
            for (i, (_, n)) in g.nodes.iter().enumerate() {
                let p = core_to_node(&g, i);
                let (s, e) = (p[0], *p.last().unwrap());
                let on_core = |q: (f32, f32)| {
                    let c = g.core;
                    q.0 >= c.x - 0.1 && q.0 <= c.r() + 0.1 && q.1 >= c.y - 0.1 && q.1 <= c.b() + 0.1
                };
                assert!(on_core(s), "{:?} {aw}x{ah}: starts on the core", g.mode);
                assert!(
                    e.0 >= n.x - 0.1
                        && e.0 <= n.r() + 0.1
                        && e.1 >= n.y - 0.1
                        && e.1 <= n.b() + 0.1,
                    "{:?} {aw}x{ah}: ends on the node",
                    g.mode
                );
            }
        }
    }

    #[test]
    fn the_core_is_square_bigger_than_the_cards_and_its_content_fits_inside() {
        for aw in (256..=420).step_by(4) {
            for ah in (200..=900).step_by(20) {
                let g = geometry(aw as f32, ah as f32);
                if g.mode == Mode::T {
                    continue;
                }
                let (aw, ah) = (aw as f32, ah as f32);
                let u = g.rect(Pos::User);
                assert!((g.core.w - g.core.h).abs() < 0.01, "square");
                // below 240px of height the compact grid is height-bound: the core is then about a card's width
                let bigger = if ah >= 240. { u.w } else { 0.8 * u.w };
                assert!(
                    g.core.w > bigger,
                    "{aw}x{ah}: core {} vs card {}",
                    g.core.w,
                    u.w
                );
                for (p, r) in &g.nodes {
                    assert!(
                        (r.w - u.w).abs() < 0.01 && (r.h - u.h).abs() < 0.01,
                        "{p:?} equal"
                    );
                    assert!(r.y >= -0.01 && r.b() <= ah + 0.01, "{p:?} inside {aw}x{ah}");
                }
            }
        }
    }

    /// Containment by construction: at every panel size the content of the core and of each card, from the same
    /// pure functions the painter uses, fits inside its border box less the declared border and padding.
    #[test]
    fn core_and_cards_contain_their_content_at_every_panel_size() {
        let titles = [
            "User & prompt",
            "Subagents",
            "Memory",
            "A very long node title that never fits",
        ];
        let ch = (9, 4, 21);
        for aw in (120..=1200).step_by(8) {
            for ah in (80..=1600).step_by(8) {
                let (aw, ah) = (aw as f32, ah as f32);
                let g = geometry(aw, ah);
                for r in std::iter::once(&g.core).chain(g.nodes.iter().map(|(_, r)| r)) {
                    assert!(r.w >= 0. && r.h >= 0., "{aw}x{ah}: negative box {r:?}");
                }
                for large in [false, true] {
                    let l = core_layout(g.mode, g.core.w, g.core.h, large, ch);
                    let (w, h) = core_content(g.mode, &l, ch);
                    let rw = g.core.w - 2. * (l.border + l.pad);
                    let rh =
                        g.core.h - 2. * l.border - if g.mode == Mode::T { 0. } else { 2. * l.pad };
                    assert!(l.logo >= 0. && l.pad >= 0., "{aw}x{ah}: {l:?}");
                    assert!(
                        w <= rw + 0.01,
                        "{aw}x{ah} {:?} {large}: {w} wide in {rw}: {l:?}",
                        g.mode
                    );
                    assert!(
                        h <= rh + 0.01,
                        "{aw}x{ah} {:?} {large}: {h} high in {rh}: {l:?}",
                        g.mode
                    );
                    // parts go in the order tag, sub line, title, never the other way round
                    assert!(
                        !l.tag || l.sub || g.mode == Mode::T,
                        "{aw}x{ah}: tag without sub"
                    );
                    assert!(
                        l.title.is_some() || (!l.sub && !l.tag),
                        "{aw}x{ah}: parts without a title"
                    );
                }
                let tmin = g.tmin;
                for (_, r) in &g.nodes {
                    let f = card_fit(g.mode, tmin, r.w, r.h);
                    for t in titles {
                        let name = card_name(t, f.name_max);
                        let (w, h) = card_content(g.mode, &f, name.chars().count());
                        assert!(
                            w <= r.w - card_inset(g.mode) + 0.01
                                || f.name_max == 0 && name.is_empty() && f.icon <= r.w,
                            "{aw}x{ah} {:?} {t:?}: {w} wide in a {} card",
                            g.mode,
                            r.w
                        );
                        assert!(
                            h <= r.h + 0.01,
                            "{aw}x{ah} {:?}: {h} high in {}",
                            g.mode,
                            r.h
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn a_tiny_core_shows_only_a_centred_logo_and_a_huge_one_caps_the_content() {
        let ch = (9, 4, 21);
        let tiny = core_layout(Mode::B, 40., 40., true, ch);
        assert!(
            tiny.title.is_none() && !tiny.sub && !tiny.tag && tiny.logo > 0. && tiny.logo <= 40.
        );
        let degenerate = core_layout(Mode::A, 0., 0., true, ch);
        assert!(degenerate.logo == 0. && degenerate.title.is_none());
        let huge = core_layout(Mode::A, 2000., 2000., true, ch);
        assert!(huge.logo <= 76. && huge.title.is_some() && huge.tag);
        // 100 px: the title still fits, the sub line and tag do not
        let mid = core_layout(Mode::A, 100., 100., true, ch);
        assert!(mid.title.is_some() && !mid.sub && !mid.tag, "{mid:?}");
    }

    #[test]
    fn modes_follow_the_body_size() {
        assert_eq!(mode_for(480., 330.), Mode::A);
        assert_eq!(mode_for(480., 313.), Mode::B);
        assert_eq!(mode_for(329., 313.), Mode::T);
        assert_eq!(mode_for(480., 199.), Mode::T);
    }

    #[test]
    fn wires_attach_at_edge_midpoints_and_the_core_is_symmetric() {
        for (aw, ah) in [(476., 340.), (476., 260.), (236., 330.)] {
            let g = geometry(aw, ah);
            if g.mode == Mode::T {
                continue;
            }
            assert_eq!(g.wires.len(), 8);
            let mid = aw / 2.;
            assert!((g.core.cx() - mid).abs() < 0.01, "core centred");
            for (p, wire) in [
                Pos::User,
                Pos::Hooks,
                Pos::Guards,
                Pos::Sub,
                Pos::Skills,
                Pos::Mcp,
                Pos::Tools,
                Pos::Mem,
            ]
            .iter()
            .zip(&g.wires)
            {
                let (a, b) = ends(wire);
                let n = g.rect(*p);
                let horiz = !matches!(p, Pos::Sub | Pos::Skills);
                assert!(
                    on_edge_mid(n, a, horiz),
                    "{p:?} wire starts at its edge midpoint"
                );
                assert!(
                    (b.0 - g.core.x).abs() < 0.01
                        || (b.0 - g.core.r()).abs() < 0.01
                        || (b.1 - g.core.y).abs() < 0.01
                        || (b.1 - g.core.b()).abs() < 0.01,
                    "{p:?} wire ends on the core edge"
                );
            }
            // mirror symmetry about the vertical centre line
            for (l, r) in [
                (Pos::User, Pos::Guards),
                (Pos::Sub, Pos::Skills),
                (Pos::Mcp, Pos::Mem),
            ] {
                let (a, b) = (g.rect(l), g.rect(r));
                assert!(
                    (a.cx() - mid + b.cx() - mid).abs() < 0.01,
                    "{l:?}/{r:?} symmetric"
                );
            }
            let off = |i: usize| ends(&g.wires[i]).1.0 - g.core.cx();
            assert!((off(0) + off(2)).abs() < 0.01 && (off(5) + off(7)).abs() < 0.01);
            assert!(off(1).abs() < 0.01 && off(6).abs() < 0.01);
        }
    }

    #[test]
    fn cards_do_not_overlap_and_stay_inside() {
        for (aw, ah) in [(476., 400.), (476., 260.), (236., 150.), (356., 230.)] {
            let g = geometry(aw, ah);
            let mut all: Vec<R> = g.nodes.iter().map(|n| n.1).collect();
            all.push(g.core);
            for (i, a) in all.iter().enumerate() {
                assert!(
                    a.x >= -0.01 && a.r() <= aw + 0.01,
                    "{:?} {aw}x{ah} inside x",
                    g.mode
                );
                for b in &all[i + 1..] {
                    let hit = a.x < b.r() - 0.5
                        && b.x < a.r() - 0.5
                        && a.y < b.b() - 0.5
                        && b.y < a.b() - 0.5;
                    assert!(!hit, "overlap in {:?} {aw}x{ah}", g.mode);
                }
            }
        }
    }

    #[test]
    fn t_mode_has_a_trunk_and_one_branch_per_node() {
        let g = geometry(236., 150.);
        assert_eq!(g.mode, Mode::T);
        assert!(g.tmin);
        assert_eq!(g.wires.len(), 9);
        assert_eq!(g.nodes.len(), 8);
    }
}
