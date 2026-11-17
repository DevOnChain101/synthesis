//! Development only (feature `shot`): `synthesis-desktop --shot out.png --view outcomes --sample [--size WxH]
//! [--screen ready|off|windows|unreachable|notready] [--centre goal|decisions|history]
//! [--inspector overview|workers|activity|diff|evidence] [--pane list|centre|inspector] [--outcome ID]
//! [--card N] [--choose N] [--form new|new-errors|revise|review-new|review-revise|stale]` draws the whole shell with the Outcomes tab on its sample data, offscreen (no
//! window server), and writes the frame to a PNG. Nothing is read or sent.

use std::{path::PathBuf, sync::Arc};

use gpui::{AppContext as _, HeadlessAppContext, px, size};
use synthesis_view::outcomes::InspectorTab;

use super::form::FormText;
use super::link::{FormMsg, STALE};
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
    form: Option<String>,
}

/// `None` unless `--shot` comes with `--view outcomes`.
pub fn parse(args: &[String]) -> Option<Args> {
    let val = |flag: &str| {
        args.iter()
            .position(|a| a == flag)
            .and_then(|i| args.get(i + 1))
    };
    if val("--view").map(String::as_str) != Some("outcomes") {
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
        form: val("--form").cloned(),
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
            s.go(crate::ui::View::Outcomes, window, cx);
            s.outcomes().update(cx, |o, cx| {
                let st = &mut o.state;
                st.screen = match a.screen.as_str() {
                    "off" => Screen::Off,
                    "windows" => Screen::Windows,
                    "notready" => Screen::NotReady(
                        "the orchestrator is on but the store is not migrated: run `synt outcome enable` in a terminal, then press r".into(),
                    ),
                    "unreachable" => Screen::Unreachable(
                        "the orchestrator is not running (no endpoint published). Start it with `synt outcome list` in a terminal, then press r.".into(),
                    ),
                    _ => Screen::Ready,
                };
                if let Some(id) = &a.outcome {
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
                if let Some(kind) = a.form.as_deref() {
                    form_state(st, kind);
                }
                cx.notify();
            });
        });
        shell
    })?;
    cx.run_until_parked();
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

/// The form in the state `kind` names (the inputs are made when the view draws; they start with the form's text).
fn form_state(st: &mut super::model::State, kind: &str) {
    let revise = kind.contains("revise") || kind == "stale";
    if revise {
        st.open_revise();
    } else {
        st.open_new();
        if let Some(f) = st.form.as_mut() {
            f.head_known(&st.workspace, Ok(super::sample::COMMIT.into()));
        }
    }
    match kind {
        "new-errors" => {
            let t = FormText {
                workspace: st.workspace.clone(),
                ..Default::default()
            };
            st.submit(&t);
        }
        "review-new" | "review-revise" => {
            st.apply_form(FormMsg::Review(Box::new(super::sample::review(revise))));
        }
        "stale" => st.apply_form(FormMsg::Failed {
            text: STALE.into(),
            spent: true,
        }),
        _ => {}
    }
}
