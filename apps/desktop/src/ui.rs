//! The window: a left rail (TUI view, Agent view, Settings) and the content of the active view.
//! Phase 0: the views are placeholders; later phases fill them from `engine` / `synthesis_view`.

use std::borrow::Cow;

use gpui::{
    App, Context, Entity, IntoElement, ParentElement as _, Render, Styled as _, Window, div,
    prelude::*, px, svg,
};

use crate::agent_view::AgentView;
use crate::settings_view::{self, Close, SettingsView};
use crate::tui_view::driver::{LiveCfg, OpenSettings};
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
}

/// Width of the rail; the TUI view takes the rest and lays itself out on that width.
pub const RAIL_W: f32 = 72.;

pub struct Shell {
    view: View,
    /// The view the Settings panel returns to when it closes.
    back: View,
    tui: Entity<TuiView>,
    agent: Entity<AgentView>,
    settings: Entity<SettingsView>,
}

impl Shell {
    /// Switches the view; the views that keep a window-wide key hook are told whether they are shown.
    /// Settings slides over the view it was opened from (`back`).
    fn go(&mut self, v: View, window: &mut Window, cx: &mut Context<Self>) {
        if v == View::Settings && self.view != View::Settings {
            self.back = self.view;
            self.settings.update(cx, |p, cx| p.focus(window, cx));
        }
        self.view = v;
        let tui_on = v == View::Tui || (v == View::Settings && self.back == View::Tui);
        self.tui.update(cx, |t, cx| {
            let was = t.active;
            t.active = tui_on;
            if tui_on && !was {
                t.focus_prompt(window, cx);
            }
        });
        self.agent
            .update(cx, |a, cx| a.set_shown(v == View::Agent, window, cx));
        cx.notify();
    }

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
            .on_click(cx.listener(move |s, _, window, cx| {
                let to = if v == View::Settings && s.view == View::Settings {
                    s.back
                } else {
                    v
                };
                s.go(to, window, cx);
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
        let content = if v == View::Settings {
            // the panel slides over the view it was opened from
            let under = match self.back {
                View::Tui => div().size_full().child(self.tui.clone()),
                View::Agent => div().size_full().child(self.agent.clone()),
                View::Settings => div().size_full(),
            };
            div()
                .relative()
                .flex_1()
                .min_w(px(0.))
                .child(under)
                .child(div().absolute().inset_0().child(self.settings.clone()))
        } else if v == View::Tui {
            div().flex_1().min_w(px(0.)).child(self.tui.clone())
        } else {
            div().flex_1().min_w(px(0.)).child(self.agent.clone())
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
    let fonts: [&'static [u8]; 7] = [
        include_bytes!("../assets/fonts/JetBrainsMono-Regular.ttf"),
        brand::UI_FONT_FILES[0],
        brand::UI_FONT_FILES[1],
        brand::UI_FONT_FILES[2],
        brand::MONO_FONT_FILES[0],
        brand::MONO_FONT_FILES[1],
        brand::MONO_FONT_FILES[2],
    ];
    if let Err(e) = cx
        .text_system()
        .add_fonts(fonts.into_iter().map(Cow::Borrowed).collect())
    {
        eprintln!("synthesis-desktop: cannot register fonts: {e}");
    }
}

/// Opens the main window: the live view when `live` is given, else the sample.
pub fn open(cx: &mut App, live: Option<LiveCfg>) {
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
    // `--sample` reads and writes nothing, so its Settings view has no files to open
    let settings_cfg = match &live {
        Some(c) => Ok(settings_view::model::Cfg {
            env: c.env.clone(),
            home: c.synthesis_home.clone(),
            user_home: c.home.clone().map(Into::into).unwrap_or_default(),
            cwd: c.project.clone(),
            managed: synthesis_settings::managed_settings_path(),
            trust: if c.args.common.trust_project {
                synthesis_config::ProjectTrust::Trusted
            } else {
                synthesis_config::ProjectTrust::Untrusted
            },
        }),
        None => Err("--sample draws sample data and reads no files".to_string()),
    };
    let hidden = live
        .as_ref()
        .map(|c| settings_view::hidden_from(&c.tui))
        .unwrap_or_default();
    let opened = cx.open_window(opts, |window, cx| {
        let agent = cx.new(|cx| match &live {
            // the engine starts when the view is first shown
            Some(cfg) => AgentView::live(window, cx, cfg.clone(), RAIL_W),
            None => AgentView::with_sessions(
                window,
                cx,
                vec![crate::agent_view::sample::live()],
                RAIL_W,
            ),
        });
        let tui = cx.new(|cx| match live {
            Some(cfg) => TuiView::live(window, cx, cfg, hidden, RAIL_W),
            None => TuiView::new(RAIL_W, hidden),
        });
        tui.update(cx, |t, cx| t.start(window, cx));
        let settings = cx.new(|cx| SettingsView::new(window, cx, settings_cfg, RAIL_W));
        cx.new(|cx| {
            cx.subscribe_in(
                &settings,
                window,
                |s: &mut Shell, _, _: &Close, window, cx| s.go(s.back, window, cx),
            )
            .detach();
            cx.subscribe_in(
                &tui,
                window,
                |s: &mut Shell, _, _: &OpenSettings, window, cx| {
                    if s.view != View::Settings {
                        s.go(View::Settings, window, cx);
                    }
                },
            )
            .detach();
            Shell {
                view: View::Tui,
                back: View::Tui,
                tui,
                agent,
                settings,
            }
        })
    });
    if let Err(e) = opened {
        eprintln!("synthesis-desktop: cannot open the window: {e}");
    }
}
