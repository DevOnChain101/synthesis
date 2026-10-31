//! Dark theme tokens, taken from the `:root` of `design/03-designs/desktop/desktop.html`
//! (docs/specs/desktop.md section 8). Colour means status only; the light theme comes later with
//! the same names.

use gpui::{App, Hsla, SharedString};
use gpui_component::{Theme, ThemeMode};

pub const BG: u32 = 0x0B0D10;
pub const SURFACE: u32 = 0x14171C;
pub const PANEL: u32 = 0x181C22;
pub const LINE: u32 = 0x2B3038;
pub const MUTED: u32 = 0x9096A0;
pub const TEXT: u32 = 0xEEF0F2;
pub const SILVER: u32 = 0xC6C9CE;
pub const PEARL: u32 = 0xF0E9E3;
pub const ACCENT: u32 = 0xC7BDB6;
pub const SUCCESS: u32 = 0xA0BDAA;
pub const PENDING: u32 = 0xD5C2AD;
pub const ERROR: u32 = 0xB65E60;

// Fixed values of desktop.html outside `:root` (sidebar, title bar, active nav item, buttons).
pub const SIDEBAR: u32 = 0x11151A;
pub const TITLEBAR: u32 = 0x15181D;
pub const NAV_ACTIVE: u32 = 0x292D32;
pub const BUTTON_WHITE: u32 = 0xEBE9E6;
pub const BUTTON_WHITE_TEXT: u32 = 0x151619;
pub const INPUT_BORDER: u32 = 0x414852;

// Code viewer colours (muted, after the code panel of desktop-preview.png).
pub const SYNTAX_KEYWORD: u32 = 0xA9B4E8;
pub const SYNTAX_COMMENT: u32 = 0x6E7681;

pub fn hsla(rgb: u32) -> Hsla {
    gpui::rgb(rgb).into()
}

/// Dark mode with the Synthesis tokens. Chrome uses the UI sans (`brand::UI_FONT`); `font` (a
/// family name, the Settings > Appearance choice) is the monospace for code and terminal only.
pub fn apply(font: &str, cx: &mut App) {
    Theme::change(ThemeMode::Dark, None, cx);
    let t = Theme::global_mut(cx);
    let c = &mut t.colors;
    c.background = hsla(BG);
    c.foreground = hsla(TEXT);
    c.border = hsla(LINE);
    c.input = hsla(INPUT_BORDER);
    c.muted = hsla(SURFACE);
    c.muted_foreground = hsla(MUTED);
    c.accent = hsla(NAV_ACTIVE);
    c.accent_foreground = hsla(TEXT);
    c.primary = hsla(BUTTON_WHITE);
    c.primary_hover = hsla(SILVER);
    c.primary_active = hsla(ACCENT);
    c.primary_foreground = hsla(BUTTON_WHITE_TEXT);
    c.secondary = hsla(PANEL);
    c.secondary_hover = hsla(NAV_ACTIVE);
    c.secondary_active = hsla(LINE);
    c.secondary_foreground = hsla(TEXT);
    c.popover = hsla(PANEL);
    c.popover_foreground = hsla(TEXT);
    c.sidebar = hsla(SIDEBAR);
    c.sidebar_border = hsla(LINE);
    c.sidebar_foreground = hsla(SILVER);
    c.sidebar_accent = hsla(NAV_ACTIVE);
    c.sidebar_accent_foreground = hsla(TEXT);
    c.title_bar = hsla(TITLEBAR);
    c.title_bar_border = hsla(LINE);
    c.success = hsla(SUCCESS);
    c.warning = hsla(PENDING);
    c.danger = hsla(ERROR);
    c.ring = hsla(ACCENT);
    c.caret = hsla(PEARL);
    t.font_family = SharedString::from(crate::brand::UI_FONT);
    t.mono_font_family = SharedString::from(font.to_string());
}

#[cfg(test)]
mod tests {
    use super::*;

    const HTML: &str = include_str!("../../../design/03-designs/desktop/desktop.html");

    fn root_var(name: &str) -> u32 {
        let key = format!("--{name}:#");
        let at = HTML
            .find(&key)
            .unwrap_or_else(|| panic!("{key} not in :root"))
            + key.len();
        u32::from_str_radix(&HTML[at..at + 6], 16).unwrap()
    }

    #[test]
    fn tokens_match_the_design_root() {
        for (name, v) in [
            ("bg", BG),
            ("surface", SURFACE),
            ("panel", PANEL),
            ("line", LINE),
            ("muted", MUTED),
            ("text", TEXT),
            ("silver", SILVER),
            ("pearl", PEARL),
            ("accent", ACCENT),
        ] {
            assert_eq!(root_var(name), v, "--{name}");
        }
    }

    #[test]
    fn status_and_chrome_colours_appear_in_the_design() {
        for v in [
            SUCCESS,
            PENDING,
            ERROR,
            SIDEBAR,
            TITLEBAR,
            NAV_ACTIVE,
            BUTTON_WHITE,
            INPUT_BORDER,
        ] {
            let hex = format!("#{v:06x}");
            assert!(HTML.contains(&hex), "{hex} missing from desktop.html");
        }
    }
}
