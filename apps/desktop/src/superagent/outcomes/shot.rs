//! Development only (feature `shot`): `synthesis-desktop --shot out.png --view super-agent --sample [--size WxH]
//! [--mode super-agent|tui] [--badge] [--tools N] [--rail-hidden] [--popup decision|decision-focus|decision-expanded]
//! [--screen ready|off|windows|unreachable|notready] [--guide] [--focus cancel|confirm] [--centre goal|decisions|history]
//! [--inspector overview|workers|activity|diff|evidence] [--pane list|centre|inspector] [--outcome ID|none]
//! [--card N] [--choose N]` draws the whole shell with the
//! Super Agent view on its sample data, offscreen (no window server), and writes the frame to a PNG. Nothing is read or
//! sent. `--mode tui` draws the TUI view's window instead (its title bar with the badge, `--badge`); `--popup` shows the
//! docked decision card (it needs a centre tab other than Decisions, so it implies `--centre goal`).

use std::{path::PathBuf, sync::Arc};

use gpui::{AppContext as _, HeadlessAppContext, px, size};
use synthesis_view::outcomes::InspectorTab;

use super::model::{CentreTab, Pane, Screen};
use crate::tui_view::{TuiView, layout::Hidden};

pub struct Args {
    path: PathBuf,
    w: f32,
    h: f32,
    screen: String,
    centre: CentreTab,
    itab: InspectorTab,
    pane: Pane,
    outcome: Option<String>,
    card: usize,
    choose: Option<usize>,
    /// `--frame new-goal|new-ro-goal`: that New goal frame is open (the form of the Super Agent tab).
    frame: Option<String>,
    /// The disclosure of the Off / not-migrated screen is open.
    guide: bool,
    focus: Option<String>,
    /// `--mode tui`: the TUI view is the one shown (the title bar with the badge).
    tui_mode: bool,
    /// The Super Agent button counts what waits (the sample's decision, and `tools` sessions).
    badge: bool,
    tools: usize,
    rail_hidden: bool,
    popup: Option<String>,
}

/// `None` unless `--shot` comes with `--view outcomes`.
pub fn parse(args: &[String]) -> Option<Args> {
    let val = |flag: &str| {
        args.iter()
            .position(|a| a == flag)
            .and_then(|i| args.get(i + 1))
    };
    if val("--view").map(String::as_str) != Some("super-agent") {
        return None;
    }
    let (w, h) = val("--size")
        .and_then(|s| s.split_once('x'))
        .and_then(|(w, h)| Some((w.parse().ok()?, h.parse().ok()?)))
        .unwrap_or((1400., 900.));
    Some(Args {
        path: PathBuf::from(val("--shot")?),
        w,
        h,
        screen: val("--screen").cloned().unwrap_or_else(|| "ready".into()),
        centre: match val("--centre").map(String::as_str) {
            Some("goal") => CentreTab::Goal,
            Some("history") => CentreTab::History,
            Some("decisions") => CentreTab::Decisions,
            _ if val("--popup").is_some() => CentreTab::Goal,
            _ => CentreTab::Decisions,
        },
        itab: val("--inspector")
            .and_then(|t| InspectorTab::parse(t))
            .unwrap_or(InspectorTab::Overview),
        pane: match val("--pane").map(String::as_str) {
            Some("centre") => Pane::Centre,
            Some("inspector") => Pane::Inspector,
            _ => Pane::List,
        },
        outcome: val("--outcome").cloned(),
        card: val("--card").and_then(|n| n.parse().ok()).unwrap_or(0),
        choose: val("--choose").and_then(|n| n.parse().ok()),
        frame: val("--frame").cloned(),
        guide: args.iter().any(|a| a == "--guide"),
        focus: val("--focus").cloned(),
        tui_mode: val("--mode").map(String::as_str) == Some("tui"),
        badge: args.iter().any(|a| a == "--badge"),
        tools: val("--tools").and_then(|n| n.parse().ok()).unwrap_or(0),
        rail_hidden: args.iter().any(|a| a == "--rail-hidden"),
        popup: val("--popup").cloned(),
    })
}

type Err = Box<dyn std::error::Error + Send + Sync>;

pub fn render(a: Args) -> Result<(), Err> {
    let text = gpui_platform::current_platform(true).text_system();
    let mut cx = HeadlessAppContext::with_platform(
        text,
        Arc::new(crate::brand::Assets),
        gpui_platform::current_headless_renderer,
    );
    cx.update(|cx| {
        gpui_component::init(cx);
        crate::ui::register_fonts(cx);
        crate::theme::apply("JetBrains Mono", cx);
    });
    let win = cx.open_window(size(px(a.w), px(a.h)), |window, cx| {
        let tui = cx.new(|_| TuiView::new(0., Hidden::default()));
        let shell = cx.new(|cx| crate::ui::Shell::around(window, cx, tui, Err("--shot".into())));
        shell.update(cx, |s, cx| {
            if !a.tui_mode {
                s.go(crate::ui::View::SuperAgent, window, cx);
            }
            if a.badge {
                s.count_waiting_in_sample(cx);
            }
            s.outcomes().update(cx, |o, cx| {
                o.sample_clock();
                o.set_waiting_tools(
                    (0..a.tools).map(|i| (format!("s{i}"), format!("session {i}"))).collect(),
                    cx,
                );
                if a.rail_hidden {
                    o.toggle_rail(cx);
                }
                let st = &mut o.state;
                st.screen = match a.screen.as_str() {
                    "off" => Screen::Off,
                    "windows" => Screen::Windows,
                    "notready" => Screen::NotReady(
                        "the orchestrator is on but the session store is not migrated".into(),
                    ),
                    "unreachable" => Screen::Unreachable(
                        "the orchestrator is not running (no endpoint published). Start it with `synt outcome list` in a terminal, then press r.".into(),
                    ),
                    _ => Screen::Ready,
                };
                if a.outcome.as_deref() == Some("none") {
                    (st.selected, st.detail, st.inspector) = (None, None, None);
                } else if let Some(id) = &a.outcome {
                    super::sample::show(st, id, a.itab);
                } else if let Some(i) = st.items.first().cloned() {
                    st.itab = a.itab;
                    st.inspector = Some(super::sample::inspector(a.itab, &i));
                }
                st.centre = a.centre;
                st.pane = a.pane;
                st.show_card(a.card, 0);
                if let Some(c) = a.choose {
                    st.choose(c);
                }
                if a.guide {
                    st.open_guide();
                    if let Some(g) = st.guide.as_mut() {
                        g.focus = match a.focus.as_deref() {
                            Some("cancel") => Some(super::model::Btn::Cancel),
                            Some("confirm") => Some(super::model::Btn::Confirm),
                            _ => None,
                        };
                    }
                }
                cx.notify();
            });
        });
        if let Some(f) = a.frame.as_deref() {
            let kind = if f == "new-ro-goal" {
                crate::superagent::frames::GoalKind::ReadOnly
            } else {
                crate::superagent::frames::GoalKind::Writes
            };
            shell.update(cx, |s, cx| {
                s.superagent()
                    .update(cx, |v, cx| v.open(crate::superagent::frames::Frame::NewGoal(kind), cx))
            });
        }
        shell
    })?;
    cx.run_until_parked();
    // the card holds the window's focus when it has the keys: set once the first frame is drawn
    if let Some(p @ ("decision-focus" | "decision-expanded")) = a.popup.as_deref() {
        win.update(&mut cx, |shell, window, cx| {
            shell.outcomes().clone().update(cx, |o, cx| {
                o.focus_decision(window, cx);
                if p == "decision-expanded" {
                    o.expand_decision(cx);
                }
            })
        })?;
        cx.run_until_parked();
    }
    let img = cx.capture_screenshot(win.into())?;
    img.save(&a.path)?;
    eprintln!(
        "wrote {} ({}x{})",
        a.path.display(),
        img.width(),
        img.height()
    );
    Ok(())
}
