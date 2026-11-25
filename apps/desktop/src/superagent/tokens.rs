//! Super Agent design tokens: the values of `super-agent-v3/index.html` `:root` (DESIGN-NOTES section 10.2).
//! Separate from `theme.rs` on purpose, the TUI view keeps its own palette. The ten brand colours are checked against
//! `design/02-design-tokens/tokens.css` by a test, so they cannot drift.

use std::time::Duration;

use gpui::{FontWeight, Hsla, rgb};

// Brand colours (0xRRGGBB).
pub const BG: u32 = 0x0B0C0F;
pub const SURFACE: u32 = 0x15171B;
pub const ELEVATED: u32 = 0x242830;
pub const BORDER: u32 = 0x3A3F4A;
pub const TEXT: u32 = 0x9CA3AF;
pub const TEXT_STRONG: u32 = 0xE5E7EB;
pub const ACCENT: u32 = 0xD9D9E0;
pub const ACCENT_LIGHT: u32 = 0xF7F8FA;
pub const MUTED_BLUE: u32 = 0x0B1322;
pub const SOFT_BEIGE: u32 = 0xD7CEC7;
const WHITE: u32 = 0xFFFFFF;

pub fn hsla(c: u32) -> Hsla {
    rgb(c).into()
}

/// `c` at `a` (0..1).
pub fn alpha(c: u32, a: f32) -> Hsla {
    Hsla { a, ..hsla(c) }
}

// Derived tints.
pub const STROKE_A: f32 = 0.08;
pub const HOVER_A: f32 = 0.06;
pub const WELL_A: f32 = 0.45;
pub const WARM_TOP_A: f32 = 0.20;
pub const SCRIM_A: f32 = 0.72;
/// Glass tints (macOS 27+ only): surface and elevated.
pub const GLASS_TINT_A: f32 = 0.86;
pub const GLASS_TINT_POP_A: f32 = 0.88;

/// `c` as a window background: at the window opacity (`synt.tui.opacity`), like the TUI view's fills.
pub fn window_fill(c: u32) -> Hsla {
    alpha(c, crate::theme::alpha())
}

/// A shell fill of the Super Agent view (rail, title row, sidebar): solid, or the glass tint over the blurred window.
pub fn surface_fill() -> Hsla {
    match super::material::current() {
        super::material::Material::Glass => alpha(SURFACE, GLASS_TINT_A),
        super::material::Material::Opaque => window_fill(SURFACE),
    }
}
/// 1 px hairlines and inset rims.
pub fn stroke() -> Hsla {
    alpha(WHITE, STROKE_A)
}
pub fn hover() -> Hsla {
    alpha(WHITE, HOVER_A)
}
/// The housing of a segmented control.
pub fn well() -> Hsla {
    alpha(BG, WELL_A)
}
/// Soft beige as the light that hits every top edge.
pub fn warm_top() -> Hsla {
    alpha(SOFT_BEIGE, WARM_TOP_A)
}
pub fn scrim() -> Hsla {
    alpha(BG, SCRIM_A)
}
/// Primary button fill: accent 14 % over elevated, precomputed (gpui fills do not stack).
pub fn primary_fill() -> Hsla {
    let (a, e) = (hsla(ACCENT).to_rgb(), hsla(ELEVATED).to_rgb());
    let mix = |x: f32, y: f32| y + (x - y) * 0.14;
    Hsla::from(gpui::Rgba {
        r: mix(a.r, e.r),
        g: mix(a.g, e.g),
        b: mix(a.b, e.b),
        a: 1.,
    })
}
/// Primary button rim: accent 38 %.
pub fn primary_rim() -> Hsla {
    alpha(ACCENT, 0.38)
}
/// Pressed controls scale to this (the mock's `:active`); gpui has no transforms, so the kit uses the hover fill.
pub const PRESS_SCALE: f32 = 0.97;

// Radii: window 20 (glass only) > float 16 (glass; 12 opaque) > panel 12 > control 8; pill = full.
pub const R_WINDOW: f32 = 20.;
pub const R_FLOAT: f32 = 16.;
pub const R_PANEL: f32 = 12.;
pub const R_CONTROL: f32 = 8.;
pub const R_PILL: f32 = 999.;

// Spacing scale.
pub const S1: f32 = 4.;
pub const S2: f32 = 8.;
pub const S3: f32 = 12.;
pub const S4: f32 = 16.;
pub const S5: f32 = 24.;
pub const S6: f32 = 32.;

// Type scale: size and line height in px.
pub const T1: (f32, f32) = (11., 16.);
pub const T2: (f32, f32) = (12., 16.);
pub const T3: (f32, f32) = (13., 20.);
pub const T4: (f32, f32) = (15., 24.);
pub const T5: (f32, f32) = (22., 28.);
pub const W_REGULAR: FontWeight = FontWeight::NORMAL;
pub const W_MEDIUM: FontWeight = FontWeight::MEDIUM;
pub const W_SEMIBOLD: FontWeight = FontWeight::SEMIBOLD;
/// Letter spacing in em: titles (T5) and heads (T4).
pub const TRACK_TITLE: f32 = -0.022;
pub const TRACK_HEAD: f32 = -0.011;

// Motion.
pub const D_PRESS: Duration = Duration::from_millis(90);
pub const D_HOVER: Duration = Duration::from_millis(140);
pub const D_POP: Duration = Duration::from_millis(180);
pub const D_SHEET: Duration = Duration::from_millis(280);
/// Spring settle time (overshoot about 5 %).
pub const D_SPRING: Duration = Duration::from_millis(320);
/// Cubic-bezier control points (x1, y1, x2, y2).
pub const EASE_OUT: [f32; 4] = [0.23, 1., 0.32, 1.];
pub const EASE_IN_OUT: [f32; 4] = [0.77, 0., 0.175, 1.];
pub const EASE_DRAWER: [f32; 4] = [0.32, 0.72, 0., 1.];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn brand_colours_equal_tokens_css() {
        let css = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../design/02-design-tokens/tokens.css"
        ))
        .unwrap();
        let want = [
            ("bg", BG),
            ("surface", SURFACE),
            ("elevated", ELEVATED),
            ("border", BORDER),
            ("text", TEXT),
            ("text-strong", TEXT_STRONG),
            ("accent", ACCENT),
            ("accent-light", ACCENT_LIGHT),
            ("muted-blue", MUTED_BLUE),
            ("soft-beige", SOFT_BEIGE),
        ];
        for (name, v) in want {
            let line = css
                .lines()
                .find_map(|l| l.trim().strip_prefix(&format!("--{name}:")))
                .unwrap_or_else(|| panic!("--{name} missing in tokens.css"));
            let hex = line.trim().trim_end_matches(';').trim_start_matches('#');
            assert_eq!(u32::from_str_radix(hex, 16).unwrap(), v, "--{name}");
        }
    }

    fn lum(c: u32) -> f32 {
        let f = |v: u32| {
            let v = v as f32 / 255.;
            if v <= 0.03928 {
                v / 12.92
            } else {
                ((v + 0.055) / 1.055).powf(2.4)
            }
        };
        0.2126 * f(c >> 16 & 255) + 0.7152 * f(c >> 8 & 255) + 0.0722 * f(c & 255)
    }

    /// `c` at alpha `a` over a grey backdrop of `b` (0 or 255), as a 0xRRGGBB.
    fn over(c: u32, a: f32, b: u32) -> u32 {
        let ch = |sh: u32| (((c >> sh & 255) as f32) * a + b as f32 * (1. - a)).round() as u32;
        ch(16) << 16 | ch(8) << 8 | ch(0)
    }

    #[test]
    fn glass_fills_keep_aa_text_contrast_on_the_worst_backdrop() {
        // GLASS_TINT_POP_A (elevated, 88 %) gives 3.98 for TEXT over white, so no text-carrying fill uses it
        for (fill, a) in [(SURFACE, GLASS_TINT_A)] {
            for backdrop in [0x00, 0xFF] {
                let bg = lum(over(fill, a, backdrop));
                for text in [TEXT, TEXT_STRONG] {
                    let l = lum(text);
                    let ratio = (l + 0.05) / (bg + 0.05);
                    assert!(
                        ratio >= 4.5,
                        "{text:06X} on {fill:06X}@{a} over {backdrop:#X}: {ratio:.2}"
                    );
                }
            }
        }
    }

    #[test]
    fn no_colour_literal_is_written_twice() {
        let src = include_str!("tokens.rs");
        let src = src.split("#[cfg(test)]").next().unwrap();
        let mut seen = std::collections::HashSet::new();
        for w in src.split(|c: char| !c.is_ascii_alphanumeric()) {
            if w.len() == 8 && w.starts_with("0x") && w[2..].bytes().all(|b| b.is_ascii_hexdigit())
            {
                assert!(seen.insert(w.to_ascii_uppercase()), "{w} appears twice");
            }
        }
        assert_eq!(seen.len(), 11, "ten brand colours and white");
    }
}
