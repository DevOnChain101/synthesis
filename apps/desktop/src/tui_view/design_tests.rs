//! Acceptance tests of the TUI view against `cli.html`: the panel rectangles the design gives in a real
//! browser (`tests/design/cli-rects.json`, dumped with headless Chrome by `getBoundingClientRect` at the five
//! viewport sizes) against the bounds GPUI lays out for the sample data (which is the design's own sample).
//! Run `cargo test -p synthesis-desktop --lib design_rects -- --nocapture` for the comparison table.
//!
//! OWNER-APPROVED DEVIATION from `cli.html` (2026-11-07, docs/specs/desktop.md "Column rule"): the design's column
//! proportions (250|1fr|500 ...) leave the chat 43% of a 1400px window; the app keeps the chat at 55% or more at every
//! width (`layout::cols`). The Chrome rects stay the reference for everything else: the left column's x/y/h and the
//! y/h of the centre column and header/footer to 1px; the x/width of the centre and right columns, and what the
//! narrower right column changes (wrapping in the cards), are checked by the rule itself (`chat_share`, `columns_*`).

use gpui::{Bounds, Pixels, TestAppContext, VisualTestContext, px, size};
use serde_json::Value;

use super::Side;
use super::TuiView;
use super::layout::{GAP, Hidden, PAD, cols};

const SIZES: [(f32, f32); 5] = [
    (900., 520.),
    (1000., 700.),
    (1200., 800.),
    (1400., 900.),
    (1920., 1080.),
];

/// (design key, selector in the view).
const MAP: [(&str, &str); 29] = [
    ("topbar", "header"),
    ("col.l", "col-left"),
    ("col.c", "col-centre"),
    ("col.r", "col-right"),
    ("panel:ws", "p-workspace"),
    ("panel:running", "p-running"),
    ("panel:sess", "p-session"),
    ("panel:tools", "p-tools"),
    ("panel:act", "p-activity"),
    ("panel:history", "p-history"),
    ("panel:topo1", "p-topology"),
    ("status-context", "status-context"),
    ("panel:agent", "p-agent"),
    ("panel:ctx", "p-context"),
    ("panel:use1", "p-usage"),
    ("panel:chat", "p-chat"),
    ("chead", "chead"),
    ("tabs2", "tabs"),
    ("tab#0", "tab-0"),
    ("tab#1", "tab-1"),
    ("tab#2", "tab-2"),
    ("tab#3", "tab-3"),
    ("tab#4", "tab-4"),
    ("inputbar", "prompt"),
    ("sendbtn", "send"),
    ("agent-switch", "agent-switch"),
    ("modebar", "modebar"),
    ("footer", "footer"),
    ("hdr:ws", "p-workspace-head"),
];

#[derive(PartialEq)]
enum Group {
    Left,
    Centre,
    Right,
    Full,
}

fn group(key: &str) -> Group {
    match key {
        "col.l" | "panel:ws" | "panel:running" | "panel:sess" | "panel:tools" | "panel:act"
        | "panel:history" | "hdr:ws" => Group::Left,
        "col.r" | "panel:topo1" | "status-context" | "panel:agent" | "panel:ctx" | "panel:use1" => {
            Group::Right
        }
        "topbar" | "footer" => Group::Full,
        _ => Group::Centre,
    }
}

/// Tolerance in px per (key, axis). 1px everywhere the box does not depend on text wrapping; the boxes whose
/// height follows wrapped text (browser and GPUI break lines at different places) are listed with why.
fn tol(key: &str, w: f32) -> (f32, f32) {
    match key {
        // Session wraps its values ("claude-3.5-sonnet" breaks at the hyphen in Chrome, not in GPUI) in the
        // columns narrower than 250px, and its height feeds the panels below it and Agent Status.
        "panel:sess" | "panel:tools" | "panel:act" | "panel:history" if w < 1400. => (1., 40.),
        // the Agent Status / Context row is as tall as the taller card, and both wrap in the narrow right
        // column (260-380px)
        "status-context" | "panel:agent" | "panel:ctx" | "panel:topo1" | "panel:use1"
            if w < 1400. =>
        {
            (1., 60.)
        }
        // Workspace/Running shrink in proportion to their content when the column is short; the content
        // heights are the design's to within a text line
        "panel:ws" | "panel:running" if w < 1400. => (1., 12.),
        // text rows: a 19.375px line box comes out 19.5px, so a panel of six rows is 0.75px taller and the
        // panels sharing the column with it give that back
        "panel:topo1" | "status-context" | "panel:agent" | "panel:ctx" | "panel:use1"
        | "panel:ws" | "panel:running" | "panel:sess" | "panel:tools" | "panel:act"
        | "panel:history" => (1., 1.5),
        _ => (1., 1.),
    }
}

fn open(cx: &mut TestAppContext, w: f32, h: f32) -> &mut VisualTestContext {
    open_with(cx, w, h, Hidden::default())
}

fn open_with(cx: &mut TestAppContext, w: f32, h: f32, hidden: Hidden) -> &mut VisualTestContext {
    let (_, v) = cx.add_window_view(|_, _| TuiView::new(0., hidden));
    v.simulate_resize(size(px(w), px(h)));
    v.run_until_parked();
    v
}

fn rects() -> Value {
    serde_json::from_str(include_str!("../../tests/design/cli-rects.json")).unwrap()
}

fn f(p: Pixels) -> f32 {
    f32::from(p)
}

fn get(v: &mut VisualTestContext, sel: &'static str) -> Option<Bounds<Pixels>> {
    v.debug_bounds(sel)
}

/// Every mapped rectangle at every size; prints a table of design vs GPUI and fails on a deviation beyond
/// the tolerance of [`tol`]. Elements the design hides at a size (Usage under 500px of column) must be absent.
#[gpui::test]
fn design_rects(cx: &mut TestAppContext) {
    let all = rects();
    let mut bad = vec![];
    for (w, h) in SIZES {
        let key = format!("{}x{}", w as u32, h as u32);
        let d = &all[&key]["rects"];
        let v = open(cx, w, h);
        println!(
            "\n== {key} (design topology mode {})",
            all[&key]["topo_mode"]
        );
        println!("element          design x,y,w,h             gpui x,y,w,h               max dev");
        for (dk, sel) in MAP {
            let Some(r) = d.get(dk).and_then(Value::as_array) else {
                continue;
            };
            let want: Vec<f32> = r.iter().map(|x| x.as_f64().unwrap() as f32).collect();
            let hidden = want[2] == 0. && want[3] == 0.;
            let c = cols(w);
            let grp = group(dk);
            // a collapsed column is a drawer: not in the layout
            let collapsed =
                (grp == Group::Left && !c.left_inline) || (grp == Group::Right && !c.right_inline);
            let got = get(v, sel);
            match (got, hidden || collapsed) {
                (None, true) => {
                    println!("{dk:16} {:>26} {:>26}  hidden in both", "hidden", "hidden");
                    continue;
                }
                (Some(_), true) => {
                    bad.push(format!("{key} {dk}: hidden in the design, drawn here"));
                    continue;
                }
                (None, false) => {
                    bad.push(format!("{key} {dk}: missing"));
                    continue;
                }
                _ => {}
            }
            let g = got.unwrap();
            let have = [
                f(g.origin.x),
                f(g.origin.y),
                f(g.size.width),
                f(g.size.height),
            ];
            let mut want = want;
            // the left column is 220px (design: 180/190/200 below 1400): its panels are wider by the difference
            if grp == Group::Left {
                want[2] += c.left - d["col.l"][2].as_f64().unwrap() as f32;
            }
            let mut dev: Vec<f32> = (0..4).map(|i| have[i] - want[i]).collect();
            // x and width of the centre and right columns follow the owner's rule, not the design
            if matches!(grp, Group::Centre | Group::Right) {
                dev[0] = 0.;
                dev[2] = 0.;
            }
            let (tw, th) = tol(dk, w);
            // the right column is narrower than the design's at every inline width: its cards wrap more
            let th = if grp == Group::Right && dk != "col.r" {
                th.max(80.)
            } else {
                th
            };
            // gpui rounds every 19.375px text line up to 19.5px: a panel of n rows is n/8 px taller
            let tols = [tw, tw.max(th), tw, th];
            // an element below a taller/shorter text-dependent one moves with it: y gets the height tolerance
            let ok = (0..4).all(|i| dev[i].abs() <= tols[i] + 0.5);
            let mx = dev.iter().fold(0f32, |a, b| a.max(b.abs()));
            println!(
                "{dk:16} {:>26} {:>26}  {mx:6.2}{}",
                format!(
                    "{:.1},{:.1},{:.1},{:.1}",
                    want[0], want[1], want[2], want[3]
                ),
                format!(
                    "{:.1},{:.1},{:.1},{:.1}",
                    have[0], have[1], have[2], have[3]
                ),
                if ok { "" } else { "  <-- OUT" }
            );
            if !ok {
                bad.push(format!("{key} {dk}: design {want:?} gpui {have:?}"));
            }
        }
    }
    assert!(
        bad.is_empty(),
        "layout deviates from cli.html:\n{}",
        bad.join("\n")
    );
}

/// The Topology graph: the mode the body size picks and every card rectangle, against the browser's
/// `.ncard` boxes (relative to the topology grid, which starts at the panel body plus its 12px side padding).
#[gpui::test]
fn topology_cards(cx: &mut TestAppContext) {
    use super::sample::Pos;
    use super::topo::{Mode, geometry};
    let all = rects();
    let mut bad = vec![];
    for (w, h) in SIZES {
        let key = format!("{}x{}", w as u32, h as u32);
        let d = &all[&key]["rects"];
        let v = open(cx, w, h);
        let body = v.debug_bounds("p-topology-body");
        let (aw, ah) = body.map_or((0., 0.), |b| (f(b.size.width) - 24., f(b.size.height) - 2.));
        // the geometry is judged on the design's own grid size: panels above it are text-dependent
        let g = d["topo1g"].as_array().unwrap();
        let (gw, gh) = (g[2].as_f64().unwrap() as f32, g[3].as_f64().unwrap() as f32);
        let geo = geometry(gw, gh);
        // the right column is narrower than the design's (owner rule), so the body can pick another mode:
        // `topology_fits_the_narrow_column` checks that one. This test is the pure geometry on the design's grid.
        let _ = (aw, ah);
        let want_mode = match all[&key]["topo_mode"].as_str().unwrap() {
            "A" => Mode::A,
            "B" => Mode::B,
            _ => Mode::T,
        };
        if geo.mode != want_mode {
            bad.push(format!(
                "{key}: mode {:?} (design grid size) != design {want_mode:?}",
                geo.mode
            ));
            continue;
        }
        let (gx, gy) = (g[0].as_f64().unwrap() as f32, g[1].as_f64().unwrap() as f32);
        println!("\n== {key} topology {want_mode:?} (design grid {gw:.1}x{gh:.1})");
        for (name, pos) in [
            ("user", Some(Pos::User)),
            ("hooks", Some(Pos::Hooks)),
            ("guards", Some(Pos::Guards)),
            ("sub", Some(Pos::Sub)),
            ("skills", Some(Pos::Skills)),
            ("mcp", Some(Pos::Mcp)),
            ("tools", Some(Pos::Tools)),
            ("mem", Some(Pos::Mem)),
            ("core", None),
        ] {
            let Some(r) = d.get(format!("ncard:{name}")).and_then(Value::as_array) else {
                continue;
            };
            let want: Vec<f32> = r.iter().map(|x| x.as_f64().unwrap() as f32).collect();
            let want = [want[0] - gx, want[1] - gy, want[2], want[3]];
            let got = pos.map_or(geo.core, |p| geo.rect(p));
            let have = [got.x, got.y, got.w, got.h];
            // owner request (departs from cli.html): in the A/B grids the cards fill the panel (three equal
            // columns, rows share the height, the core fills the middle cell), all eight the same size. So the
            // left column is compared by x only; the rest must just be equal to each other.
            let grid = geo.mode != Mode::T;
            let side = matches!(pos, Some(Pos::User | Pos::Sub | Pos::Mcp));
            let cmp: &[usize] = match (grid, side, pos.is_some()) {
                (false, ..) => &[0, 1, 2, 3],
                (true, true, _) => &[0],
                (true, false, _) => &[],
            };
            let mx = cmp
                .iter()
                .fold(0f32, |a, &i| a.max((have[i] - want[i]).abs()));
            if grid && pos.is_some() {
                let u = geo.rect(Pos::User);
                if (got.w - u.w).abs() > 0.01 || (got.h - u.h).abs() > 0.01 {
                    bad.push(format!(
                        "{key} {name}: size {}x{} != the other cards'",
                        got.w, got.h
                    ));
                }
            }
            println!("{name:8} design {want:.1?} gpui {have:.1?} dev {mx:.2}");
            if mx > 1.5 {
                bad.push(format!("{key} {name}: design {want:?} gpui {have:?}"));
            }
        }
    }
    assert!(
        bad.is_empty(),
        "topology deviates from cli.html:\n{}",
        bad.join("\n")
    );
}

fn near(a: f32, e: f32, what: &str) {
    assert!((a - e).abs() < 1.5, "{what}: {a} != {e}");
}

fn bounds(v: &mut VisualTestContext, sel: &'static str) -> Bounds<Pixels> {
    get(v, sel).unwrap_or_else(|| panic!("no element {sel}"))
}

/// The `synt.tui.panels.*` keys: a hidden panel gives its space away.
#[gpui::test]
fn hiding_panels_gives_their_space_away(cx: &mut TestAppContext) {
    let mut h = Hidden::default();
    for k in ["session", "mcp", "tools", "activity"] {
        h.hide(k);
    }
    let v = open_with(cx, 1400., 900., h);
    let (ws, run) = (bounds(v, "p-workspace"), bounds(v, "p-running"));
    near(
        f(ws.size.height),
        f(run.size.height),
        "Workspace and Running split the column",
    );
    near(
        f(ws.size.height) + 8. + f(run.size.height),
        f(bounds(v, "col-left").size.height),
        "the split fills the column",
    );
    assert!(get(v, "p-history").is_none() && get(v, "p-activity").is_none());

    let mut h = Hidden::default();
    h.hide("status");
    h.hide("tokens");
    let v = open_with(cx, 1400., 900., h);
    assert!(get(v, "status-context").is_none());
    let (t, u) = (bounds(v, "p-topology"), bounds(v, "p-usage"));
    near(
        f(t.size.height) + 10. + f(u.size.height),
        f(bounds(v, "col-right").size.height),
        "right column fills",
    );

    let mut h = Hidden::default();
    h.hide("status");
    let v = open_with(cx, 1400., 900., h);
    near(
        f(bounds(v, "p-context").size.width),
        f(bounds(v, "status-context").size.width),
        "Context takes the whole row",
    );

    let mut h = Hidden::default();
    for k in ["topology", "usage", "status", "tokens"] {
        h.hide(k);
    }
    let v = open_with(cx, 1400., 900., h);
    assert!(get(v, "col-right").is_none());
    near(
        f(bounds(v, "col-centre").bottom_right().x),
        1400. - 10.,
        "the chat spans to the right edge",
    );

    let mut h = Hidden::default();
    h.hide("usage");
    let v = open_with(cx, 1400., 900., h);
    let (t, c) = (bounds(v, "p-topology"), bounds(v, "status-context"));
    near(
        f(t.size.height) + 10. + f(c.size.height),
        f(bounds(v, "col-right").size.height),
        "topology takes the rest",
    );
}

/// Under 500px of column height Usage goes, under 400 the cards drop rows (`fitRight`), as in the design.
#[gpui::test]
fn the_short_window_drops_usage(cx: &mut TestAppContext) {
    let v = open(cx, 1400., 900.);
    assert!(get(v, "p-usage").is_some());
    // the column is the window minus header, footer and padding (94px): 560 - 94 = 466 < 500
    let v = open(cx, 1400., 560.);
    assert!(get(v, "p-usage").is_none());
    let v = open(cx, 1400., 600.);
    assert!(get(v, "p-usage").is_some(), "506 >= 500");
}

const WIDE: [(f32, f32); 7] = [
    (900., 520.),
    (1000., 700.),
    (1200., 800.),
    (1400., 900.),
    (1920., 1080.),
    (2560., 1440.),
    (3440., 1440.),
];

/// The owner rule: the chat column is at least 55% of the window width at every size, the columns do not
/// overlap, and the chat frame is the tallest element (it fills the content height). Prints the table.
#[gpui::test]
fn chat_share(cx: &mut TestAppContext) {
    println!("size        left    centre  right   chat share");
    for (w, h) in WIDE {
        let v = open(cx, w, h);
        let win = bounds(v, "view");
        let content_h = h - 46. - 28. - 2. * PAD;
        let mut cols_x = vec![];
        let mut widths = [0f32; 3];
        for (i, sel) in ["col-left", "col-centre", "col-right"]
            .into_iter()
            .enumerate()
        {
            if let Some(b) = get(v, sel) {
                widths[i] = f(b.size.width);
                cols_x.push((f(b.origin.x), f(b.bottom_right().x), sel));
                near(f(b.size.height), content_h, &format!("{sel} height"));
            }
        }
        let share = widths[1] / w;
        println!(
            "{:4}x{:<5} {:6.0} {:7.0} {:6.0}   {:5.1}%",
            w,
            h,
            widths[0],
            widths[1],
            widths[2],
            share * 100.
        );
        assert!(share >= 0.55, "{w}x{h}: chat is {:.1}%", share * 100.);
        assert_eq!(f(win.size.width), w);
        // in order, one GAP apart, inside the window padding
        near(cols_x[0].0, PAD, "first column at the padding");
        for p in cols_x.windows(2) {
            near(
                p[1].0 - p[0].1,
                GAP,
                &format!("gap {} -> {}", p[0].2, p[1].2),
            );
        }
        near(
            cols_x.last().unwrap().1,
            w - PAD,
            "last column at the right padding",
        );
        // the chat frame fills its column and no panel of the side columns is taller
        let chat = f(bounds(v, "p-chat").size.height);
        near(chat, content_h, "chat fills the content height");
        for sel in ["p-workspace", "p-topology", "p-usage", "p-running"] {
            if let Some(b) = get(v, sel) {
                assert!(f(b.size.height) <= chat, "{sel} taller than the chat");
            }
        }
        // the centre widths of the rule
        let c = cols(w);
        near(
            widths[1],
            w - 2. * PAD
                - if c.left_inline { c.left + GAP } else { 0. }
                - if c.right_inline { c.right + GAP } else { 0. },
            "centre = the rest",
        );
    }
}

/// Collapsed columns are drawers: closed by default, opened by `toggle_drawer`, one at a time, drawn
/// over the chat without moving it, closed by the same call.
#[gpui::test]
fn drawers(cx: &mut TestAppContext) {
    for (w, h, left_drawer, right_drawer) in [
        (900., 520., true, true),
        (1000., 700., false, true),
        (1200., 800., false, true),
        (1400., 900., false, false),
    ] {
        let (tui, v) = cx.add_window_view(|_, _| TuiView::new(0., Hidden::default()));
        v.simulate_resize(size(px(w), px(h)));
        v.run_until_parked();
        assert!(get(v, "drawer").is_none(), "{w}: closed by default");
        assert_eq!(get(v, "col-left").is_none(), left_drawer, "{w} left inline");
        assert_eq!(
            get(v, "col-right").is_none(),
            right_drawer,
            "{w} right inline"
        );
        assert_eq!(get(v, "drawer-left").is_some(), left_drawer);
        assert_eq!(get(v, "drawer-right").is_some(), right_drawer);
        let chat = bounds(v, "col-centre");
        for (side, on) in [(Side::Left, left_drawer), (Side::Right, right_drawer)] {
            if !on {
                continue;
            }
            tui.update(v, |t, cx| t.toggle_drawer(side, cx));
            v.run_until_parked();
            let dr = bounds(v, "drawer");
            let (sel, cw) = match side {
                Side::Left => ("col-left", cols(w).left),
                Side::Right => ("col-right", cols(w).right),
            };
            let col = bounds(v, sel);
            near(f(col.size.width), cw, "drawer column width");
            assert!(
                f(col.origin.x) >= f(dr.origin.x)
                    && f(col.bottom_right().x) <= f(dr.bottom_right().x) + 0.5
            );
            near(f(dr.origin.y), 46., "drawer under the header");
            near(f(dr.bottom_right().y), h - 28., "drawer over the footer");
            assert_eq!(bounds(v, "col-centre"), chat, "the chat does not move");
            if side == Side::Right {
                near(
                    f(col.bottom_right().x),
                    w - PAD,
                    "right drawer at the right edge",
                );
            }
            // toggling the other side swaps, the same side closes
            tui.update(v, |t, cx| t.toggle_drawer(side, cx));
            v.run_until_parked();
            assert!(get(v, "drawer").is_none(), "{w}: closed again");
        }
        if left_drawer {
            tui.update(v, |t, cx| t.toggle_drawer(Side::Left, cx));
            tui.update(v, |t, cx| t.toggle_drawer(Side::Right, cx));
            v.run_until_parked();
            assert!(get(v, "col-right").is_some() && get(v, "col-left").is_none());
        }
        // growing the window turns an open drawer into the inline column
        tui.update(v, |t, _| t.drawer = Some(Side::Right));
        v.simulate_resize(size(px(1920.), px(1080.)));
        v.run_until_parked();
        assert!(get(v, "drawer").is_none() && get(v, "col-right").is_some());
    }
    // a hidden right column has no button
    let mut hid = Hidden::default();
    for k in ["topology", "usage", "status", "tokens"] {
        hid.hide(k);
    }
    let v = open_with(cx, 900., 520., hid);
    assert!(get(v, "drawer-right").is_none() && get(v, "drawer-left").is_some());
    near(
        f(bounds(v, "col-centre").size.width),
        880.,
        "chat takes the hidden column's space",
    );
}

/// Topology and Usage inside the narrower right column (inline from 1280, or as a drawer at 300px): no card
/// leaves the body or overlaps another, the panel stays inside its column.
#[gpui::test]
fn topology_fits_the_narrow_column(cx: &mut TestAppContext) {
    use super::topo::geometry;
    for (w, h, drawer) in [
        (900., 520., true),
        (1000., 700., true),
        (1200., 800., true),
        (1280., 800., false),
        (1400., 900., false),
        (1920., 1080., false),
        (3440., 1440., false),
    ] {
        let (tui, v) = cx.add_window_view(|_, _| TuiView::new(0., Hidden::default()));
        v.simulate_resize(size(px(w), px(h)));
        v.run_until_parked();
        if drawer {
            tui.update(v, |t, cx| t.toggle_drawer(Side::Right, cx));
            v.run_until_parked();
        }
        let col = bounds(v, "col-right");
        let body = bounds(v, "p-topology-body");
        let topo = bounds(v, "p-topology");
        let (aw, ah) = (f(body.size.width) - 24., f(body.size.height) - 2.);
        let geo = geometry(aw, ah);
        println!(
            "{w}x{h} column {:.0} topology body {aw:.0}x{ah:.0} mode {:?}",
            f(col.size.width),
            geo.mode
        );
        assert!(aw >= 300. - 24. - 20.);
        assert!(
            topo.origin.x >= col.origin.x
                && topo.bottom_right().x <= col.bottom_right().x + px(0.5)
        );
        assert!(topo.bottom_right().y <= col.bottom_right().y + px(0.5));
        let mut rs: Vec<_> = geo.nodes.iter().map(|(_, r)| *r).collect();
        rs.push(geo.core);
        for (i, r) in rs.iter().enumerate() {
            // below 750px of window height the design itself lets the T list run past a 150px panel (its grid is
            // 168 tall in a 120 body); the height is judged from there up
            let fits_y = h < 750. || (r.y >= -0.5 && r.y + r.h <= ah + 0.5);
            assert!(
                r.x >= -0.5 && r.x + r.w <= aw + 0.5 && fits_y,
                "{w}: card {i} {r:?} leaves the {aw}x{ah} body"
            );
            for q in &rs[i + 1..] {
                let apart = r.x + r.w <= q.x + 0.5
                    || q.x + q.w <= r.x + 0.5
                    || r.y + r.h <= q.y + 0.5
                    || q.y + q.h <= r.y + 0.5;
                assert!(apart, "{w}: {r:?} overlaps {q:?}");
            }
        }
        if let Some(u) = get(v, "p-usage") {
            assert!(
                u.origin.x >= col.origin.x && u.bottom_right().x <= col.bottom_right().x + px(0.5)
            );
            assert!(u.bottom_right().y <= col.bottom_right().y + px(0.5));
        }
    }
}
