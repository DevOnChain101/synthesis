//! The window: a left rail (TUI view, Agent view, Settings) and the content of the active view.
//! Phase 0: the views are placeholders; later phases fill them from `engine` / `synthesis_view`.

use std::borrow::Cow;

use gpui::{
    App, Context, Entity, FontWeight, IntoElement, ParentElement as _, Render, Styled as _, Window,
    div, prelude::*, px, svg,
};

use crate::tui_view::{TuiView, layout};
use crate::{brand, theme};
use synthesis_tui_render::theme as t;

/// Icons the shell draws (checked by a test in `brand`).
pub const ICONS: [&str; 3] = ["square-terminal", "bot", "settings"];

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum View {
    Tui,
    Agent,
    Settings,
}

impl View {
    fn label(self) -> &'static str {
        match self {
            View::Tui => "TUI",
            View::Agent => "Agent",
            View::Settings => "Settings",
        }
    }
    fn icon(self) -> &'static str {
        match self {
            View::Tui => ICONS[0],
            View::Agent => ICONS[1],
            View::Settings => ICONS[2],
        }
    }
    fn blurb(self) -> &'static str {
        match self {
            View::Tui => "The classic TUI, drawn natively from the engine's events.",
            View::Agent => "Conversation, tool calls and approvals of the session.",
            View::Settings => "Model, permissions, appearance.",
        }
    }
}

/// Width of the rail; the TUI view takes the rest and lays itself out on that width.
pub const RAIL_W: f32 = 72.;

pub struct Shell {
    view: View,
    tui: Entity<TuiView>,
}

impl Shell {
    fn rail_button(&self, v: View, cx: &mut Context<Self>) -> impl IntoElement {
        let on = self.view == v;
        let fg = theme::hsla(if on { t::ACCENT } else { t::TEXT });
        div()
            .id(v.label())
            .flex()
            .flex_col()
            .items_center()
            .gap_1()
            .py_2()
            .w_full()
            .cursor_pointer()
            .text_color(fg)
            .when(on, |d| d.bg(theme::hsla(t::ELEVATED)))
            .hover(|d| d.bg(theme::hsla(t::ELEVATED)))
            .on_click(cx.listener(move |s, _, _, cx| {
                s.view = v;
                cx.notify();
            }))
            .child(
                svg()
                    .path(format!("icons/{}.svg", v.icon()))
                    .size(px(20.))
                    .text_color(fg),
            )
            .child(div().text_size(px(11.)).child(v.label()))
    }
}

impl Render for Shell {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let mono = cx
            .global::<gpui_component::Theme>()
            .mono_font_family
            .clone();
        let rail = div()
            .flex()
            .flex_col()
            .w(px(RAIL_W))
            .h_full()
            .flex_none()
            .bg(theme::hsla(t::SURFACE))
            .border_r_1()
            .border_color(theme::hsla(t::BORDER))
            // room for the macOS traffic lights
            .pt(px(if cfg!(target_os = "macos") { 40. } else { 12. }))
            .child(self.rail_button(View::Tui, cx))
            .child(self.rail_button(View::Agent, cx))
            .child(div().flex_1())
            .child(self.rail_button(View::Settings, cx));
        let v = self.view;
        let body = div()
            .flex_1()
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap_2()
            .child(
                div()
                    .text_size(px(20.))
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(theme::hsla(t::TEXT_STRONG))
                    .child(v.label()),
            )
            .child(
                div()
                    .font_family(if v == View::Tui {
                        mono
                    } else {
                        brand::UI_FONT.into()
                    })
                    .text_color(theme::hsla(t::MUTED))
                    .child(v.blurb()),
            );
        let content = if v == View::Tui {
            div().flex_1().min_w(px(0.)).child(self.tui.clone())
        } else {
            body
        };
        div()
            .size_full()
            .flex()
            .flex_row()
            .bg(theme::hsla(t::BG))
            .text_size(px(13.))
            .font_family(brand::UI_FONT)
            .child(rail)
            .child(content)
    }
}

/// The bundled fonts: the UI sans and the TUI view's monospace (SIL OFL, `assets/fonts`).
pub fn register_fonts(cx: &App) {
    let fonts: [&'static [u8]; 4] = [
        include_bytes!("../assets/fonts/JetBrainsMono-Regular.ttf"),
        brand::UI_FONT_FILES[0],
        brand::UI_FONT_FILES[1],
        brand::UI_FONT_FILES[2],
    ];
    if let Err(e) = cx
        .text_system()
        .add_fonts(fonts.into_iter().map(Cow::Borrowed).collect())
    {
        eprintln!("synthesis-desktop: cannot register fonts: {e}");
    }
}

/// Opens the main window.
pub fn open(cx: &mut App) {
    let mac = cfg!(target_os = "macos");
    let opts = gpui::WindowOptions {
        titlebar: Some(gpui::TitlebarOptions {
            title: Some("Synthesis".into()),
            // macOS: full-bleed, the traffic lights sit over the rail
            appears_transparent: mac,
            traffic_light_position: mac.then(|| gpui::point(px(16.), px(18.))),
        }),
        window_bounds: Some(gpui::WindowBounds::Windowed(gpui::Bounds::centered(
            None,
            gpui::size(px(1280.), px(800.)),
            cx,
        ))),
        // the design's minimum app size (cli.html `#app`) plus the rail
        window_min_size: Some(gpui::size(px(layout::MIN_W + RAIL_W), px(layout::MIN_H))),
        ..Default::default()
    };
    let opened = cx.open_window(opts, |_, cx| {
        let tui = cx.new(|_| TuiView::new(RAIL_W, layout::Hidden::default()));
        cx.new(|_| Shell {
            view: View::Tui,
            tui,
        })
    });
    if let Err(e) = opened {
        eprintln!("synthesis-desktop: cannot open the window: {e}");
    }
}
