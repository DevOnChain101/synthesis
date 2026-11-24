//! The window: a title bar with the view icons (TUI, Super Agent, Settings) over the content of the active view.
//!
//! The title bar follows the design's chrome (`index.html` `#chrome`): a 30px row above the content,
//! the icon group (`.cb` 26x22, radius 6, hover and pressed backgrounds) at the right, the gear after it. In the
//! Super Agent view this bar is not drawn: `superagent::SuperAgentView` owns the whole window (its rail's first row
//! and its title row carry the lights, tabs, mode buttons and gear, spec 5.1). macOS
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

use crate::outcomes_view::OutcomesView;
use crate::outcomes_view::popup::OpenSession;
use crate::settings_view::{self, Applied, Close, Restart, SettingsView};
use crate::superagent::material;
use crate::superagent::session::host::SessionHost;
use crate::superagent::{SuperAgentView, SwitchView};
use crate::tui_view::driver::{LiveCfg, OpenSettings, SessionStarted};
use crate::tui_view::{Side, TuiView, layout};
use crate::widgets::chrome::{self, icon, tracked, with_alpha};
use crate::{brand, theme};
use synthesis_cli_ui_render::theme as t;

/// Icons the shell draws that are Lucide (checked by a test in `brand`); the title bar's own are the design's.
pub const ICONS: [&str; 2] = ["square-terminal", "undo-2"];

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum View {
    Tui,
    /// The Super Agent view (`outcomes_view`: the things it shows are called outcomes).
    SuperAgent,
    Settings,
}

impl View {
    fn label(self) -> &'static str {
        match self {
            View::Tui => "TUI",
            View::SuperAgent => "Super Agent",
            View::Settings => "Settings",
        }
    }
    fn icon(self) -> &'static str {
        match self {
            View::Tui => "tui",
            View::SuperAgent => "super-agent",
            View::Settings => "gear",
        }
    }
    fn slug(self) -> &'static str {
        match self {
            View::Tui => "tui",
            View::SuperAgent => "super-agent",
            View::Settings => "settings",
        }
    }
    /// The shortcut shown in the tooltip.
    fn keys(self) -> &'static str {
        match self {
            View::Tui => "1",
            View::SuperAgent => "2",
            View::Settings => ",",
        }
    }
}

/// Height of the title bar (`--chrome-h`).
pub const TITLEBAR_H: f32 = 30.;
/// Room kept for the macOS traffic lights (`.mac #chrome{padding-left:78px}`).
const TRAFFIC_LIGHTS_W: f32 = 78.;

/// A tooltip: the name and the shortcut.
pub(crate) struct Tip(pub String);

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
    outcomes: Entity<OutcomesView>,
    /// What `View::SuperAgent` shows; `outcomes` stays its data and link owner.
    sa: Entity<SuperAgentView>,
    /// Tests of the old outcomes UI (deleted in phase E) mount it in place of `sa`.
    legacy_ui: bool,
    /// A restart was asked from Settings and the new session is not live yet.
    restarting: bool,
    /// The Super Agent button counts what waits for the person: only a live app (never the sample), and the count is
    /// 0 while the orchestrator is off, unsupported or not connected.
    count_waiting: bool,
    /// The timer of the next frame of the title bar's blinking red (see `chrome::next_frame`).
    anim_tick: Option<gpui::Task<()>>,
}

impl Shell {
    /// The app menu's Settings item: opens Settings, or closes it back to the view it came from.
    pub(crate) fn toggle_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let to = if self.view == View::Settings {
            self.back
        } else {
            View::Settings
        };
        self.go(to, window, cx);
    }

    /// The app menu's Quit item: the TUI view's own quit (engine shutdown, then the app exits).
    pub(crate) fn quit(&mut self, cx: &mut Context<Self>) {
        self.tui.update(cx, |t, cx| t.quit(cx));
    }

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
        let from_sa = self.view == View::SuperAgent
            || (self.view == View::Settings && self.back == View::SuperAgent);
        let handoff =
            (v == View::Tui && from_sa && !self.legacy_ui).then(|| self.sa.read(cx).handoff(cx));
        self.view = v;
        // the card's keys never outlive the view: a hidden view must not take the window's focus back later
        let sa = v == View::SuperAgent || (v == View::Settings && self.back == View::SuperAgent);
        self.outcomes.update(cx, |o, _| o.set_visible(sa));
        if v == View::SuperAgent {
            let (known, remote) = self.tui.read(cx).known_folders();
            self.outcomes.update(cx, |o, cx| {
                o.state.known = known;
                o.state.remote = remote;
                o.shown(window, cx)
            });
            if !self.legacy_ui {
                self.sa.update(cx, |v, cx| v.shown(window, cx));
            }
        }
        // the hosted chats take keys only while the Super Agent view itself is on screen (Settings over it: none)
        let on_screen = v == View::SuperAgent && !self.legacy_ui;
        self.sa.update(cx, |s, cx| s.set_visible(on_screen, cx));
        // the lights (macOS only) sit in the first row of the Super Agent rail, else in the 30 px bar
        #[cfg(target_os = "macos")]
        window.set_traffic_light_position(gpui::point(
            px(14.),
            px(crate::superagent::traffic_light_y(v == View::SuperAgent)),
        ));
        let tui_on = v == View::Tui || (v == View::Settings && self.back == View::Tui);
        self.tui.update(cx, |t, cx| {
            let was = t.active;
            t.active = tui_on;
            t.shown = v == View::Tui;
            if tui_on && !was {
                t.focus_prompt(window, cx);
            }
        });
        if let Some(h) = handoff {
            self.tui.update(cx, |t, cx| t.adopt_handoff(h, cx));
        }
        cx.notify();
    }

    /// A session that waits for a tool permission is answered in the TUI view, in that session.
    fn open_waiting_session(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        self.go(View::Tui, window, cx);
        if !id.starts_with('#') {
            self.tui.update(cx, |t, cx| t.open_session(id, cx));
        }
    }

    /// The Super Agent view (the offscreen shots seed it).
    #[cfg(any(test, feature = "shot"))]
    pub(crate) fn outcomes(&self) -> &Entity<OutcomesView> {
        &self.outcomes
    }

    /// Shows the old outcomes UI instead of the Super Agent view (its tests, until phase E).
    #[cfg(test)]
    pub(crate) fn mount_legacy_ui(&mut self) {
        self.legacy_ui = true;
    }

    /// The badge counts the sample's waiting things too (shots and tests of the badge).
    #[cfg(any(test, feature = "shot"))]
    pub(crate) fn count_waiting_in_sample(&mut self, cx: &mut Context<Self>) {
        self.count_waiting = true;
        self.sa.update(cx, |v, _| v.count_waiting = true);
        cx.notify();
    }

    #[cfg(feature = "shot")]
    pub(crate) fn superagent(&self) -> &Entity<SuperAgentView> {
        &self.sa
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
        tui: synthesis_settings::UiSettings,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        theme::set_opacity(tui.opacity);
        window.set_background_appearance(material::background(&tui, material::current()));
        self.tui.update(cx, |v, cx| v.apply_settings(tui, cx));
        cx.notify();
    }

    /// What waits for the person (the number on the Super Agent button): 0 unless the app counts it.
    fn waiting(&self, cx: &App) -> usize {
        self.sa.read(cx).waiting(cx)
    }

    /// A button of the icon group: 26x22, radius 6; hover lightens, the open view is pressed. The Super Agent
    /// button carries the badge (what waits for the person) when there is one.
    fn icon_button(&self, v: View, cx: &mut Context<Self>) -> impl IntoElement {
        let on = self.view == v;
        let fg = if on {
            theme::hsla(t::TEXT_STRONG)
        } else {
            theme::hsla(t::MUTED)
        };
        let waiting = if v == View::SuperAgent {
            self.waiting(cx)
        } else {
            0
        };
        let tip = format!(
            "{}  ({}{}){}",
            v.label(),
            if cfg!(target_os = "macos") {
                "\u{2318}"
            } else {
                "Ctrl+"
            },
            v.keys(),
            if waiting > 0 {
                format!("  \u{b7}  {waiting} waiting for you")
            } else {
                String::new()
            }
        );
        div()
            .id(v.label())
            .debug_selector(move || format!("mode-{}", v.slug()))
            .relative()
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
            .when(waiting > 0, |d| {
                // a click on the badge opens Super Agent with the oldest waiting thing in front
                d.child(
                    div()
                        .id("waiting-badge")
                        .debug_selector(|| "waiting-badge".to_string())
                        .absolute()
                        .top(px(-5.))
                        .right(px(-5.))
                        .min_w(px(14.))
                        .h(px(14.))
                        .px(px(3.))
                        .flex()
                        .items_center()
                        .justify_center()
                        .rounded_full()
                        .bg(theme::hsla(t::WARN))
                        .text_size(px(9.))
                        .line_height(px(14.))
                        .font_weight(gpui::FontWeight::BOLD)
                        .text_color(theme::hsla(t::BG))
                        .on_click(cx.listener(|s, _, window, cx| {
                            cx.stop_propagation();
                            s.go(View::SuperAgent, window, cx);
                            s.outcomes.update(cx, |o, cx| o.next_approval(window, cx));
                        }))
                        .child(if waiting > 99 {
                            "99+".to_string()
                        } else {
                            waiting.to_string()
                        }),
                )
            })
    }

    /// Minimise / maximise / close of the Windows and Linux title bar.
    pub(crate) fn window_button(area: WindowControlArea) -> impl IntoElement {
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

    fn sa_or_legacy(&self) -> gpui::AnyView {
        if self.legacy_ui {
            self.outcomes.clone().into()
        } else {
            self.sa.clone().into()
        }
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
                        d.child(brand::mark_img(16.).size(px(16.)).flex_none())
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
                                // only the dot blinks: the words stay at full contrast, and one fixed line box
                                // inside the strip keeps them whole
                                .child(div().size(px(8.)).flex_none().rounded_full().bg(red))
                                .child(
                                    div()
                                        .flex_none()
                                        .whitespace_nowrap()
                                        .text_size(px(12.))
                                        .line_height(px(16.))
                                        .text_color(theme::hsla(t::ERROR))
                                        .child("approval needed"),
                                ),
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
                    .child(self.icon_button(View::SuperAgent, cx)),
            )
            .child(self.icon_button(View::Settings, cx))
            .when(!mac, |d| {
                d.child(div().w(px(8.)))
                    .child(Self::window_button(WindowControlArea::Min))
                    .child(Self::window_button(WindowControlArea::Max))
                    .child(Self::window_button(WindowControlArea::Close))
            })
    }
}

impl Render for Shell {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // the title bar's red signal repaints with the view it belongs to
        if self.tui.read(cx).attention() == Some(true) {
            chrome::next_frame(|t| &mut t.anim_tick, self, cx);
        }
        let content = match self.view {
            View::Tui => div().size_full().child(self.tui.clone()),
            View::SuperAgent => div().size_full().child(self.sa_or_legacy()),
            // the panel slides over the view it was opened from
            View::Settings => {
                let under = match self.back {
                    View::SuperAgent => div().size_full().child(self.sa_or_legacy()),
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
            .bg(theme::ground())
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
                    "2" => s.go(View::SuperAgent, window, cx),
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
            // the Super Agent view draws its own title row over the whole window
            .children((self.view != View::SuperAgent || self.legacy_ui).then(|| self.title_bar(cx)))
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
        material::set(material::detect(
            material::read_choice(&c.synthesis_home).as_deref(),
        ));
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
        window_background: live.as_ref().map_or(Default::default(), |c| {
            material::background(&c.tui, material::current())
        }),
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
            // what is effective: a remembered trust (inert) is shown as untrusted, like the engine loads it
            trust: if c.args.common.trust_project && !c.args.common.project_exec_inert {
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
        let env = live
            .as_ref()
            .map(|c| (c.env.clone(), c.project.display().to_string()));
        let tui = cx.new(|cx| match live {
            Some(cfg) => TuiView::live(window, cx, cfg, hidden, 0.),
            None => TuiView::new(0., hidden),
        });
        let host_cfg = tui.read(cx).live_cfg();
        Self::with_outcomes(window, cx, tui, settings_cfg, env, host_cfg)
    }

    /// The shell over a TUI view the caller built (the offscreen shots fill it before it is drawn); the
    /// Super Agent view shows its sample.
    pub fn around(
        window: &mut Window,
        cx: &mut Context<Self>,
        tui: Entity<TuiView>,
        settings_cfg: Result<settings_view::model::Cfg, String>,
    ) -> Self {
        Self::with_outcomes(window, cx, tui, settings_cfg, None, None)
    }

    /// A sample TUI view whose Super Agent hosts live sessions on `cfg` (tests: the standalone engine of a live TUI
    /// view would wake the test scheduler from its thread).
    #[cfg(test)]
    pub(crate) fn around_live(window: &mut Window, cx: &mut Context<Self>, cfg: LiveCfg) -> Self {
        let tui = cx.new(|_| TuiView::new(0., Default::default()));
        Self::with_outcomes(window, cx, tui, Err("test".into()), None, Some(cfg))
    }

    /// `outcomes_env`: the environment the live Super Agent view reads its setting and home from, and the workspace a new outcome works in by default; `None` is the sample.
    fn with_outcomes(
        window: &mut Window,
        cx: &mut Context<Self>,
        tui: Entity<TuiView>,
        settings_cfg: Result<settings_view::model::Cfg, String>,
        outcomes_env: Option<(synthesis_config::Env, String)>,
        host_cfg: Option<LiveCfg>,
    ) -> Self {
        let outcomes = cx.new(|cx| match outcomes_env.clone() {
            Some((env, ws)) => OutcomesView::live(env, ws, cx),
            None => OutcomesView::sample(cx),
        });
        // the badge needs the list without the view being opened: only a live app whose gate says yes connects at
        // launch (checked once here); otherwise no thread, no poll and no badge exist. A later explicit retry in the
        // Super Agent view connects it on its own.
        let live_outcomes = outcomes_env.is_some();
        if outcomes_env
            .as_ref()
            .is_some_and(|(env, _)| crate::approvals::gate(env).is_ok())
        {
            outcomes.update(cx, |o, cx| o.connect(cx));
        }
        let host = cx.new(|cx| SessionHost::new(host_cfg, window, cx));
        let conductor = outcomes_env
            .as_ref()
            .map(|(env, ws)| (env.clone(), std::path::PathBuf::from(ws)));
        let sa = cx.new(|cx| {
            SuperAgentView::new(outcomes.clone(), host, conductor, live_outcomes, window, cx)
        });
        cx.subscribe_in(
            &sa,
            window,
            |s: &mut Shell, _, e: &SwitchView, window, cx| {
                // the gear closes Settings when it is already open (like the title bar's button)
                let to = if e.0 == View::Settings && s.view == View::Settings {
                    s.back
                } else {
                    e.0
                };
                s.go(to, window, cx);
            },
        )
        .detach();
        // the title bar's badge follows the list and what the sessions wait for
        cx.observe(&outcomes, |_, _, cx| cx.notify()).detach();
        cx.subscribe_in(
            &outcomes,
            window,
            |s: &mut Shell, _, e: &OpenSession, window, cx| {
                s.open_waiting_session(&e.0, window, cx)
            },
        )
        .detach();
        cx.subscribe_in(
            &sa,
            window,
            |s: &mut Shell, _, e: &OpenSession, window, cx| {
                s.open_waiting_session(&e.0, window, cx)
            },
        )
        .detach();
        cx.subscribe_in(
            &outcomes,
            window,
            |s: &mut Shell, _, e: &crate::outcomes_view::OpenFolder, window, cx| {
                // a plain New session opens as a tab of Super Agent, never in the TUI view; a refusal is the view's notice
                s.sa.update(cx, |v, cx| {
                    let _ = v.open_session_folder(&e.0, window, cx);
                });
            },
        )
        .detach();
        cx.observe_window_activation(window, |s: &mut Shell, window, cx| {
            s.outcomes
                .update(cx, |o, cx| o.window_activation_changed(window, cx))
        })
        .detach();
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
        // sessions waiting for a tool permission count for the badge and Alt+A of the Super Agent view
        cx.observe(&tui, |s: &mut Shell, tui, cx| {
            if s.count_waiting {
                let tools = tui.read(cx).waiting_tools();
                s.outcomes
                    .update(cx, |o, cx| o.set_waiting_tools(tools, cx));
            }
        })
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
            outcomes,
            sa,
            legacy_ui: false,
            restarting: false,
            count_waiting: live_outcomes,
            anim_tick: None,
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
        let _g = theme::opacity_guard();
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
    fn the_icons_switch_views_and_outcomes_is_a_view_of_its_own(cx: &mut TestAppContext) {
        let _g = theme::opacity_guard();
        let (shell, v) = open(cx, 1400., 930.);
        shell.update(v, |s, cx| {
            assert_eq!(s.view, View::Tui);
            let _ = cx;
        });
        shell.update_in(v, |s, window, cx| s.go(View::SuperAgent, window, cx));
        v.run_until_parked();
        assert!(
            v.debug_bounds("header").is_none(),
            "the TUI view is not drawn on the Super Agent page"
        );
        assert!(
            v.debug_bounds("superagent").is_some(),
            "the Super Agent view is"
        );
        assert!(
            v.debug_bounds("sa-title-row").is_some() && v.debug_bounds("titlebar").is_none(),
            "the Super Agent view draws its own title row"
        );
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
        let _g = theme::opacity_guard();
        let (shell, v) = open(cx, 1400., 930.);
        let tui = synthesis_settings::UiSettings {
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
        let _g = theme::opacity_guard();
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
        let _g = theme::opacity_guard();
        let (shell, v) = open(cx, 1000., 800.);
        for leave in [View::Tui, View::SuperAgent] {
            theme::set_opacity(100);
            shell.update_in(v, |s, window, cx| s.go(View::Settings, window, cx));
            // a drag in progress (the preview reaches the shell as `Applied`), never released
            shell.update_in(v, |s, window, cx| {
                s.apply_look(
                    synthesis_settings::UiSettings {
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

    fn b(v: &mut gpui::VisualTestContext, sel: &'static str) -> (f32, f32, f32, f32) {
        let r = v.debug_bounds(sel).unwrap_or_else(|| panic!("no {sel}"));
        (
            f32::from(r.origin.x),
            f32::from(r.origin.y),
            f32::from(r.size.width),
            f32::from(r.size.height),
        )
    }

    const MODES: [&str; 3] = ["mode-tui", "mode-super-agent", "mode-settings"];
    const SIZES: [(f32, f32); 2] = [(1400., 900.), (1100., 700.)];

    #[gpui::test]
    fn super_agent_draws_its_own_chrome_and_the_tui_title_bar_does_not_change(
        cx: &mut TestAppContext,
    ) {
        let _g = theme::opacity_guard();
        for (w, h) in SIZES {
            let (shell, v) = open(cx, w, h);
            // the TUI view: the 30 px bar with the mode buttons and the gear, none of the Super Agent chrome
            assert!(v.debug_bounds("sa-rail").is_none() && v.debug_bounds("breadcrumb").is_none());
            assert!(MODES.iter().all(|m| v.debug_bounds(m).is_some()));
            shell.update_in(v, |s, window, cx| s.go(View::SuperAgent, window, cx));
            v.run_until_parked();
            // Super Agent: no shell title bar; the rail's first row holds back, forward and the toggle, the tabs start
            // where the rail ends, and the mode buttons and the gear are at the right of the 48 px title row
            assert!(v.debug_bounds("titlebar").is_none() && v.debug_bounds("breadcrumb").is_none());
            let rail = b(v, "sa-rail");
            let rw = if w < 900. { 72. } else { 248. };
            assert_eq!(
                (rail.0, rail.1, rail.2, rail.3),
                (0., 0., rw, h),
                "{w}x{h}: a full-height rail"
            );
            let (back, fwd, tog) = (b(v, "sa-back"), b(v, "sa-forward"), b(v, "sa-rail-toggle"));
            assert!(
                back.1 < crate::superagent::TITLE_ROW_H && fwd.1 < crate::superagent::TITLE_ROW_H
            );
            assert!(
                back.0 < fwd.0 && fwd.0 < tog.0,
                "{w}x{h}: back, forward, toggle"
            );
            let tab = b(v, "sa-tab-0");
            assert_eq!(
                tab.0, rail.2,
                "{w}x{h}: tabs start at the rail's right edge"
            );
            let (tui, sa, gear) = (
                b(v, "sa-mode-tui"),
                b(v, "sa-mode-super-agent"),
                b(v, "sa-gear"),
            );
            assert!(tui.0 < sa.0 && sa.0 < gear.0, "TUI | Super Agent | gear");
            // back to the TUI view: its title bar is the same as before
            shell.update_in(v, |s, window, cx| s.go(View::Tui, window, cx));
            v.run_until_parked();
            assert!(v.debug_bounds("sa-rail").is_none() && v.debug_bounds("titlebar").is_some());
        }
    }

    #[gpui::test]
    fn the_badge_counts_what_waits_and_is_absent_when_nothing_is_counted(cx: &mut TestAppContext) {
        let _g = theme::opacity_guard();
        let (shell, v) = open(cx, 1400., 900.);
        // the sample does not count (only a live app does): no badge, in either view
        assert!(v.debug_bounds("waiting-badge").is_none());
        assert_eq!(shell.read_with(v, |s, cx| s.waiting(cx)), 0);
        let before = b(v, "mode-super-agent");
        shell.update(v, |s, cx| s.count_waiting_in_sample(cx));
        v.run_until_parked();
        // the sample has one decision waiting; the badge hangs on the Super Agent button without moving anything
        assert_eq!(shell.read_with(v, |s, cx| s.waiting(cx)), 1);
        assert!(v.debug_bounds("waiting-badge").is_some());
        assert_eq!(b(v, "mode-super-agent"), before, "the badge is an overlay");
        // a session waiting for a tool permission counts too
        let o = shell.read_with(v, |s, _| s.outcomes().clone());
        o.update(v, |o, cx| {
            o.set_waiting_tools(vec![("s1".into(), "x".into())], cx)
        });
        v.run_until_parked();
        assert_eq!(shell.read_with(v, |s, cx| s.waiting(cx)), 2);
        // the link not ready (off, windows, unreachable): the badge goes
        o.update(v, |o, cx| {
            o.state.screen = crate::outcomes_view::model::Screen::Off;
            cx.notify();
        });
        v.run_until_parked();
        assert!(v.debug_bounds("waiting-badge").is_none());
    }

    #[gpui::test]
    fn a_click_on_the_mode_button_or_on_the_badge_opens_super_agent(cx: &mut TestAppContext) {
        let _g = theme::opacity_guard();
        let (shell, v) = open(cx, 1400., 900.);
        let c = v.debug_bounds("mode-super-agent").unwrap().center();
        v.simulate_click(c, Default::default());
        assert_eq!(shell.read_with(v, |s, _| s.view), View::SuperAgent);
        // the TUI button goes back; with a badge up, a click on the badge opens Super Agent with its card in front
        let c = v.debug_bounds("sa-mode-tui").unwrap().center();
        v.simulate_click(c, Default::default());
        assert_eq!(shell.read_with(v, |s, _| s.view), View::Tui);
        shell.update(v, |s, cx| s.count_waiting_in_sample(cx));
        v.run_until_parked();
        let badge = v.debug_bounds("waiting-badge").unwrap().center();
        v.simulate_click(badge, Default::default());
        v.run_until_parked();
        assert_eq!(shell.read_with(v, |s, _| s.view), View::SuperAgent);
    }

    #[gpui::test]
    fn the_rail_buttons_walk_the_frame_history_and_collapse_the_rail(cx: &mut TestAppContext) {
        let _g = theme::opacity_guard();
        let (shell, v) = open(cx, 1400., 900.);
        shell.update_in(v, |s, window, cx| s.go(View::SuperAgent, window, cx));
        v.run_until_parked();
        let sa = shell.read_with(v, |s, _| s.sa.clone());
        let active =
            |v: &mut gpui::VisualTestContext| sa.read_with(v, |s, _| s.frames.active().clone());
        let click = |v: &mut gpui::VisualTestContext, sel: &'static str| {
            let c = v.debug_bounds(sel).unwrap().center();
            v.simulate_click(c, Default::default());
            v.run_until_parked();
        };
        assert!(
            !sa.read_with(v, |s, cx| s.can_nav(true, cx)),
            "nowhere to go yet"
        );
        click(v, "sa-back");
        assert_eq!(
            active(v),
            crate::superagent::frames::Frame::Home,
            "a disabled button does nothing"
        );
        // two outcomes from the rail
        let ids = shell.read_with(v, |s, cx| {
            s.outcomes
                .read(cx)
                .state
                .items
                .iter()
                .map(|i| i.row.id.clone())
                .collect::<Vec<_>>()
        });
        for (i, id) in ids.iter().take(2).enumerate() {
            let _ = id;
            click(v, ["sa-rail-2", "sa-rail-3"][i]);
        }
        let last = active(v);
        assert_eq!(
            last,
            crate::superagent::frames::Frame::Outcome(ids[1].clone())
        );
        click(v, "sa-back");
        assert_eq!(
            active(v),
            crate::superagent::frames::Frame::Outcome(ids[0].clone())
        );
        click(v, "sa-forward");
        assert_eq!(active(v), last, "forward");
        // the toggle collapses the rail to icons and expands it again
        assert_eq!(b(v, "sa-rail").2, 248.);
        click(v, "sa-rail-toggle");
        assert_eq!(b(v, "sa-rail").2, 72.);
        click(v, "sa-rail-toggle");
        assert_eq!(b(v, "sa-rail").2, 248.);
    }

    #[gpui::test]
    fn settings_over_super_agent_shows_the_shell_bar_and_the_gear_toggles_it(
        cx: &mut TestAppContext,
    ) {
        let _g = theme::opacity_guard();
        let (shell, v) = open(cx, 1100., 700.);
        shell.update_in(v, |s, window, cx| s.go(View::SuperAgent, window, cx));
        v.run_until_parked();
        // the gear of the Super Agent title row opens Settings over it
        let c = v.debug_bounds("sa-gear").unwrap().center();
        v.simulate_click(c, Default::default());
        v.run_until_parked();
        assert_eq!(
            shell.read_with(v, |s, _| (s.view, s.back)),
            (View::Settings, View::SuperAgent)
        );
        assert!(
            v.debug_bounds("titlebar").is_some(),
            "the shell bar is back while Settings is open"
        );
        let c = v.debug_bounds("mode-settings").unwrap().center();
        v.simulate_click(c, Default::default());
        v.run_until_parked();
        assert_eq!(shell.read_with(v, |s, _| s.view), View::SuperAgent);
        assert!(v.debug_bounds("titlebar").is_none());
    }

    #[gpui::test]
    fn the_sample_hosts_no_session_and_says_why(cx: &mut TestAppContext) {
        let _g = theme::opacity_guard();
        let (shell, v) = open(cx, 1400., 900.);
        shell.update_in(v, |s, window, cx| s.go(View::SuperAgent, window, cx));
        v.run_until_parked();
        let o = shell.read_with(v, |s, _| s.outcomes().clone());
        let dir = tempfile::tempdir().unwrap();
        o.update(v, |_, cx| {
            cx.emit(crate::outcomes_view::OpenFolder(dir.path().to_path_buf()))
        });
        v.run_until_parked();
        assert_eq!(shell.read_with(v, |s, _| s.view), View::SuperAgent);
        let sa = shell.read_with(v, |s, _| s.sa.clone());
        assert_eq!(sa.read_with(v, |s, _| s.frames.tabs().len()), 1, "no tab");
        assert!(v.debug_bounds("sa-notice").is_some(), "the reason is shown");
    }

    /// A live app over a scripted model: `(tui, hosted)` flags of who takes keys.
    fn live(
        cx: &mut TestAppContext,
        cfg: LiveCfg,
    ) -> (Entity<Shell>, &mut gpui::VisualTestContext) {
        cx.update(gpui_component::init);
        let (shell, v) = cx.add_window_view(|window, cx| Shell::around_live(window, cx, cfg));
        v.simulate_resize(size(px(1400.), px(900.)));
        v.run_until_parked();
        (shell, v)
    }

    fn keys(
        shell: &Entity<Shell>,
        v: &mut gpui::VisualTestContext,
    ) -> ((bool, bool), Vec<(bool, bool)>) {
        shell.read_with(v, |s, cx| {
            let t = s.tui.read(cx);
            let host = s.sa.read(cx).host.read(cx);
            (
                (t.shown, t.active),
                host.views()
                    .iter()
                    .map(|h| (h.read(cx).shown, h.read(cx).active))
                    .collect(),
            )
        })
    }

    #[gpui::test]
    fn a_new_session_is_a_real_chat_inside_super_agent_and_two_views_never_both_take_keys(
        cx: &mut TestAppContext,
    ) {
        use crate::superagent::frames::Frame;
        use crate::superagent::session::host::tests::fx;
        let _g = theme::opacity_guard();
        let (f, cfg) = fx(2);
        cx.executor().allow_parking();
        let (shell, v) = live(cx, cfg);
        let sa = shell.read_with(v, |s, _| s.sa.clone());
        shell.update_in(v, |s, window, cx| s.go(View::Tui, window, cx));
        assert_eq!(keys(&shell, v).0, (true, true), "the TUI view has the keys");
        shell.update_in(v, |s, window, cx| s.go(View::SuperAgent, window, cx));
        v.run_until_parked();
        assert_eq!(
            keys(&shell, v).0,
            (false, false),
            "not while Super Agent shows"
        );
        let folder = f.base.join("f1");
        let o = shell.read_with(v, |s, _| s.outcomes().clone());
        o.update(v, |_, cx| {
            cx.emit(crate::outcomes_view::OpenFolder(folder.clone()))
        });
        v.run_until_parked();
        // the person stays in Super Agent, in a session frame with a real embedded chat
        let id = "s1".to_string();
        assert_eq!(shell.read_with(v, |s, _| s.view), View::SuperAgent);
        assert_eq!(
            sa.read_with(v, |s, _| s.frames.active().clone()),
            Frame::Session(id)
        );
        assert_eq!(sa.read_with(v, |s, cx| s.host.read(cx).len()), 1);
        assert!(v.debug_bounds("sa-session").is_some() && v.debug_bounds("view").is_some());
        assert_eq!(
            keys(&shell, v),
            ((false, false), vec![(true, false)]),
            "only the front frame (window inactive in tests)"
        );
        // Settings over Super Agent releases the keys of the hosted chat, and the TUI view does not take them
        shell.update_in(v, |s, window, cx| s.go(View::Settings, window, cx));
        assert_eq!(keys(&shell, v), ((false, false), vec![(false, false)]));
        shell.update_in(v, |s, window, cx| s.go(View::SuperAgent, window, cx));
        assert_eq!(keys(&shell, v).1, vec![(true, false)]);
        // the TUI view: it takes the keys and the hosted chat gives them up
        shell.update_in(v, |s, window, cx| s.go(View::Tui, window, cx));
        assert_eq!(keys(&shell, v), ((true, true), vec![(false, false)]));
        // another frame in front of the session: no hosted chat takes a key
        shell.update_in(v, |s, window, cx| s.go(View::SuperAgent, window, cx));
        sa.update(v, |s, cx| s.focus_tab(0, cx));
        assert_eq!(keys(&shell, v), ((false, false), vec![(false, false)]));
        sa.update(v, |s, cx| s.focus_tab(1, cx));
        // an embedded question counts for the rail and the mode button
        sa.update(v, |s, _| s.count_waiting = true);
        let before = sa.read_with(v, |s, cx| s.waiting(cx));
        let view = sa.read_with(v, |s, cx| s.host.read(cx).views().remove(0));
        view.update(v, |t, cx| t.push_test_question(cx));
        assert_eq!(sa.read_with(v, |s, cx| s.waiting(cx)), before + 1);
        // closing the tab ends the chat (its engine and panes: the host's own tests wait for the engine lock)
        sa.update(v, |s, cx| s.close_tab(1, cx));
        assert_eq!(sa.read_with(v, |s, cx| s.host.read(cx).len()), 0);
        assert_eq!(
            view.read_with(v, |t, _| (t.shown, t.active)),
            (false, false)
        );
        assert_eq!(sa.read_with(v, |s, cx| s.waiting(cx)), before);
        drop(view);
        shell.update_in(v, |s, window, cx| s.go(View::Tui, window, cx));
        let _ = f;
    }

    #[gpui::test]
    fn the_home_frame_is_the_home_view_and_alt_a_reaches_it(cx: &mut TestAppContext) {
        let _g = theme::opacity_guard();
        let (shell, v) = open(cx, 1400., 900.);
        shell.update_in(v, |s, window, cx| s.go(View::SuperAgent, window, cx));
        shell.update(v, |s, cx| s.count_waiting_in_sample(cx));
        v.run_until_parked();
        assert!(v.debug_bounds("sa-home").is_some());
        let sa = shell.read_with(v, |s, _| s.sa.clone());
        sa.update(v, |s, cx| {
            s.open(
                crate::superagent::frames::Frame::NewGoal(
                    crate::superagent::frames::GoalKind::ReadOnly,
                ),
                cx,
            )
        });
        let ev = gpui::KeyDownEvent {
            keystroke: gpui::Keystroke {
                modifiers: gpui::Modifiers {
                    alt: true,
                    ..Default::default()
                },
                key: "a".into(),
                key_char: None,
            },
            is_held: false,
            prefer_character_input: false,
        };
        assert!(sa.update_in(v, |s, w, cx| s.on_key(&ev, w, cx)));
        assert_eq!(
            sa.read_with(v, |s, _| s.frames.active().clone()),
            crate::superagent::frames::Frame::Home
        );
    }

    #[gpui::test]
    fn leaving_super_agent_drops_the_cards_keys_and_a_later_activation_steals_no_focus(
        cx: &mut TestAppContext,
    ) {
        use gpui::Focusable as _;
        let _g = theme::opacity_guard();
        let (shell, v) = open(cx, 1400., 900.);
        let o = shell.read_with(v, |s, _| s.outcomes().clone());
        shell.update_in(v, |s, window, cx| s.go(View::SuperAgent, window, cx));
        o.update(v, |o, cx| {
            o.state.centre = crate::outcomes_view::model::CentreTab::Goal;
            cx.notify();
        });
        v.run_until_parked();
        o.update_in(v, |o, window, cx| o.focus_decision(window, cx));
        assert!(o.update_in(v, |o, window, _| o.popup_has_keys(window)));
        // Cmd+1 to the TUI view: the card's keys are dropped at once
        shell.update_in(v, |s, window, cx| s.go(View::Tui, window, cx));
        v.run_until_parked();
        // the window going away and coming back must not move the focus to the hidden view
        shell.update_in(v, |s, window, cx| {
            s.outcomes
                .update(cx, |o, cx| o.window_activation_changed(window, cx))
        });
        v.run_until_parked();
        assert!(!o.update_in(v, |o, window, cx| o.focus_handle(cx).is_focused(window)));
        assert!(!o.update_in(v, |o, window, _| o.popup_has_keys(window)));
    }

    #[gpui::test]
    fn history_buttons_do_nothing_while_a_form_is_open(cx: &mut TestAppContext) {
        let _g = theme::opacity_guard();
        let (shell, v) = open(cx, 1400., 900.);
        shell.update_in(v, |s, window, cx| s.go(View::SuperAgent, window, cx));
        let o = shell.read_with(v, |s, _| s.outcomes().clone());
        let second = o.read_with(v, |o, _| o.state.items[1].row.id.clone());
        o.update(v, |o, cx| o.select_id(&second, cx));
        assert!(o.read_with(v, |o, _| o.can_back()));
        let sel = o.read_with(v, |o, _| o.state.selected.clone());
        o.update(v, |o, cx| {
            o.state.open_new();
            cx.notify();
        });
        assert!(!o.read_with(v, |o, _| o.can_back() || o.can_forward()));
        o.update(v, |o, cx| o.nav(true, cx));
        assert_eq!(o.read_with(v, |o, _| o.state.selected.clone()), sel);
    }
}
