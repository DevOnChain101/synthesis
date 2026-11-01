//! Palette, gradient helpers and width-aware `fit` (port of `theme.js`; widths come from
//! `synthesis_tui_base::text`). Every colour is 24-bit: like Node, the renderers never consult the
//! colour mode, `synthesis_tui_base::sgr::encode` downgrades at output time.

use synthesis_tui_base::text::{cluster_width, csi_len, strip_ansi, width};
use unicode_segmentation::UnicodeSegmentation;

pub type Rgb = [u8; 3];

pub const PURPLE: Rgb = [192, 132, 252];
pub const BLUE: Rgb = [96, 165, 250];
pub const GREEN: Rgb = [74, 222, 128];
pub const YELLOW: Rgb = [250, 204, 21];
pub const ORANGE: Rgb = [251, 146, 60];
pub const PINK: Rgb = [244, 114, 182];
pub const CYAN: Rgb = [34, 211, 238];
pub const TEAL: Rgb = [45, 212, 191];
pub const RED: Rgb = [248, 113, 113];

pub const ACCENT_CORE: Rgb = [129, 140, 248];
pub const ACCENT_LIGHT: Rgb = [165, 180, 252];
pub const ACCENT_GLOW: Rgb = [199, 210, 254];
pub const ACCENT_DEEP: Rgb = [79, 70, 229];
pub const ACCENT_CYAN: Rgb = [103, 232, 249];
pub const ACCENT_INK: Rgb = [17, 19, 38];

pub const WARM_CREAM: Rgb = [250, 250, 250];
pub const WARM_DIM: Rgb = [98, 98, 106];
pub const WARM_DEEP: Rgb = [58, 58, 64];

pub const T_CLAUDE: Rgb = ACCENT_CORE;
pub const T_SHIMMER: Rgb = ACCENT_LIGHT;
pub const T_TEXT: Rgb = [245, 245, 245];
pub const T_BODY: Rgb = [215, 215, 220];
pub const T_INACTIVE: Rgb = [165, 165, 172];
pub const T_SUBTLE: Rgb = [112, 112, 120];
pub const T_BORDER: Rgb = WARM_DIM;
pub const T_PERMISSION: Rgb = CYAN;
pub const T_AUTO_ACCEPT: Rgb = PURPLE;
pub const T_PLAN_MODE: Rgb = GREEN;
pub const T_MCP: Rgb = BLUE;
pub const T_BASH: Rgb = PINK;
pub const T_SUCCESS: Rgb = GREEN;
pub const T_ERROR: Rgb = RED;
pub const T_WARNING: Rgb = YELLOW;
pub const T_TRACK: Rgb = WARM_DEEP;

pub const HEAT: [Rgb; 5] = [
    [22, 27, 34],
    [14, 68, 41],
    [0, 109, 50],
    [38, 166, 65],
    [57, 211, 83],
];
pub const SELECT_BG: Rgb = [60, 72, 105];
pub const SELECT_FG: Rgb = [245, 245, 245];
pub const BORDER_GRAD: [Rgb; 5] = [
    ACCENT_DEEP,
    ACCENT_CORE,
    ACCENT_CYAN,
    ACCENT_CORE,
    ACCENT_DEEP,
];
pub const LOGO_GRAD: [Rgb; 4] = [ACCENT_LIGHT, ACCENT_CYAN, [196, 181, 253], ACCENT_LIGHT];
pub const AGENT_COLORS: [Rgb; 8] = [PURPLE, BLUE, GREEN, YELLOW, TEAL, PINK, CYAN, RED];

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
