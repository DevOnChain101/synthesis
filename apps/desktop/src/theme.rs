//! Platinum palette (docs/cli-palette.md) as GPUI colours. The values are the CLI's own tokens
//! (`synthesis-cli-ui-render::theme`), so the two views cannot drift apart.

use std::sync::atomic::{AtomicU8, Ordering};

use gpui::{App, Hsla, SharedString, WindowBackgroundAppearance};
use gpui_component::{Theme, ThemeMode};
use synthesis_cli_ui_render::theme as t;

pub fn hsla(c: t::Rgb) -> Hsla {
    gpui::rgb(u32::from_be_bytes([0, c[0], c[1], c[2]])).into()
}

static OPACITY: AtomicU8 = AtomicU8::new(100);

/// Sets the window opacity (percent) that [`fill`] paints with. One window per process, so a global.
pub fn set_opacity(pct: u8) {
    OPACITY.store(
        pct.clamp(synthesis_settings::UI_OPACITY_MIN, 100),
        Ordering::Relaxed,
    );
}

/// A background fill at the window opacity. Text and borders use [`hsla`], never this, so they stay opaque.
pub fn fill(c: t::Rgb) -> Hsla {
    Hsla {
        a: f32::from(OPACITY.load(Ordering::Relaxed)) / 100.,
        ..hsla(c)
    }
}

/// `a` with `k` (0..1) of `b` mixed in.
pub fn mix(a: t::Rgb, b: t::Rgb, k: f32) -> t::Rgb {
    let m = |x: u8, y: u8| (f32::from(x) + (f32::from(y) - f32::from(x)) * k).round() as u8;
    [m(a[0], b[0]), m(a[1], b[1]), m(a[2], b[2])]
}

/// The two ends of the window ground: the brand BG drifting from a cool tint at the top to a warm one at the
/// bottom, so the app never sits on a flat near-black. Small steps: the text tokens keep their contrast (tested).
pub fn ground_stops() -> (t::Rgb, t::Rgb) {
    (mix(t::BG, t::INFO, 0.05), mix(t::BG, t::ACCENT_DEEP, 0.14))
}

/// The window ground at the window opacity (see [`ground_stops`]); `fill(t::BG)` with a soft drift.
pub fn ground() -> gpui::Background {
    let (top, bottom) = ground_stops();
    gpui::linear_gradient(
        180.,
        gpui::linear_color_stop(fill(top), 0.),
        gpui::linear_color_stop(fill(bottom), 1.),
    )
}

/// A surface that must stay readable at any window opacity (the approval dock, the Settings popup): always opaque.
pub fn solid(c: t::Rgb) -> Hsla {
    hsla(c)
}

/// What the window asks the platform for: nothing behind it at 100 %, else see-through, blurred if wanted
/// and supported.
pub fn background(tui: &synthesis_settings::UiSettings) -> WindowBackgroundAppearance {
    if tui.opacity >= 100 {
        WindowBackgroundAppearance::Opaque
    } else if tui.blur && crate::settings_view::model::blur_unsupported().is_none() {
        WindowBackgroundAppearance::Blurred
    } else {
        WindowBackgroundAppearance::Transparent
    }
}

/// Colour token and icon of a permission mode (the mode bar). One place; every `Mode` differs.
pub fn mode_look(m: synthesis_permissions::Mode) -> (t::Rgb, &'static str) {
    use synthesis_permissions::Mode::*;
    match m {
        Default => (t::INFO, "shield"),
        AcceptEdits => (t::ACCENT_LIGHT, "pencil-check"),
        Plan => (t::SUCCESS, "layers"),
        Auto => (t::WARN, "chevrons-right"),
        DontAsk => (t::CYAN, "cpu"),
        BypassPermissions => (t::ERROR, "wrench"),
    }
}

/// Colours of the values in the Session and Context panels (labels stay dim). One place, platinum tokens only.
pub mod value {
    use synthesis_cli_ui_render::theme as t;
    pub const MODE: t::Rgb = t::CYAN;
    pub const MODEL: t::Rgb = t::ACCENT_LIGHT;
    pub const PATH: t::Rgb = t::INFO;
    pub const NUMBER: t::Rgb = t::ACCENT;
}

/// Context use: green below 60 percent, amber from 60, red from 85.
pub fn ctx_level(pct: f32) -> t::Rgb {
    if pct >= 85. {
        t::ERROR
    } else if pct >= 60. {
        t::WARN
    } else {
        t::SUCCESS
    }
}

/// The colour of a session status word.
pub fn status_colour(word: &str) -> t::Rgb {
    match word {
        "running" | "working" | "verifying" => t::SUCCESS,
        "starting" | "waiting approval" => t::WARN,
        "failed" | "blocked" | "interrupted" => t::ERROR,
        _ => t::INFO,
    }
}

/// Dark mode with the platinum tokens. Chrome uses the bundled UI sans (`brand::UI_FONT`); `mono`
/// is the monospace of the TUI view and of code.
pub fn apply(mono: &str, cx: &mut App) {
    Theme::change(ThemeMode::Dark, None, cx);
    let th = Theme::global_mut(cx);
    let c = &mut th.colors;
    c.background = hsla(t::BG);
    c.foreground = hsla(t::TEXT_STRONG);
    c.border = hsla(t::BORDER);
    c.input = hsla(t::RULE);
    c.muted = hsla(t::SURFACE);
    c.muted_foreground = hsla(t::MUTED);
    c.accent = hsla(t::ELEVATED);
    c.accent_foreground = hsla(t::TEXT_STRONG);
    c.primary = hsla(t::SILVER);
    c.primary_hover = hsla(t::PEARL);
    c.primary_active = hsla(t::ACCENT);
    c.primary_foreground = hsla(t::BG);
    c.secondary = hsla(t::SURFACE);
    c.secondary_hover = hsla(t::ELEVATED);
    c.secondary_active = hsla(t::BORDER);
    c.secondary_foreground = hsla(t::TEXT_STRONG);
    c.popover = hsla(t::SURFACE);
    c.popover_foreground = hsla(t::TEXT_STRONG);
    c.sidebar = hsla(t::SURFACE);
    c.sidebar_border = hsla(t::BORDER);
    c.sidebar_foreground = hsla(t::TEXT);
    c.sidebar_accent = hsla(t::ELEVATED);
    c.sidebar_accent_foreground = hsla(t::TEXT_STRONG);
    c.title_bar = hsla(t::SURFACE);
    c.title_bar_border = hsla(t::BORDER);
    c.success = hsla(t::SUCCESS);
    c.warning = hsla(t::WARN);
    c.danger = hsla(t::ERROR);
    c.ring = hsla(t::ACCENT);
    c.caret = hsla(t::PEARL);
    th.font_family = SharedString::from(crate::brand::UI_FONT);
    th.mono_font_family = SharedString::from(mono.to_string());
}

/// Tests that read or write the opacity global (any test with a `Shell`, which applies settings) hold this for their whole
/// run: the global is per process and tests run on parallel threads. It starts each of them at 100 %.
#[cfg(test)]
pub(crate) fn opacity_guard() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let g = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    set_opacity(100);
    g
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_ground_is_never_flat_black_and_text_keeps_its_contrast_on_it() {
        let lum = |c: t::Rgb| {
            let l = |v: u8| {
                let v = f64::from(v) / 255.;
                if v <= 0.03928 {
                    v / 12.92
                } else {
                    ((v + 0.055) / 1.055).powf(2.4)
                }
            };
            0.2126 * l(c[0]) + 0.7152 * l(c[1]) + 0.0722 * l(c[2])
        };
        let ratio = |a: t::Rgb, b: t::Rgb| {
            let (x, y) = (lum(a), lum(b));
            (x.max(y) + 0.05) / (x.min(y) + 0.05)
        };
        let (top, bottom) = ground_stops();
        assert_ne!(top, bottom);
        for g in [top, bottom] {
            assert!(g != [0, 0, 0] && g.iter().any(|v| *v > t::BG[2]), "{g:?}");
            for fg in [t::MUTED, t::TEXT, t::TEXT_STRONG] {
                assert!(ratio(fg, g) >= 4.5, "{fg:?} on {g:?}");
            }
        }
        assert_eq!(mix([0, 0, 0], [200, 100, 50], 0.5), [100, 50, 25]);
    }

    #[test]
    fn every_mode_has_its_own_label_colour_and_icon() {
        use synthesis_permissions::Mode;
        let looks: Vec<_> = Mode::ALL
            .iter()
            .map(|m| {
                let (c, i) = mode_look(*m);
                assert!(
                    crate::widgets::icons::body(crate::widgets::icons::for_lucide(i).unwrap())
                        .is_some()
                );
                (
                    synthesis_cli_ui_render::dialogs::mode_name(Some(m.as_str())),
                    c,
                    i,
                )
            })
            .collect();
        for (a, x) in looks.iter().enumerate() {
            for y in &looks[a + 1..] {
                assert!(x.0 != y.0 && x.1 != y.1 && x.2 != y.2, "{x:?} {y:?}");
            }
        }
    }

    #[test]
    fn fill_follows_the_opacity_and_never_goes_under_the_minimum() {
        let _g = opacity_guard();
        set_opacity(55);
        assert_eq!(fill(t::BG).a, 0.55);
        assert_eq!(hsla(t::BG).a, 1., "text colours stay opaque");
        set_opacity(1);
        assert_eq!(fill(t::BG).a, 0.30);
        set_opacity(30);
        assert_eq!(
            solid(t::SURFACE).a,
            1.,
            "the approval dock and the popup ignore the opacity"
        );
        set_opacity(100);
        assert_eq!(fill(t::BG), hsla(t::BG));
    }

    #[test]
    fn background_asks_for_what_the_settings_and_platform_allow() {
        let mut s = synthesis_settings::UiSettings::default();
        assert_eq!(background(&s), WindowBackgroundAppearance::Opaque);
        s.blur = true;
        assert_eq!(
            background(&s),
            WindowBackgroundAppearance::Opaque,
            "nothing to see through"
        );
        s.opacity = 80;
        let want = if cfg!(any(target_os = "macos", target_os = "windows")) {
            WindowBackgroundAppearance::Blurred
        } else {
            WindowBackgroundAppearance::Transparent
        };
        assert_eq!(background(&s), want);
        s.blur = false;
        assert_eq!(background(&s), WindowBackgroundAppearance::Transparent);
    }

    #[test]
    fn context_and_status_colours_follow_the_level() {
        assert_eq!(ctx_level(0.), t::SUCCESS);
        assert_eq!(ctx_level(59.9), t::SUCCESS);
        assert_eq!(ctx_level(60.), t::WARN);
        assert_eq!(ctx_level(84.9), t::WARN);
        assert_eq!(ctx_level(85.), t::ERROR);
        assert_eq!(status_colour("running"), t::SUCCESS);
        assert_eq!(status_colour("starting"), t::WARN);
        assert_eq!(status_colour("idle"), t::INFO);
        let v = [value::MODE, value::MODEL, value::PATH, value::NUMBER];
        for (i, a) in v.iter().enumerate() {
            assert!(
                v[i + 1..].iter().all(|b| a != b),
                "names and numbers differ"
            );
        }
    }

    #[test]
    fn tokens_are_the_documented_hex() {
        for (c, hex) in [
            (t::BG, 0x0B0D0F),
            (t::SURFACE, 0x17191D),
            (t::BORDER, 0x393A3E),
            (t::SILVER, 0xD8D8D8),
            (t::TEXT_STRONG, 0xF2F2F2),
            (t::MUTED, 0x85868A),
            (t::ACCENT, 0xC7AE90),
        ] {
            assert_eq!(hsla(c), gpui::rgb(hex).into());
        }
    }
}
