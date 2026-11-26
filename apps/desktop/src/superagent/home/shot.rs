//! Development only (feature `shot`): the home alone, on the sample data, inside the panel's frame, offscreen to a PNG.
//! `cargo run -p synthesis-desktop --features shot --example home_shot -- /dir` writes
//! `home-{normal,card,empty,loading,error}.png` (the main thread is needed, so it is an example, not a test).
//! `main.rs` does not know this view: the shell's own `--view super-agent` shot shows the home once it is wired in.

use std::{path::Path, sync::Arc};

use gpui::{
    AppContext as _, Context, Entity, HeadlessAppContext, IntoElement, ParentElement as _, Render,
    Styled as _, Window, div, px, size,
};

use super::HomeView;
use crate::superagent::outcomes::OutcomesView;
use crate::superagent::outcomes::model::Screen;
use crate::superagent::tokens::{self as t, hsla};

/// The panel of the shell (r 12, `--bg`, 1 px stroke) holding the home.
struct Frame(Entity<HomeView>);

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

/// `state`: normal, queue, card, empty, loading, error.
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
    let win = cx.open_window(size(px(w), px(h)), |window, cx| {
        let o = cx.new(OutcomesView::sample);
        o.update(cx, |o, cx| {
            o.sample_clock();
            if state == "normal" || state == "queue" {
                o.set_waiting_tools(vec![("s1".into(), "Fix the flaky login test".into())], cx);
            }
            if state == "queue" {
                o.state.queue = Some(crate::superagent::outcomes::sample::queue());
            }
            match state {
                "empty" => o.state.items.clear(),
                "loading" => o.state.screen = Screen::Loading,
                "error" => o.state.screen = Screen::Unreachable("no endpoint published".into()),
                _ => {}
            }
        });
        let home = cx.new(|cx| {
            let mut v = HomeView::new(o, cx);
            v.shown(window, cx);
            if state == "card" {
                v.review("o_auth", cx);
            }
            if state == "queue" {
                v.show_lapsed = true;
                v.show_answered = true;
            }
            v
        });
        cx.new(|_| Frame(home))
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
