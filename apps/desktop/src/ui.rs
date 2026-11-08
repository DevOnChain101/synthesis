//! The window: a title bar with the view icons (TUI, Agent, Settings) over the content of the active view.
//!
//! The title bar follows the Tauri app's chrome (`synthesis-cli-ui` `#chrome`): a 30px row above the content,
//! the icon group (`.cb` 26x22, radius 6, hover and pressed backgrounds) at the right, the gear after it. macOS
//! keeps the native traffic lights over a transparent title bar (the bar reserves 78px for them); Windows and
//! Linux draw their own minimise / maximise / close buttons (`WindowControlArea`) and the bar is the drag
//! region (client-side decorations). The content below is exactly the TUI view of `cli.html`: its 46px top bar,
//! columns and footer, on the whole window width.

use std::borrow::Cow;

use gpui::{
    AnyView, App, Context, Entity, InteractiveElement as _, IntoElement, ParentElement as _,
    Render, StatefulInteractiveElement as _, Styled as _, Window, WindowControlArea, div,
    prelude::*, px,
};

use crate::settings_view::{self, Applied, Close, Restart, SettingsView};
use crate::tui_view::driver::{LiveCfg, OpenSettings, SessionStarted};
use crate::tui_view::{Side, TuiView, layout};
use crate::widgets::chrome::{icon, tracked, with_alpha};
use crate::{brand, theme};
use synthesis_tui_render::theme as t;

/// Icons the shell draws that are Lucide (checked by a test in `brand`); the title bar's own are the design's.
pub const ICONS: [&str; 1] = ["square-terminal"];

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
            View::Agent => "Agent (coming soon)",
            View::Settings => "Settings",
        }
    }
    fn icon(self) -> &'static str {
        match self {
            View::Tui => "tui",
            View::Agent => "agent",
            View::Settings => "gear",
        }
    }
    /// The shortcut shown in the tooltip.
    fn keys(self) -> &'static str {
        match self {
            View::Tui => "1",
            View::Agent => "2",
            View::Settings => ",",
        }
    }
}

/// Height of the title bar (`--chrome-h`).
pub const TITLEBAR_H: f32 = 30.;
/// Room kept for the macOS traffic lights (`.mac #chrome{padding-left:78px}`).
const TRAFFIC_LIGHTS_W: f32 = 78.;

/// A tooltip: the name and the shortcut.
struct Tip(String);

impl Render for Tip {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
            .px(px(8.))
            .py(px(4.))
            .rounded(px(6.))
            .border_1()
            .border_color(theme::hsla(t::BORDER))
            .bg(theme::hsla(t::ELEVATED))
            .text_size(px(12.))
            .text_color(theme::hsla(t::TEXT_STRONG))
            .child(self.0.clone())
    }
}

pub struct Shell {
    view: View,
    /// The view the Settings panel returns to when it closes.
    back: View,
    tui: Entity<TuiView>,
    settings: Entity<SettingsView>,
    /// A restart was asked from Settings and the new session is not live yet.
    restarting: bool,
}

impl Shell {
    /// Switches the view; the TUI view's window-wide key hook is told whether it is shown. Settings slides
    /// over the view it was opened from (`back`).
    pub(crate) fn go(&mut self, v: View, window: &mut Window, cx: &mut Context<Self>) {
        if self.view == View::Settings && v != View::Settings {
            // whichever way Settings is left, a slider drag never released does not stay on the window
            self.settings
                .update(cx, |p, cx| p.show_persisted(window, cx));
        }
        if v == View::Settings && self.view != View::Settings {
            self.back = self.view;
            let block = self.tui.read(cx).restart_blocker().map(String::from);
            self.settings.update(cx, |p, cx| {
                p.set_restart_block(block, cx);
                p.focus(window, cx)
            });
        }
        self.view = v;
        let tui_on = v == View::Tui || (v == View::Settings && self.back == View::Tui);
        self.tui.update(cx, |t, cx| {
            let was = t.active;
            t.active = tui_on;
            t.shown = v == View::Tui;
            if tui_on && !was {
                t.focus_prompt(window, cx);
            }
        });
        cx.notify();
    }

    /// The Settings popup (the offscreen shots seed it).
    #[cfg(feature = "shot")]
    pub(crate) fn settings(&self) -> &Entity<SettingsView> {
        &self.settings
    }

    /// "Restart session" of the popup: only when nothing would be dropped (no task, approval or queued prompt).
    /// The popup keeps its pending-change badge and stays open until the fresh session has been started.
    fn restart_session(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let block = self.tui.read(cx).restart_blocker().map(String::from);
        let ok = block.is_none();
        self.settings
            .update(cx, |p, cx| p.set_restart_block(block, cx));
        if ok {
            // the badge goes when the new session is live (`SessionStarted`), not before
            self.restarting = self.tui.update(cx, |t, cx| t.new_session(window, cx));
            if self.restarting {
                self.go(self.back, window, cx);
            } else {
                self.settings.update(cx, |p, cx| {
                    p.set_restart_block(Some("the new session could not be started".into()), cx)
                });
            }
        }
    }

    /// `synt.tui` changed in the Settings popup: the window's opacity and blur, and what the TUI view reads, apply at once.
    fn apply_look(
        &mut self,
        tui: synthesis_settings::TuiSettings,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        theme::set_opacity(tui.opacity);
        window.set_background_appearance(theme::background(&tui));
        self.tui.update(cx, |v, cx| v.apply_settings(tui, cx));
        cx.notify();
    }

    /// A button of the icon group: 26x22, radius 6; hover lightens, the open view is pressed.
    fn icon_button(&self, v: View, cx: &mut Context<Self>) -> impl IntoElement {
        let on = self.view == v;
        let fg = if on {
            theme::hsla(t::TEXT_STRONG)
        } else {
            theme::hsla(t::MUTED)
        };
        let tip = format!(
            "{}  ({}{})",
            v.label(),
            if cfg!(target_os = "macos") {
                "\u{2318}"
            } else {
                "Ctrl+"
            },
            v.keys()
        );
        div()
            .id(v.label())
            .w(px(26.))
            .h(px(22.))
            .flex()
            .items_center()
            .justify_center()
            .rounded(px(6.))
            .cursor_pointer()
            .text_color(fg)
            .when(on, |d| d.bg(with_alpha(theme::hsla(t::TEXT_STRONG), 0.16)))
            .hover(|d| {
                d.bg(with_alpha(theme::hsla(t::TEXT_STRONG), 0.10))
                    .text_color(theme::hsla(t::TEXT_STRONG))
            })
            .tooltip(move |_, cx| -> AnyView { cx.new(|_| Tip(tip.clone())).into() })
            .on_click(cx.listener(move |s, _, window, cx| {
                let to = if v == View::Settings && s.view == View::Settings {
                    s.back
                } else {
                    v
                };
                s.go(to, window, cx);
            }))
            .child(icon(v.icon(), 14., fg))
    }

    /// Minimise / maximise / close of the Windows and Linux title bar.
    fn window_button(area: WindowControlArea) -> impl IntoElement {
        let (name, glyph, danger) = match area {
            WindowControlArea::Min => ("win-min", "win-min", false),
            WindowControlArea::Max => ("win-max", "win-max", false),
            _ => ("win-close", "win-close", true),
        };
        div()
            .id(name)
            .w(px(40.))
            .h_full()
            .flex()
            .items_center()
            .justify_center()
            .window_control_area(area)
            .text_color(theme::hsla(t::TEXT))
            .hover(move |d| {
                if danger {
                    d.bg(theme::hsla(t::ERROR)).text_color(theme::hsla(t::BG))
                } else {
                    d.bg(with_alpha(theme::hsla(t::TEXT_STRONG), 0.10))
                }
            })
            .child(icon(glyph, 12., theme::hsla(t::TEXT)))
    }

    fn title_bar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let mac = cfg!(target_os = "macos");
        div()
            .debug_selector(|| "titlebar".to_string())
            .h(px(TITLEBAR_H))
            .w_full()
            .flex_none()
            .flex()
            .items_center()
            .gap(px(6.))
            .pl(px(if mac { TRAFFIC_LIGHTS_W } else { 10. }))
            .pr(px(if mac { 10. } else { 0. }))
            .border_b_1()
            .border_color(with_alpha(theme::hsla(t::TEXT), 0.12))
            // translucent like the surface under it (the window opacity), so no opaque strip
            .bg(theme::fill(t::BG))
            .child(
                // the drag region: the whole bar except the buttons
                div()
                    .h_full()
                    .flex_1()
                    .min_w(px(8.))
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .window_control_area(WindowControlArea::Drag)
                    .id("titlebar-drag")
                    // double-click: zoom (macOS, per the system setting) / maximize and restore
                    .on_click(|ev, window, _| {
                        if ev.click_count() == 2 {
                            if cfg!(target_os = "macos") {
                                window.titlebar_double_click();
                            } else {
                                window.zoom_window();
                            }
                        }
                    })
                    .when(!mac, |d| {
                        d.child(gpui::img(brand::mark()).size(px(16.)).flex_none())
                            .child(tracked("SYNTHESIS", 10., 0.3, theme::hsla(t::TEXT_STRONG)))
                    })
                    // a dialog waits for the person: a red dot and word, blinking unless motion is off
                    .when_some(self.tui.read(cx).attention_colour(), |d, red| {
                        // a click leads back to the TUI view, where the dialog is
                        d.child(
                            div()
                                .id("attention")
                                .flex()
                                .items_center()
                                .gap(px(6.))
                                .cursor_pointer()
                                .on_click(cx.listener(|s, _, window, cx| {
                                    cx.stop_propagation();
                                    s.go(View::Tui, window, cx)
                                }))
                                .child(div().size(px(8.)).flex_none().rounded_full().bg(red))
                                .child(div().text_color(red).child("approval needed")),
                        )
                    }),
            )
            .child(
                div()
                    .flex()
                    .gap(px(1.))
                    .p(px(2.))
                    .mr(px(6.))
                    .rounded(px(8.))
                    .bg(with_alpha(theme::hsla(t::TEXT_STRONG), 0.07))
                    .child(self.icon_button(View::Tui, cx))
                    .child(self.icon_button(View::Agent, cx)),
            )
            .child(self.icon_button(View::Settings, cx))
            .when(!mac, |d| {
                d.child(div().w(px(8.)))
                    .child(Self::window_button(WindowControlArea::Min))
                    .child(Self::window_button(WindowControlArea::Max))
                    .child(Self::window_button(WindowControlArea::Close))
            })
    }

    /// The Agent page: not built yet (a different design is planned).
    fn coming_soon() -> impl IntoElement {
        div()
            .debug_selector(|| "coming-soon".to_string())
            .size_full()
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap(px(10.))
            .child(gpui::img(brand::mark()).size(px(72.)))
            .child(tracked("AGENT", 16., 0.32, theme::hsla(t::TEXT_STRONG)))
            .child(
                div()
                    .px(px(10.))
                    .py(px(2.))
                    .rounded(px(10.))
                    .border_1()
                    .border_color(theme::hsla(t::BORDER))
                    .text_color(theme::hsla(t::TEXT))
                    .child("Coming soon"),
            )
    }
}

impl Render for Shell {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // the title bar's red signal repaints with the view it belongs to
        if self.tui.read(cx).attention() == Some(true) {
            window.request_animation_frame();
        }
        let content = match self.view {
            View::Tui => div().size_full().child(self.tui.clone()),
            View::Agent => div().size_full().child(Self::coming_soon()),
            // the panel slides over the view it was opened from
            View::Settings => {
                let under = match self.back {
                    View::Agent => div().size_full().child(Self::coming_soon()),
                    _ => div().size_full().child(self.tui.clone()),
                };
                div()
                    .size_full()
                    .relative()
                    .child(under)
                    .child(div().absolute().inset_0().child(self.settings.clone()))
            }
        };
        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(theme::fill(t::BG))
            .text_size(px(13.))
            .font_family(brand::UI_FONT)
            .on_key_down(cx.listener(|s, ev: &gpui::KeyDownEvent, window, cx| {
                let k = &ev.keystroke;
                if !k.modifiers.secondary(cx) {
                    return;
                }
                match k.key.as_str() {
                    "," if s.view == View::Settings => s.go(s.back, window, cx),
                    "," => s.go(View::Settings, window, cx),
                    "1" => s.go(View::Tui, window, cx),
                    "2" => s.go(View::Agent, window, cx),
                    // the collapsed side columns of the TUI view (narrow windows): sidebar, inspector
                    "b" | "i" if s.view == View::Tui => {
                        let side = if k.key == "b" {
                            Side::Left
                        } else {
                            Side::Right
                        };
                        s.tui.update(cx, |t, cx| t.toggle_drawer(side, cx));
                    }
                    _ => return,
                }
                cx.stop_propagation();
            }))
            .child(self.title_bar(cx))
            .child(div().flex_1().min_h(px(0.)).child(content))
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
    if let Some(c) = &live {
        theme::set_opacity(c.tui.opacity);
    }
    let opts = gpui::WindowOptions {
        titlebar: Some(gpui::TitlebarOptions {
            title: Some("Synthesis".into()),
            // a custom title bar everywhere: the traffic lights sit in it on macOS (its 30px row, so centred)
            appears_transparent: true,
            traffic_light_position: mac.then(|| gpui::point(px(14.), px(9.))),
        }),
        // Windows and Linux draw their own controls
        window_decorations: (!mac).then_some(gpui::WindowDecorations::Client),
        window_bounds: Some(gpui::WindowBounds::Windowed(gpui::Bounds::centered(
            None,
            gpui::size(px(1280.), px(800.)),
            cx,
        ))),
        window_background: live
            .as_ref()
            .map_or(Default::default(), |c| theme::background(&c.tui)),
        // the design's minimum app size (cli.html `#app`) plus the title bar
        window_min_size: Some(gpui::size(
            px(layout::MIN_W),
            px(layout::MIN_H + TITLEBAR_H),
        )),
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
        cx.new(|cx| Shell::new(window, cx, live, hidden, settings_cfg))
    });
    if let Err(e) = opened {
        eprintln!("synthesis-desktop: cannot open the window: {e}");
    }
}

impl Shell {
    /// The shell over the TUI view and the Settings slide-in. `live` `None` draws the sample.
    pub fn new(
        window: &mut Window,
        cx: &mut Context<Self>,
        live: Option<LiveCfg>,
        hidden: layout::Hidden,
        settings_cfg: Result<settings_view::model::Cfg, String>,
    ) -> Self {
        let tui = cx.new(|cx| match live {
            Some(cfg) => TuiView::live(window, cx, cfg, hidden, 0.),
            None => TuiView::new(0., hidden),
        });
        Self::around(window, cx, tui, settings_cfg)
    }

    /// The shell over a TUI view the caller built (the offscreen shots fill it before it is drawn).
    pub fn around(
        window: &mut Window,
        cx: &mut Context<Self>,
        tui: Entity<TuiView>,
        settings_cfg: Result<settings_view::model::Cfg, String>,
    ) -> Self {
        tui.update(cx, |t, cx| {
            t.set_top_inset(TITLEBAR_H);
            t.start(window, cx)
        });
        let settings = cx.new(|cx| SettingsView::new(window, cx, settings_cfg, 0.));
        cx.subscribe_in(
            &settings,
            window,
            |s: &mut Shell, _, _: &Close, window, cx| s.go(s.back, window, cx),
        )
        .detach();
        cx.subscribe_in(
            &settings,
            window,
            |s: &mut Shell, _, a: &Applied, window, cx| s.apply_look(a.0, window, cx),
        )
        .detach();
        cx.subscribe_in(
            &settings,
            window,
            |s: &mut Shell, _, _: &Restart, window, cx| s.restart_session(window, cx),
        )
        .detach();
        cx.subscribe_in(
            &tui,
            window,
            |s: &mut Shell, _, _: &SessionStarted, _, cx| {
                if std::mem::take(&mut s.restarting) {
                    s.settings.update(cx, |p, cx| p.restarted(cx));
                }
            },
        )
        .detach();
        // a task that ends while the popup is open enables the button
        cx.observe(&tui, |s: &mut Shell, tui, cx| {
            if s.view == View::Settings {
                let block = tui.read(cx).restart_blocker().map(String::from);
                s.settings
                    .update(cx, |p, cx| p.set_restart_block(block, cx));
            }
        })
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
            settings,
            restarting: false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{TestAppContext, size};

    fn open(
        cx: &mut TestAppContext,
        w: f32,
        h: f32,
    ) -> (Entity<Shell>, &mut gpui::VisualTestContext) {
        cx.update(gpui_component::init);
        let (shell, v) = cx.add_window_view(|window, cx| {
            Shell::new(window, cx, None, Default::default(), Err("test".into()))
        });
        v.simulate_resize(size(px(w), px(h)));
        v.run_until_parked();
        (shell, v)
    }

    fn y(v: &mut gpui::VisualTestContext, sel: &'static str) -> Option<(f32, f32)> {
        v.debug_bounds(sel)
            .map(|b| (f32::from(b.origin.y), f32::from(b.size.height)))
    }

    #[gpui::test]
    fn the_title_bar_sits_above_the_design_content(cx: &mut TestAppContext) {
        let (_, v) = open(cx, 1400., 930.);
        // no rail: the TUI view takes the whole width, below the 30px title bar
        assert_eq!(y(v, "titlebar"), Some((0., TITLEBAR_H)));
        assert_eq!(y(v, "header"), Some((TITLEBAR_H, 46.)));
        let f = y(v, "footer").unwrap();
        assert_eq!(f.0 + f.1, 930.);
        let g = v.debug_bounds("col-left").unwrap();
        assert_eq!(f32::from(g.origin.x), 10.);
        // the content under the bar is the 1400x900 layout with the owner's column rule (`layout::cols`)
        let r = v.debug_bounds("col-right").unwrap();
        let rw = layout::cols(1400.).right;
        assert_eq!(f32::from(r.size.width), rw);
        assert_eq!(f32::from(r.origin.x), 1400. - 10. - rw);
    }

    #[gpui::test]
    fn the_icons_switch_views_and_agent_is_coming_soon(cx: &mut TestAppContext) {
        let (shell, v) = open(cx, 1400., 930.);
        shell.update(v, |s, cx| {
            assert_eq!(s.view, View::Tui);
            let _ = cx;
        });
        shell.update_in(v, |s, window, cx| s.go(View::Agent, window, cx));
        v.run_until_parked();
        assert!(
            v.debug_bounds("header").is_none(),
            "the TUI view is not drawn on the Agent page"
        );
        assert!(v.debug_bounds("titlebar").is_some(), "the title bar stays");
        shell.update_in(v, |s, window, cx| s.go(View::Tui, window, cx));
        v.run_until_parked();
        assert!(v.debug_bounds("header").is_some());
        // Settings slides over the view it was opened from, and the same button closes it
        shell.update_in(v, |s, window, cx| s.go(View::Settings, window, cx));
        assert_eq!(
            shell.read_with(v, |s, _| (s.view, s.back)),
            (View::Settings, View::Tui)
        );
    }

    #[gpui::test]
    fn settings_opacity_reaches_the_title_bar_and_the_tui_view(cx: &mut TestAppContext) {
        let (shell, v) = open(cx, 1400., 930.);
        let tui = synthesis_settings::TuiSettings {
            opacity: 60,
            blur: true,
            panels: [true, true, false, true, true, true, true, true],
            ..Default::default()
        };
        shell.update_in(v, |s, window, cx| s.apply_look(tui, window, cx));
        v.run_until_parked();
        // the title bar and the surfaces paint `theme::fill`; text colours stay opaque
        assert_eq!(theme::fill(t::BG).a, 0.6);
        assert_eq!(theme::hsla(t::TEXT_STRONG).a, 1.);
        // the TUI view hid the Tools panel at once
        assert!(shell.read_with(v, |s, cx| s.tui.read(cx).hidden_panels().tools));
        shell.update_in(v, |s, window, cx| {
            s.apply_look(Default::default(), window, cx)
        });
        assert_eq!(theme::fill(t::BG).a, 1.);
    }

    #[gpui::test]
    fn restart_session_is_refused_while_it_would_drop_work_and_keeps_the_badge(
        cx: &mut TestAppContext,
    ) {
        cx.update(|cx| {
            gpui_component::init(cx);
            theme::apply("JetBrains Mono", cx);
        });
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let (home, ws) = (root.join("home"), root.join("ws"));
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(&ws).unwrap();
        let cfg = settings_view::model::Cfg {
            env: synthesis_config::Env::from_pairs([
                ("HOME", home.to_string_lossy().into_owned()),
                ("SYNT_HOME", home.to_string_lossy().into_owned()),
            ]),
            user_home: home.clone(),
            home,
            cwd: ws,
            managed: root.join("managed.json"),
            trust: synthesis_config::ProjectTrust::Untrusted,
        };
        let (shell, v) = cx.add_window_view(|window, cx| {
            Shell::new(window, cx, None, Default::default(), Ok(cfg))
        });
        v.simulate_resize(size(px(1000.), px(800.)));
        shell.update_in(v, |s, window, cx| {
            s.go(View::Settings, window, cx);
            let st = s.settings.clone();
            st.update(cx, |p, cx| {
                p.apply(
                    "synt.memory.enabled",
                    settings_view::model::Change::Bool(false),
                    window,
                    cx,
                );
                cx.emit(Restart);
            });
        });
        v.run_until_parked();
        // no live session here: nothing to restart, so the popup stays, says why, and the change is still pending
        assert_eq!(shell.read_with(v, |s, _| s.view), View::Settings);
        assert!(v.debug_bounds("restart-blocked").is_some());
        assert!(shell.read_with(v, |s, cx| {
            s.settings.read(cx).model().unwrap().restart_pending() == 1
        }));
    }

    #[gpui::test]
    fn leaving_settings_by_any_route_undoes_an_unreleased_opacity_drag(cx: &mut TestAppContext) {
        let (shell, v) = open(cx, 1000., 800.);
        for leave in [View::Tui, View::Agent] {
            theme::set_opacity(100);
            shell.update_in(v, |s, window, cx| s.go(View::Settings, window, cx));
            // a drag in progress (the preview reaches the shell as `Applied`), never released
            shell.update_in(v, |s, window, cx| {
                s.apply_look(
                    synthesis_settings::TuiSettings {
                        opacity: 40,
                        ..Default::default()
                    },
                    window,
                    cx,
                )
            });
            assert_eq!(theme::fill(t::BG).a, 0.4);
            // the title-bar icons and Cmd+1/Cmd+2 all end in `go`
            shell.update_in(v, |s, window, cx| s.go(leave, window, cx));
            v.run_until_parked();
            assert_eq!(
                theme::fill(t::BG).a,
                1.,
                "{leave:?}: back to the file's value"
            );
        }
    }
}
