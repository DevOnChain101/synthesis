//! The Topology graph: node cards around the core and the wires between them. `geometry` is pure
//! (rects and wire paths from the body size, rules of `cli.html` `layoutTopo`/`drawLinks` modes A, B
//! and T); `paint` draws it inside a GPUI canvas. Mode L of the design (tall and narrow) falls back to T.

use std::collections::HashMap;

use gpui::{
    AnyElement, App, Bounds, IntoElement, ParentElement as _, PathBuilder, Pixels, Point,
    Styled as _, Window, canvas, div, fill, point, prelude::*, px, size,
};
use synthesis_tui_render::theme as t;

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
    let (px_, py, cg, rg, row_min) = if a {
        (2f32, 6f32, 18f32, 22f32, 86f32)
    } else {
        (2f32, 4f32, 10f32, 12f32, 46f32)
    };
    let w = aw - 2. * px_;
    // owner request: three equal columns and cards that fill their cell (all the same size), the core filling
    // the middle cell; so the grid follows the panel's width and height instead of leaving it empty
    let col = (w - 2. * cg) / 3.;
    let row_h = row_min.max((ah - 2. * py - 2. * rg) / 3.);
    let col_x = [px_, px_ + col + cg, px_ + 2. * (col + cg)];
    let row_y = |i: usize| py + i as f32 * (row_h + rg);
    let card_h = if a {
        row_h.clamp(86., 150.)
    } else {
        row_h.clamp(44.45, 70.)
    };
    let mut nodes = Vec::new();
    for (ri, row) in GRID.iter().enumerate() {
        for (ci, p) in row.iter().enumerate() {
            if ri == 1 && ci == 1 {
                continue;
            }
            nodes.push((
                *p,
                R {
                    x: col_x[ci],
                    y: row_y(ri) + (row_h - card_h) / 2.,
                    w: col,
                    h: card_h,
                },
            ));
        }
    }
    let (cw, ch) = if a {
        (col.min(240.), row_h.min(240.))
    } else {
        (col.min(200.), row_h)
    };
    let core = R {
        x: col_x[1] + (col - cw) / 2.,
        y: row_y(1) + (row_h - ch) / 2.,
        w: cw,
        h: ch,
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

fn card(n: &TopoNode, r: R, mode: Mode, tmin: bool) -> AnyElement {
    let base = hsla(node_colour(n.pos));
    let col = if n.state == NodeState::Blocked {
        hsla(t::ERROR)
    } else {
        base
    };
    let a = mode == Mode::A;
    let title_size = if a { 12. } else { 10.5 };
    let line_size = if a { 12.5 } else { 10.5 };
    let nlines = match mode {
        Mode::A => 2,
        Mode::B => 1,
        Mode::T => usize::from(!tmin),
    };
    let mut d = div()
        .w(px(r.w))
        .h(px(r.h))
        .flex()
        .overflow_hidden()
        .font_family(MONO)
        .text_color(hsla(t::TEXT));
    let ic = if a {
        16.
    } else if mode == Mode::B {
        12.
    } else {
        11.
    };
    // a grid card's title is cut with an ellipsis where the card ends (T rows have room)
    let mut name = n.title.to_uppercase();
    if mode != Mode::T {
        let max = ((r.w - 14. - ic - 6.) / (title_size * 0.63))
            .floor()
            .max(2.) as usize;
        if name.chars().count() > max {
            name = name.chars().take(max - 1).collect::<String>() + "\u{2026}";
        }
    }
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
        .child(icon(node_icon(n.pos), ic, col))
        .child(tracked(&name, title_size, 0.03, col));
    let lines = n.lines.iter().take(nlines).map(|l| {
        div()
            .truncate()
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
            .bg(with_alpha(col, 0.06))
            .child(title)
            .children(lines.take(1))
    } else {
        let mut d = d
            .flex_col()
            .when(!a, |d| d.justify_center())
            .when(a, |d| d.pt(px(9.)))
            .px(px(if a { 10. } else { 7. }))
            .border_1()
            .border_color(with_alpha(
                col,
                if n.state == NodeState::Idle {
                    0.55
                } else {
                    0.8
                },
            ))
            .rounded(px(if a { 10. } else { 8. }))
            .child(div().pb(px(if a { 6. } else { 2. })).child(title));
        for l in lines {
            d = d.child(l);
        }
        d
    };
    d.into_any_element()
}

fn core_card(c: &Core, r: R, mode: Mode, large: bool) -> AnyElement {
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
            .shadow(vec![halo(0.28, s * 0.35), halo(0.14, s * 0.2)])
            .child(gpui::img(brand::mark()).size(px(s)))
            .into_any_element()
    };
    let d = div()
        .w(px(r.w))
        .h(px(r.h))
        .flex()
        .overflow_hidden()
        .font_family(MONO)
        .items_center()
        .bg(hsla(t::BG))
        .border_color(hsla(t::ACCENT_LIGHT))
        .text_color(hsla(t::TEXT));
    match mode {
        Mode::T => d
            .gap(px(8.))
            .px(px(10.))
            .border_1()
            .rounded(px(6.))
            .child(logo(14.))
            .child(tracked(&c.title, 10.5, 0.14, ink))
            .child(div().ml_auto().text_size(px(10.)).child(c.tag.clone())),
        Mode::B => d
            .flex_col()
            .justify_center()
            .border_2()
            .rounded(px(12.))
            .child(logo(22.))
            .child(
                div()
                    .text_size(px(9.5))
                    .text_color(ink)
                    .child(c.title.clone()),
            ),
        Mode::A => {
            // the logo shrinks with the card (`min(76px, 100% - 44px)`)
            let l = 76f32.min(r.h - if large { 44. } else { 30. }).max(24.);
            let mut d = d
                .flex_col()
                .justify_center()
                .border_2()
                .rounded(px(12.))
                .child(logo(l))
                .child(div().mt(px(4.)).child(tracked(&c.title, 14., 0.2, ink)));
            if large {
                d = d
                    .child(tracked(&c.sub, 12., 0.25, ink))
                    .child(div().mt(px(6.)).text_size(px(12.)).child(c.tag.clone()));
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
pub fn graph(s: &Snapshot) -> impl IntoElement {
    let s = s.clone();
    canvas(
        move |b: Bounds<Pixels>, window: &mut Window, cx: &mut App| {
            let geo = geometry(f32::from(b.size.width), f32::from(b.size.height));
            let by: HashMap<Pos, &TopoNode> = s.topology.iter().map(|n| (n.pos, n)).collect();
            // `data-core`: the sub line and the tag show when a third of the body is 150px or more
            let large = (f32::from(b.size.height) - 56.) / 3. >= 150.;
            let mut els = vec![(geo.core, core_card(&s.core, geo.core, geo.mode, large))];
            for (p, r) in &geo.nodes {
                if let Some(n) = by.get(p) {
                    els.push((*r, card(n, *r, geo.mode, geo.tmin)));
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
            for wire in &p.geo.wires {
                let mut pb = PathBuilder::stroke(px(1.25));
                for seg in wire {
                    match *seg {
                        Seg::M(x, y) => pb.move_to(pt(o, x, y)),
                        Seg::L(x, y) => pb.line_to(pt(o, x, y)),
                        Seg::Q(cx_, cy, x, y) => pb.curve_to(pt(o, x, y), pt(o, cx_, cy)),
                    }
                }
                if let Ok(path) = pb.build() {
                    window.paint_path(path, line);
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
