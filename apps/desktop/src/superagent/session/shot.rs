//! Development only (feature `shot`): the outcome and worker frames on the sample data, inside the panel's frame,
//! offscreen to a PNG. `cargo run -p synthesis-desktop --features shot --example session_shot -- /dir` writes
//! `frame-{outcome,outcome-loading,outcome-error,worker-live,worker-queued,worker-finished,worker-off}.png` (the main
//! thread is needed, so it is an example, not a test). The text of the messages and events is built here, never a
//! real secret.

use std::{path::Path, sync::Arc};

use gpui::{
    AnyView, AppContext as _, Context, HeadlessAppContext, IntoElement, ParentElement as _, Render,
    Styled as _, Window, div, px, size,
};
use serde_json::json;
use synthesis_view::outcomes::{InspectorContent, MessageRow, MessageState};

use super::outcome::OutcomeFrame;
use super::worker::WorkerFrame;
use crate::outcomes_view::OutcomesView;
use crate::outcomes_view::model::Screen;
use crate::superagent::tokens::{self as t, hsla};

/// The panel of the shell (r 12, `--bg`, 1 px stroke) holding a frame.
struct Panel(AnyView);

impl Render for Panel {
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

fn msg(
    seq: u64,
    state: MessageState,
    attempt: Option<&str>,
    item: Option<&str>,
    text: &str,
) -> MessageRow {
    MessageRow {
        seq,
        state,
        attempt: attempt.map(Into::into),
        work_item: item.map(Into::into),
        text: text.into(),
    }
}

/// `state`: outcome, outcome-loading, outcome-error, worker-live, worker-queued, worker-finished, worker-off.
pub fn render(path: &Path, state: &str, (w, h): (f32, f32)) -> Result<(), Err> {
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
    let state = state.to_string();
    let win = cx.open_window(size(px(w), px(h)), move |window, cx| {
        let o = cx.new(OutcomesView::sample);
        o.update(cx, |o, _| {
            o.state.attempt_message = state != "worker-off";
            match state.as_str() {
                "outcome-loading" => o.state.detail = None,
                "outcome-error" => o.state.screen = Screen::Unreachable("no endpoint published".into()),
                _ => {}
            }
            if let Some(d) = o.state.detail.as_mut() {
                d.messages = vec![
                    msg(5, MessageState::Delivered, Some("at4"), None, "Use the REST shape and keep the old API answering."),
                    msg(6, MessageState::Live, Some("at4"), None, "Skip the flaky integration test for now."),
                    msg(7, MessageState::Queued, None, Some("wi4"), "When you start, read auth/refresh.rs first."),
                ];
                let evs: Vec<_> = (1..=6)
                    .map(|i| json!({"seq": i, "type": "attempt.progress", "payload": {"attempt": "at4", "step": i, "note": "ran cargo test -p auth"}}))
                    .collect();
                d.history = InspectorContent::activity(&evs, 6);
            }
        });
        let view: AnyView = if state.starts_with("outcome") {
            cx.new(|cx| OutcomeFrame::new(o, "o_auth".into(), cx)).into()
        } else {
            let item = match state.as_str() {
                "worker-queued" => "wi4",
                "worker-finished" => "wi1",
                _ => "wi3",
            };
            o.update(cx, |o, _| o.state.selected = Some("o_auth".into()));
            cx.new(|cx| WorkerFrame::new(o, "o_auth".into(), item.into(), "", window, cx))
                .into()
        };
        cx.new(|_| Panel(view))
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
