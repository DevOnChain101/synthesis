//! Acceptance tests of the TUI view against `cli.html`: the panel rectangles the design gives in a real
//! browser (`tests/design/cli-rects.json`, dumped with headless Chrome by `getBoundingClientRect` at the five
//! viewport sizes) against the bounds GPUI lays out for the sample data (which is the design's own sample).
//! Run `cargo test -p synthesis-desktop --lib design_rects -- --nocapture` for the comparison table.

use gpui::{Bounds, Pixels, TestAppContext, VisualTestContext, px, size};
use serde_json::Value;

use super::TuiView;
use super::layout::Hidden;

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
            let got = get(v, sel);
            match (got, hidden) {
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
            let dev: Vec<f32> = (0..4).map(|i| have[i] - want[i]).collect();
            let (tw, th) = tol(dk, w);
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
        let body = v.debug_bounds("p-topology-body").expect("topology body");
        let (aw, ah) = (f(body.size.width) - 24., f(body.size.height) - 2.);
        // the geometry is judged on the design's own grid size: panels above it are text-dependent
        let g = d["topo1g"].as_array().unwrap();
        let (gw, gh) = (g[2].as_f64().unwrap() as f32, g[3].as_f64().unwrap() as f32);
        let geo = geometry(gw, gh);
        let mode_here = geometry(aw, ah).mode;
        let want_mode = match all[&key]["topo_mode"].as_str().unwrap() {
            "A" => Mode::A,
            "B" => Mode::B,
            _ => Mode::T,
        };
        if mode_here != want_mode || geo.mode != want_mode {
            bad.push(format!(
                "{key}: mode {mode_here:?} (design grid size {:?}) != design {want_mode:?}",
                geo.mode
            ));
            continue;
        }
        let (gx, gy) = (g[0].as_f64().unwrap() as f32, g[1].as_f64().unwrap() as f32);
        println!("\n== {key} topology {want_mode:?} body {aw:.1}x{ah:.1}");
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
            let mx = (0..4).fold(0f32, |a, i| a.max((have[i] - want[i]).abs()));
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
