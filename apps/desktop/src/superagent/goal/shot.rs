//! Development only (feature `shot`): the goal form on sample data, inside the panel's frame, offscreen to a PNG.
//! `cargo run -p synthesis-desktop --features shot --example goal_shot -- /dir` writes `goal-{writes,ro}-{empty,
//! filled,errors,review,dialog}.png` (the main thread is needed, so it is an example, not a test).

use std::{path::Path, sync::Arc, time::Duration};

use gpui::{
    AppContext as _, Context, Entity, HeadlessAppContext, IntoElement, ParentElement as _, Render,
    Styled as _, Window, div, px, size,
};
use serde_json::json;
use synthesis_core::outcome::{Budgets, Contract};
use synthesis_view::outcomes::CriteriaView;

use super::GoalForm;
use super::model::Field;
use crate::superagent::frames::GoalKind;
use crate::superagent::outcomes::form::Review;
use crate::superagent::outcomes::link::FormMsg;
use crate::superagent::outcomes::sample;
use crate::superagent::tokens::{self as t, hsla};

/// The panel of the shell (r 12, `--bg`, 1 px stroke) holding the form.
struct Frame(Entity<GoalForm>);

impl Render for Frame {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div().size_full().p(px(t::S2)).bg(hsla(t::SURFACE)).child(
            div()
                .size_full()
                .rounded(px(t::R_PANEL))
                .bg(hsla(t::BG))
                .border_1()
                .border_color(t::stroke())
                .overflow_hidden()
                .child(self.0.clone()),
        )
    }
}

type Err = Box<dyn std::error::Error + Send + Sync>;

const HASH: &str = "3c9a41be7d02f58a6e1c90d4b7a3582f1e60c9d47b2a8f35e0614cd9a7b3e5e1";

fn read_only_review() -> Review {
    let c = Contract::read_only(
        "Which modules still use the old logger?".into(),
        &[
            "Names every module".into(),
            "Gives a file and line for each".into(),
        ],
        Budgets::without_cost(),
    );
    Review {
        outcome: "o_logger".into(),
        version: 1,
        hash: HASH.into(),
        contract: CriteriaView::from_json(
            &json!({"version": 1, "hash": HASH, "approved": false, "contract": c}),
        ),
        diff: None,
        key: "approve-sample".into(),
    }
}

/// `kind`: writes or ro. `state`: empty, filled, errors, review, dialog.
pub fn render(path: &Path, kind: &str, state: &str, (w, h): (f32, f32)) -> Result<(), Err> {
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
    let writes = kind == "writes";
    let (state, kind) = (
        state.to_string(),
        if writes {
            GoalKind::Writes
        } else {
            GoalKind::ReadOnly
        },
    );
    let win = cx.open_window(size(px(w), px(h)), move |window, cx| {
        let form = cx.new(|cx| {
            let mut g = GoalForm::new(kind, None, "/Users/dev/Vibe/api-gateway", window, cx);
            if writes {
                g.apply_form(
                    FormMsg::Head {
                        ws: "/Users/dev/Vibe/api-gateway".into(),
                        result: Ok("9433c678e4b1d2f0a7c35e8b61d9f4a02c7b5e13".into()),
                    },
                    cx,
                );
            }
            if state != "empty" {
                let t =
                    |g: &mut GoalForm,
                     f: Field,
                     v: &str,
                     w: &mut Window,
                     cx: &mut Context<GoalForm>| { g.set_text(f, v, w, cx) };
                if writes {
                    t(
                        &mut g,
                        Field::Goal,
                        "Move login to OIDC and keep the old API answering the same requests.",
                        window,
                        cx,
                    );
                    t(
                        &mut g,
                        Field::Criteria,
                        "Login through OIDC works with the test IdP\nThe old API still answers 200",
                        window,
                        cx,
                    );
                    t(&mut g, Field::Checks, "cargo test -p auth", window, cx);
                    t(&mut g, Field::Scope, "auth/**\ntests/**", window, cx);
                    t(&mut g, Field::Turns, "30", window, cx);
                } else {
                    t(
                        &mut g,
                        Field::Goal,
                        "Which modules still use the old logger?",
                        window,
                        cx,
                    );
                    t(
                        &mut g,
                        Field::Criteria,
                        "Names every module\nGives a file and line for each",
                        window,
                        cx,
                    );
                    t(
                        &mut g,
                        Field::Folder,
                        "/Users/dev/Vibe/api-gateway",
                        window,
                        cx,
                    );
                }
            }
            if state == "errors" {
                // a half-filled form sent: the errors show under their fields
                g.set_text(Field::Criteria, "", window, cx);
                if writes {
                    g.set_text(Field::Checks, "", window, cx);
                }
                g.send(cx);
            }
            if state == "review" || state == "dialog" {
                let r = if writes {
                    let mut r = sample::review(false);
                    if let Ok(c) = r.contract.as_mut() {
                        c.max_turns = Some(30);
                    }
                    r
                } else {
                    read_only_review()
                };
                g.apply_form(FormMsg::Review(Box::new(r)), cx);
            }
            if state == "dialog" {
                g.open_dialog(window, cx);
                if let Some(d) = g.dialog.as_mut() {
                    d.since -= Duration::from_millis(600);
                }
            }
            g
        });
        cx.new(|_| Frame(form))
    })?;
    cx.run_until_parked();
    let img = cx.capture_screenshot(win.into())?;
    img.save(path)?;
    eprintln!(
        "wrote {} ({}x{})",
        path.display(),
        img.width(),
        img.height()
    );
    Ok(())
}
