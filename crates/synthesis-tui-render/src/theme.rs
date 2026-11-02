//! Palette tokens, gradient helpers and width-aware `fit` (port of `theme.js`; widths come from
//! `synthesis_tui_base::text`). Every colour is 24-bit: like Node, the renderers never consult the
//! colour mode, `synthesis_tui_base::sgr::encode` downgrades at output time.

use synthesis_tui_base::text::{cluster_width, csi_len, strip_ansi, width};
use unicode_segmentation::UnicodeSegmentation;

pub type Rgb = [u8; 3];

// ---- The Synthesis palette: graphite/silver neutrals (design/02-design-tokens/color-schema.md)
// plus ONE accent family (teal) and distinct semantic colours. No violet, magenta or indigo
// anywhere: `palette_tests` checks the hue of every token and the contrast of every text token.
// Every colour the renderers use is a token here (the selection pair lives in `tui-base`, which
// cannot depend on this crate, and is re-exported below).

// Neutrals. BG/SURFACE/ELEVATED are the brand grounds; the terminal's own background shows.
pub const BG: Rgb = [11, 12, 15];
pub const SURFACE: Rgb = [21, 23, 27];
pub const ELEVATED: Rgb = [36, 40, 48];
/// Brand border; decorative (rules, tracks), never text.
pub const BORDER: Rgb = [58, 63, 74];
/// A lighter rule for frames that should read on a dark ground; decorative.
pub const RULE: Rgb = [75, 82, 96];
/// Brand `text`.
pub const TEXT: Rgb = [156, 163, 175];
/// Dimmest readable text (hints, placeholders).
pub const MUTED: Rgb = [127, 135, 151];
pub const TEXT_STRONG: Rgb = [229, 231, 235];
/// Brand `accent` (a neutral silver).
pub const SILVER: Rgb = [217, 217, 224];
/// Brand `accent-light`.
pub const PEARL: Rgb = [247, 248, 250];
/// Brand `soft-beige`; also the thinking colour.
pub const BEIGE: Rgb = [215, 206, 199];

// The one accent: teal.
pub const ACCENT: Rgb = [94, 200, 184];
pub const ACCENT_LIGHT: Rgb = [154, 227, 214];
pub const ACCENT_GLOW: Rgb = [205, 243, 236];
pub const ACCENT_DEEP: Rgb = [46, 139, 126];
/// Text on an accent-coloured ground.
pub const ACCENT_INK: Rgb = BG;

// Semantic colours, each distinct from the others and from the accent.
pub const SUCCESS: Rgb = [123, 216, 143];
pub const WARN: Rgb = [229, 181, 103];
pub const ERROR: Rgb = [242, 109, 109];
pub const DIFF_ADD: Rgb = [158, 206, 106];
pub const DIFF_REMOVE: Rgb = [232, 144, 106];
pub const THINKING: Rgb = BEIGE;
pub const INFO: Rgb = [125, 180, 230];
pub const CYAN: Rgb = [108, 207, 224];

// Names the renderers already use.
pub const GREEN: Rgb = SUCCESS;
pub const YELLOW: Rgb = WARN;
pub const RED: Rgb = ERROR;
pub const ORANGE: Rgb = DIFF_REMOVE;
pub const BLUE: Rgb = INFO;
pub const TEAL: Rgb = ACCENT;

pub const WARM_CREAM: Rgb = PEARL;
pub const WARM_DIM: Rgb = RULE;
pub const WARM_DEEP: Rgb = BORDER;

pub const T_CLAUDE: Rgb = ACCENT;
pub const T_SHIMMER: Rgb = ACCENT_LIGHT;
pub const T_TEXT: Rgb = TEXT_STRONG;
pub const T_BODY: Rgb = SILVER;
pub const T_INACTIVE: Rgb = TEXT;
pub const T_SUBTLE: Rgb = MUTED;
pub const T_BORDER: Rgb = RULE;
pub const T_PERMISSION: Rgb = CYAN;
pub const T_AUTO_ACCEPT: Rgb = BEIGE;
pub const T_PLAN_MODE: Rgb = SUCCESS;
pub const T_MCP: Rgb = INFO;
pub const T_BASH: Rgb = DIFF_REMOVE;
pub const T_SUCCESS: Rgb = SUCCESS;
pub const T_ERROR: Rgb = ERROR;
pub const T_WARNING: Rgb = WARN;
pub const T_TRACK: Rgb = BORDER;

pub const HEAT: [Rgb; 5] = [SURFACE, [20, 64, 59], [30, 107, 97], [58, 158, 144], ACCENT];
pub use synthesis_tui_base::selection::{SELECT_BG, SELECT_FG};
pub const BORDER_GRAD: [Rgb; 5] = [ACCENT_DEEP, ACCENT, CYAN, ACCENT, ACCENT_DEEP];
pub const LOGO_GRAD: [Rgb; 4] = [ACCENT_LIGHT, CYAN, ACCENT_GLOW, ACCENT_LIGHT];
/// Sub-agent and context-segment colours (no violet).
pub const AGENT_COLORS: [Rgb; 8] = [BLUE, BEIGE, TEAL, YELLOW, DIFF_REMOVE, CYAN, GREEN, RED];

// Syntax highlighting and inline code.
pub const SYN_FG: Rgb = TEXT_STRONG;
pub const SYN_KEYWORD: Rgb = ACCENT;
pub const SYN_STORAGE: Rgb = CYAN;
pub const SYN_BUILTIN: Rgb = DIFF_ADD;
pub const SYN_LITERAL: Rgb = DIFF_REMOVE;
pub const SYN_STRING: Rgb = WARN;
pub const SYN_COMMENT: Rgb = MUTED;
pub const SYN_VARIABLE: Rgb = PEARL;
pub const CODE: Rgb = CYAN;

pub const RESET: &str = "\x1b[0m";
pub const ITALIC: &str = "\x1b[3m";
/// Highlights change colour only, never weight (Node `BOLD_ON = false`).
pub const BOLD: &str = "";
pub const BOLD_ON: bool = false;

/// `CLAUDE_SPIN`: forward `· ✢ ✶ ✻ ✽`, then the middle three backwards `✻ ✶ ✢`.
pub fn spin(frame: usize) -> &'static str {
    const S: [&str; 8] = ["·", "✢", "✶", "✻", "✽", "✻", "✶", "✢"];
    S[frame % S.len()]
}

pub fn agent_color(kind: &str) -> Rgb {
    let mut h: u32 = 0;
    for ch in kind.chars() {
        h = h.wrapping_mul(31).wrapping_add(ch as u32);
    }
    AGENT_COLORS[(h as usize) % AGENT_COLORS.len()]
}

/// `"…"` that moves: 0 to 3 dots padded to 3 cells, driven by the frame counter.
pub fn dots(frame: usize) -> String {
    let n = (frame / 3) % 4;
    format!("{}{}", ".".repeat(n), " ".repeat(3 - n))
}

/// "Checking config…" -> "Checking config" + moving dots.
pub fn waiting(text: &str, frame: usize) -> String {
    let t = text.trim_end_matches(synthesis_tui_base::text::is_js_space);
    let base = t
        .strip_suffix('…')
        .or_else(|| t.strip_suffix("..."))
        .unwrap_or(text);
    format!("{base}{}", dots(frame))
}

pub fn fg(c: Rgb) -> String {
    format!("\x1b[38;2;{};{};{}m", c[0], c[1], c[2])
}
pub fn bg(c: Rgb) -> String {
    format!("\x1b[48;2;{};{};{}m", c[0], c[1], c[2])
}

/// `Math.round`: halves go up.
pub fn js_round(x: f64) -> f64 {
    (x + 0.5).floor()
}

pub fn lerp(a: Rgb, b: Rgb, t: f64) -> Rgb {
    let mut o = [0u8; 3];
    for i in 0..3 {
        let v = f64::from(a[i]) + (f64::from(b[i]) - f64::from(a[i])) * t;
        o[i] = js_round(v).clamp(0.0, 255.0) as u8;
    }
    o
}

/// Position `t` (wraps around) on a multi-stop gradient.
pub fn cycle(stops: &[Rgb], t: f64) -> Rgb {
    let u = t - t.floor();
    let seg = u * (stops.len() - 1) as f64;
    let i = seg.floor() as usize;
    lerp(
        stops[i],
        stops[(i + 1).min(stops.len() - 1)],
        seg - i as f64,
    )
}

/// Triangle-wave gradient between two colours, for pulsing borders.
pub fn pulse(a: Rgb, b: Rgb, frame: u64, period: u64) -> Rgb {
    let p = (frame % period) as f64 / period as f64;
    lerp(a, b, if p < 0.5 { p * 2.0 } else { 2.0 - p * 2.0 })
}

/// `fg(rgb) + s + RESET`.
pub fn paint(c: Rgb, s: &str) -> String {
    format!("\x1b[38;2;{};{};{}m{s}{RESET}", c[0], c[1], c[2])
}

/// Pad or truncate to exactly `w` cells, keeping colour codes intact and never splitting a
/// grapheme cluster.
pub fn fit(s: &str, w: usize) -> String {
    if w == 0 {
        return String::new();
    }
    let stripped = strip_ansi(s);
    let ascii = stripped.bytes().all(|b| (0x20..=0x7e).contains(&b));
    let total = if ascii { stripped.len() } else { width(s) };
    if total <= w {
        let mut o = String::with_capacity(s.len() + w - total);
        o.push_str(s);
        o.extend(std::iter::repeat_n(' ', w - total));
        return o;
    }
    let budget = w - 1; // one cell for the ellipsis
    let mut out = String::with_capacity(s.len());
    let mut used = 0;
    let mut i = 0;
    'outer: while i < s.len() {
        if let Some(n) = csi_len(s, i) {
            out.push_str(&s[i..i + n]);
            i += n;
            continue;
        }
        let mut j = i;
        while j < s.len() {
            if csi_len(s, j).is_some() {
                break;
            }
            j += s[j..].chars().next().map_or(1, char::len_utf8);
        }
        if ascii {
            for ch in s[i..j].chars() {
                if used + 1 > budget {
                    break 'outer;
                }
                out.push(ch);
                used += 1;
            }
        } else {
            for g in s[i..j].graphemes(true) {
                let cw = cluster_width(g);
                if used + cw > budget {
                    break 'outer;
                }
                out.push_str(g);
                used += cw;
            }
        }
        i = j;
    }
    out.push('…');
    out.push_str(RESET);
    out.extend(std::iter::repeat_n(' ', budget.saturating_sub(used)));
    out
}

/// Joins `(priority, text)` items with `sep` and drops whole items (largest priority number first, the
/// later one on ties) until the line fits `w`, so a word is never cut mid-way (QA B-27). One remaining
/// item that is still too long is ellipsized by `fit`.
pub fn fit_items(items: &[(u8, String)], sep: &str, w: usize) -> String {
    let mut keep = vec![true; items.len()];
    let join = |keep: &[bool]| {
        let parts: Vec<&str> = items
            .iter()
            .zip(keep)
            .filter(|(_, k)| **k)
            .map(|(i, _)| i.1.as_str())
            .collect();
        parts.join(sep)
    };
    while width(&join(&keep)) > w && keep.iter().filter(|k| **k).count() > 1 {
        if let Some(i) = (0..items.len())
            .filter(|i| keep[*i])
            .max_by_key(|i| (items[*i].0, *i))
        {
            keep[i] = false;
        }
    }
    fit(&join(&keep), w)
}

/// A zero-based list `0..n` helper for `' '.repeat(n)`.
pub fn spaces(n: usize) -> String {
    " ".repeat(n)
}

#[cfg(test)]
mod fit_items_tests {
    use super::*;

    #[test]
    fn drops_whole_items_by_priority_and_never_cuts_a_word() {
        let items = [
            (1, "mode on".to_string()),
            (3, "ctx 5%".to_string()),
            (2, "model-x".to_string()),
        ];
        assert_eq!(
            fit_items(&items, " · ", 40).trim_end(),
            "mode on · ctx 5% · model-x"
        );
        assert_eq!(fit_items(&items, " · ", 20).trim_end(), "mode on · model-x");
        assert_eq!(fit_items(&items, " · ", 9).trim_end(), "mode on");
    }
}

/// Name of the one built-in palette (there is no theme switch: Mono and 16-colour terminals are
/// handled by `synthesis_tui_base::sgr::encode`, which downgrades these 24-bit tokens).
pub const THEME_NAME: &str = "synthesis";

/// WCAG relative luminance and contrast ratio, hue/saturation in degrees / 0..1 (for the tests and
/// `docs/tui-palette.md`).
pub mod measure {
    use super::Rgb;

    fn lin(c: u8) -> f64 {
        let c = f64::from(c) / 255.0;
        if c <= 0.03928 {
            c / 12.92
        } else {
            ((c + 0.055) / 1.055).powf(2.4)
        }
    }

    pub fn luminance(c: Rgb) -> f64 {
        0.2126 * lin(c[0]) + 0.7152 * lin(c[1]) + 0.0722 * lin(c[2])
    }

    pub fn contrast(a: Rgb, b: Rgb) -> f64 {
        let (x, y) = (luminance(a), luminance(b));
        (x.max(y) + 0.05) / (x.min(y) + 0.05)
    }

    /// `(hue in degrees, saturation)`, HSV.
    pub fn hue_sat(c: Rgb) -> (f64, f64) {
        let [r, g, b] = c.map(|v| f64::from(v) / 255.0);
        let (mx, mn) = (r.max(g).max(b), r.min(g).min(b));
        let d = mx - mn;
        if d == 0.0 {
            return (0.0, 0.0);
        }
        let h = if mx == r {
            ((g - b) / d).rem_euclid(6.0)
        } else if mx == g {
            (b - r) / d + 2.0
        } else {
            (r - g) / d + 4.0
        };
        (h * 60.0, d / mx)
    }

    /// Violet, indigo or magenta: a hue in 250..=345 that is not a near-grey.
    pub fn is_purple(c: Rgb) -> bool {
        let (h, s) = hue_sat(c);
        s >= 0.12 && (250.0..=345.0).contains(&h)
    }
}

#[cfg(test)]
mod palette_tests {
    use super::measure::*;
    use super::*;

    /// (name, token, readable text on BG/SURFACE)
    fn tokens() -> Vec<(&'static str, Rgb, bool)> {
        vec![
            ("BG", BG, false),
            ("SURFACE", SURFACE, false),
            ("ELEVATED", ELEVATED, false),
            ("BORDER", BORDER, false),
            ("RULE", RULE, false),
            ("TEXT", TEXT, true),
            ("MUTED", MUTED, true),
            ("TEXT_STRONG", TEXT_STRONG, true),
            ("SILVER", SILVER, true),
            ("PEARL", PEARL, true),
            ("BEIGE / THINKING", BEIGE, true),
            ("ACCENT", ACCENT, true),
            ("ACCENT_LIGHT", ACCENT_LIGHT, true),
            ("ACCENT_GLOW", ACCENT_GLOW, true),
            ("ACCENT_DEEP", ACCENT_DEEP, false),
            ("SUCCESS", SUCCESS, true),
            ("WARN", WARN, true),
            ("ERROR", ERROR, true),
            ("DIFF_ADD", DIFF_ADD, true),
            ("DIFF_REMOVE", DIFF_REMOVE, true),
            ("INFO", INFO, true),
            ("CYAN", CYAN, true),
            ("SELECT_BG", SELECT_BG, false),
        ]
    }

    #[test]
    fn no_token_is_violet_indigo_or_magenta() {
        for (n, c, _) in tokens() {
            assert!(!is_purple(c), "{n} {c:?} hue {:?}", hue_sat(c));
        }
        for c in HEAT
            .iter()
            .chain(&BORDER_GRAD)
            .chain(&LOGO_GRAD)
            .chain(&AGENT_COLORS)
        {
            assert!(!is_purple(*c), "{c:?}");
        }
        for c in [
            SYN_FG,
            SYN_KEYWORD,
            SYN_STORAGE,
            SYN_BUILTIN,
            SYN_LITERAL,
            SYN_STRING,
            SYN_COMMENT,
            SYN_VARIABLE,
            CODE,
            T_AUTO_ACCEPT,
            T_BASH,
            T_MCP,
            T_PERMISSION,
            T_PLAN_MODE,
        ] {
            assert!(!is_purple(c), "{c:?}");
        }
    }

    #[test]
    fn text_tokens_reach_4_5_to_1_on_the_brand_grounds() {
        for (n, c, text) in tokens() {
            if text {
                for (gn, g) in [("BG", BG), ("SURFACE", SURFACE)] {
                    let r = contrast(c, g);
                    assert!(r >= 4.5, "{n} on {gn}: {r:.2}");
                }
            }
        }
        // selected text and text on an accent ground
        assert!(contrast(SELECT_FG, SELECT_BG) >= 4.5);
        assert!(contrast(ACCENT_INK, ACCENT) >= 4.5);
        assert!(contrast(ACCENT_INK, ACCENT_LIGHT) >= 4.5);
        // the strong text also holds on the elevated ground
        assert!(contrast(TEXT_STRONG, ELEVATED) >= 4.5);
        assert!(contrast(ACCENT, ELEVATED) >= 4.5);
    }

    #[test]
    fn semantic_colours_are_distinct_from_each_other_and_the_accent() {
        let sem = [
            ("SUCCESS", SUCCESS),
            ("WARN", WARN),
            ("ERROR", ERROR),
            ("DIFF_ADD", DIFF_ADD),
            ("DIFF_REMOVE", DIFF_REMOVE),
            ("THINKING", THINKING),
            ("ACCENT", ACCENT),
        ];
        for (i, (a, x)) in sem.iter().enumerate() {
            for (b, y) in &sem[i + 1..] {
                assert_ne!(x, y, "{a} == {b}");
            }
        }
    }

    /// The 256- and 16-colour downgrade must not turn a token violet or magenta either.
    #[test]
    fn downgrades_stay_out_of_violet() {
        use synthesis_tui_base::sgr::{ColorMode, encode};
        let cube = [0u8, 95, 135, 175, 215, 255];
        let mut all: Vec<Rgb> = tokens().into_iter().map(|(_, c, _)| c).collect();
        all.extend(HEAT.iter().chain(&AGENT_COLORS).chain(&LOGO_GRAD));
        for c in all {
            let s = encode(&paint(c, "x"), ColorMode::Ansi256);
            let n: usize = s
                .split("38;5;")
                .nth(1)
                .and_then(|r| r.split('m').next())
                .and_then(|r| r.parse().ok())
                .unwrap_or_else(|| panic!("{s:?}"));
            let rgb = if (16..232).contains(&n) {
                let k = n - 16;
                [cube[k / 36], cube[k / 6 % 6], cube[k % 6]]
            } else {
                [8 + 10 * (n.saturating_sub(232)) as u8; 3]
            };
            assert!(n >= 16 && !is_purple(rgb), "{c:?} -> 256 index {n} {rgb:?}");
            let s16 = encode(&paint(c, "x"), ColorMode::Ansi16);
            assert!(
                !s16.contains("35m") && !s16.contains("95m"),
                "{c:?} -> {s16:?}"
            );
        }
    }

    /// Prints the table `docs/tui-palette.md` quotes: `cargo test -p synthesis-tui-render palette_table -- --nocapture`.
    #[test]
    fn palette_table() {
        for (n, c, _) in tokens() {
            let (h, s) = hue_sat(c);
            println!(
                "| {n} | #{:02X}{:02X}{:02X} | {h:.0} | {s:.2} | {:.2} | {:.2} |",
                c[0],
                c[1],
                c[2],
                contrast(c, BG),
                contrast(c, SURFACE)
            );
        }
    }
}
