//! The window: a title bar with the view icons (TUI, Super Agent, Settings) over the content of the active view.
//!
//! The title bar follows the design's chrome (`index.html` `#chrome`): a 30px row above the content,
//! the icon group (`.cb` 26x22, radius 6, hover and pressed backgrounds) at the right, the gear after it. In the
//! Super Agent view three more buttons sit at the left (hide or show the list, back, forward) with a breadcrumb
//! (`super-agent-v7-1.svg`); the right side does not move. macOS
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
    /// A restart was asked from Settings and the new session is not live yet.
    restarting: bool,
    /// The Super Agent button counts what waits for the person: only a live app (never the sample), and the count is
    /// 0 while the orchestrator is off, unsupported or not connected.
    count_waiting: bool,
    /// The timer of the next frame of the title bar's blinking red (see `chrome::next_frame`).
    anim_tick: Option<gpui::Task<()>>,
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
        }
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

    /// The Super Agent view (the offscreen shots seed it).
    #[cfg(any(test, feature = "shot"))]
    pub(crate) fn outcomes(&self) -> &Entity<OutcomesView> {
        &self.outcomes
    }

    /// The badge counts the sample's waiting things too (shots and tests of the badge).
    #[cfg(any(test, feature = "shot"))]
    pub(crate) fn count_waiting_in_sample(&mut self, cx: &mut Context<Self>) {
        self.count_waiting = true;
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
        tui: synthesis_settings::UiSettings,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        theme::set_opacity(tui.opacity);
        window.set_background_appearance(theme::background(&tui));
        self.tui.update(cx, |v, cx| v.apply_settings(tui, cx));
        cx.notify();
    }

    /// What waits for the person (the number on the Super Agent button): 0 unless the app counts it.
    fn waiting(&self, cx: &App) -> usize {
        if self.count_waiting {
            self.outcomes.read(cx).waiting_count()
        } else {
            0
        }
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

    /// A button of the Super Agent title bar's left group: same size and look as the icon group's. Disabled (dim,
    /// no hover, no action) when there is nowhere to go.
    fn nav_button(
        &self,
        name: &'static str,
        glyph: &'static str,
        tip: String,
        (on, enabled): (bool, bool),
        act: impl Fn(&mut Self, &mut Window, &mut Context<Self>) + 'static,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let fg = if !enabled {
            with_alpha(theme::hsla(t::MUTED), 0.45)
        } else if on {
            theme::hsla(t::TEXT_STRONG)
        } else {
            theme::hsla(t::MUTED)
        };
        div()
            .id(name)
            .debug_selector(move || name.to_string())
            .w(px(26.))
            .h(px(22.))
            .flex()
            .items_center()
            .justify_center()
            .rounded(px(6.))
            .text_color(fg)
            .when(on, |d| d.bg(with_alpha(theme::hsla(t::TEXT_STRONG), 0.16)))
            .when(enabled, |d| {
                d.cursor_pointer().hover(|d| {
                    d.bg(with_alpha(theme::hsla(t::TEXT_STRONG), 0.10))
                        .text_color(theme::hsla(t::TEXT_STRONG))
                })
            })
            .tooltip(move |_, cx| -> AnyView { cx.new(|_| Tip(tip.clone())).into() })
            .on_click(cx.listener(move |s, _, window, cx| {
                if enabled {
                    act(s, window, cx);
                }
            }))
            .child(icon(glyph, 14., fg))
    }

    /// The Super Agent title bar's left group (hide or show the list, back, forward) and the breadcrumb.
    fn nav_group(&self, cx: &mut Context<Self>) -> (impl IntoElement, impl IntoElement) {
        let (rail, back, fwd, goal) = {
            let o = self.outcomes.read(cx);
            (
                o.rail_open,
                o.can_back(),
                o.can_forward(),
                // capped before sanitising: the whole goal is never walked every frame
                o.breadcrumb_goal().map(|g| {
                    synthesis_harness::sanitize_for_display(
                        &g.chars().take(200).collect::<String>(),
                    )
                }),
            )
        };
        let group = div()
            .flex()
            .flex_none()
            .gap(px(1.))
            .p(px(2.))
            .rounded(px(8.))
            .bg(with_alpha(theme::hsla(t::TEXT_STRONG), 0.07))
            .child(self.nav_button(
                "nav-rail",
                "rail",
                format!("{} the list", if rail { "Hide" } else { "Show" }),
                (!rail, true),
                |s, _, cx| s.outcomes.update(cx, |o, cx| o.toggle_rail(cx)),
                cx,
            ))
            .child(self.nav_button(
                "nav-back",
                "nav-back",
                "Back  (Alt+Left)".into(),
                (false, back),
                |s, _, cx| s.outcomes.update(cx, |o, cx| o.nav(true, cx)),
                cx,
            ))
            .child(self.nav_button(
                "nav-forward",
                "nav-forward",
                "Forward  (Alt+Right)".into(),
                (false, fwd),
                |s, _, cx| s.outcomes.update(cx, |o, cx| o.nav(false, cx)),
                cx,
            ));
        let crumb = div()
            .debug_selector(|| "breadcrumb".to_string())
            .flex()
            .items_center()
            .gap(px(6.))
            .min_w(px(0.))
            .overflow_hidden()
            .text_size(px(12.))
            .child(
                div()
                    .flex_none()
                    .whitespace_nowrap()
                    .text_color(theme::hsla(if goal.is_some() {
                        t::MUTED
                    } else {
                        t::TEXT_STRONG
                    }))
                    .child("Super Agent"),
            )
            .children(goal.map(|g| {
                div()
                    .flex()
                    .items_center()
                    .gap(px(6.))
                    .min_w(px(0.))
                    .child(
                        div()
                            .flex_none()
                            .text_color(theme::hsla(t::MUTED))
                            .child("\u{203a}"),
                    )
                    .child(
                        div()
                            .min_w(px(0.))
                            .truncate()
                            .text_color(theme::hsla(t::TEXT_STRONG))
                            .child(g),
                    )
            }));
        (group, crumb)
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
        // the left group and the breadcrumb belong to the Super Agent view (also while Settings is over it)
        let (group, crumb) = if self.view == View::SuperAgent
            || (self.view == View::Settings && self.back == View::SuperAgent)
        {
            let (g, c) = self.nav_group(cx);
            (Some(g), Some(c))
        } else {
            (None, None)
        };
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
            .children(group)
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
                    .children(crumb)
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
            View::SuperAgent => div().size_full().child(self.outcomes.clone()),
            // the panel slides over the view it was opened from
            View::Settings => {
                let under = match self.back {
                    View::SuperAgent => div().size_full().child(self.outcomes.clone()),
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
        Self::with_outcomes(window, cx, tui, settings_cfg, env)
    }

    /// The shell over a TUI view the caller built (the offscreen shots fill it before it is drawn); the
    /// Super Agent view shows its sample.
    pub fn around(
        window: &mut Window,
        cx: &mut Context<Self>,
        tui: Entity<TuiView>,
        settings_cfg: Result<settings_view::model::Cfg, String>,
    ) -> Self {
        Self::with_outcomes(window, cx, tui, settings_cfg, None)
    }

    /// `outcomes_env`: the environment the live Super Agent view reads its setting and home from, and the workspace a new outcome works in by default; `None` is the sample.
    fn with_outcomes(
        window: &mut Window,
        cx: &mut Context<Self>,
        tui: Entity<TuiView>,
        settings_cfg: Result<settings_view::model::Cfg, String>,
        outcomes_env: Option<(synthesis_config::Env, String)>,
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
        // the title bar's badge follows the list and what the sessions wait for
        cx.observe(&outcomes, |_, _, cx| cx.notify()).detach();
        cx.subscribe_in(
            &outcomes,
            window,
            |s: &mut Shell, _, e: &OpenSession, window, cx| {
                // a session that waits for a tool permission is answered in the TUI view, in that session
                s.go(View::Tui, window, cx);
                if !e.0.starts_with('#') {
                    s.tui.update(cx, |t, cx| t.open_session(&e.0, cx));
                }
            },
        )
        .detach();
        cx.subscribe_in(
            &outcomes,
            window,
            |s: &mut Shell, _, e: &crate::outcomes_view::OpenFolder, window, cx| {
                s.go(View::Tui, window, cx);
                s.tui.update(cx, |t, cx| t.open_folder(&e.0, cx));
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
            v.debug_bounds("outcomes").is_some(),
            "the Super Agent view is"
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
    fn super_agent_has_three_buttons_at_the_left_and_the_mode_buttons_do_not_move(
        cx: &mut TestAppContext,
    ) {
        let _g = theme::opacity_guard();
        for (w, h) in SIZES {
            let (shell, v) = open(cx, w, h);
            // the TUI view: no left group, the mode buttons and the gear where they always were
            assert!(v.debug_bounds("nav-rail").is_none() && v.debug_bounds("breadcrumb").is_none());
            let tui_mode: Vec<_> = MODES.iter().map(|m| b(v, m)).collect();
            shell.update_in(v, |s, window, cx| s.go(View::SuperAgent, window, cx));
            v.run_until_parked();
            // Super Agent: the same right side, pixel for pixel
            let sa_mode: Vec<_> = MODES.iter().map(|m| b(v, m)).collect();
            assert_eq!(tui_mode, sa_mode, "{w}x{h}: the mode buttons stay put");
            // three 26x22 buttons in a row after the traffic lights (macOS) or the edge, 1px apart, centred in the bar
            let left = if cfg!(target_os = "macos") { 78. } else { 10. } + 2.;
            let (rail, back, fwd) = (b(v, "nav-rail"), b(v, "nav-back"), b(v, "nav-forward"));
            // (the bar is 30px with a 1px rule under it)
            let y = (TITLEBAR_H - 1. - 22.) / 2.;
            assert_eq!(rail, (left, y, 26., 22.), "{w}x{h}");
            assert_eq!(back, (left + 27., y, 26., 22.), "{w}x{h}");
            assert_eq!(fwd, (left + 54., y, 26., 22.), "{w}x{h}");
            // the breadcrumb follows them, and everything left of the mode buttons
            let crumb = b(v, "breadcrumb");
            assert!(crumb.0 >= fwd.0 + fwd.2, "{w}x{h}: {crumb:?}");
            assert!(
                crumb.0 + crumb.2 <= sa_mode[0].0,
                "{w}x{h}: {crumb:?} runs into the mode buttons"
            );
            assert!(
                sa_mode[0].0 < sa_mode[1].0 && sa_mode[1].0 < sa_mode[2].0,
                "TUI | Super Agent | gear"
            );
            assert!(
                sa_mode[2].0 - (sa_mode[1].0 + sa_mode[1].2) < 20.,
                "the gear is next to them"
            );
            // back to the TUI view: the left group is gone again
            shell.update_in(v, |s, window, cx| s.go(View::Tui, window, cx));
            v.run_until_parked();
            assert!(v.debug_bounds("nav-rail").is_none());
        }
    }

    #[gpui::test]
    fn the_breadcrumb_names_the_selected_outcome(cx: &mut TestAppContext) {
        let _g = theme::opacity_guard();
        let (shell, v) = open(cx, 1400., 900.);
        shell.update_in(v, |s, window, cx| s.go(View::SuperAgent, window, cx));
        v.run_until_parked();
        let goal = shell.read_with(v, |s, cx| {
            s.outcomes.read(cx).breadcrumb_goal().map(String::from)
        });
        assert!(
            goal.is_some_and(|g| !g.is_empty()),
            "the sample selects an outcome"
        );
        // wider than the bare "Super Agent" label
        assert!(b(v, "breadcrumb").2 > 100.);
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
        let c = v.debug_bounds("mode-tui").unwrap().center();
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
    fn the_left_buttons_walk_the_history_and_hide_the_list(cx: &mut TestAppContext) {
        let _g = theme::opacity_guard();
        let (shell, v) = open(cx, 1400., 900.);
        shell.update_in(v, |s, window, cx| s.go(View::SuperAgent, window, cx));
        v.run_until_parked();
        let o = shell.read_with(v, |s, _| s.outcomes().clone());
        let sel = |o: &Entity<OutcomesView>, v: &mut gpui::VisualTestContext| {
            o.read_with(v, |o, _| o.state.selected.clone())
        };
        let first = sel(&o, v);
        assert!(!o.read_with(v, |o, _| o.can_back()), "nowhere to go yet");
        // a click on a disabled button does nothing
        let c = v.debug_bounds("nav-back").unwrap().center();
        v.simulate_click(c, Default::default());
        assert_eq!(sel(&o, v), first);
        let second = o.read_with(v, |o, _| {
            o.state
                .items
                .iter()
                .map(|i| i.row.id.clone())
                .find(|i| Some(i) != first.as_ref())
        });
        o.update(v, |o, cx| o.select_id(&second.clone().unwrap(), cx));
        v.run_until_parked();
        assert_eq!(sel(&o, v), second);
        let c = v.debug_bounds("nav-back").unwrap().center();
        v.simulate_click(c, Default::default());
        v.run_until_parked();
        assert_eq!(sel(&o, v), first, "back");
        let c = v.debug_bounds("nav-forward").unwrap().center();
        v.simulate_click(c, Default::default());
        v.run_until_parked();
        assert_eq!(sel(&o, v), second, "forward");
        // the rail button hides the list column and shows it again; the state is the window's own
        assert!(v.debug_bounds("out-left").is_some());
        let c = v.debug_bounds("nav-rail").unwrap().center();
        v.simulate_click(c, Default::default());
        v.run_until_parked();
        assert!(v.debug_bounds("out-left").is_none() && !o.read_with(v, |o, _| o.rail_open));
        let c = v.debug_bounds("nav-rail").unwrap().center();
        v.simulate_click(c, Default::default());
        v.run_until_parked();
        assert!(v.debug_bounds("out-left").is_some());
    }

    #[gpui::test]
    fn the_left_group_stays_while_settings_is_over_super_agent(cx: &mut TestAppContext) {
        let _g = theme::opacity_guard();
        let (shell, v) = open(cx, 1100., 700.);
        shell.update_in(v, |s, window, cx| s.go(View::SuperAgent, window, cx));
        shell.update_in(v, |s, window, cx| s.go(View::Settings, window, cx));
        v.run_until_parked();
        assert!(v.debug_bounds("nav-rail").is_some());
        shell.update_in(v, |s, window, cx| s.go(View::Tui, window, cx));
        v.run_until_parked();
        assert!(v.debug_bounds("nav-rail").is_none());
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
