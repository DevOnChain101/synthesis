//! Platinum palette (docs/tui-palette.md) as GPUI colours. The values are the TUI's own tokens
//! (`synthesis-tui-render::theme`), so the two views cannot drift apart.

use gpui::{App, Hsla, SharedString};
use gpui_component::{Theme, ThemeMode};
use synthesis_tui_render::theme as t;

pub fn hsla(c: t::Rgb) -> Hsla {
    gpui::rgb(u32::from_be_bytes([0, c[0], c[1], c[2]])).into()
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

#[cfg(test)]
mod tests {
    use super::*;

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
