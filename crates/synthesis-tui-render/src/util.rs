//! Small JS-compatible helpers: `Number.prototype.toFixed`, UTF-16 string lengths and the number
//! / duration formatters shared by the panels.

/// `x.toFixed(d)`: exact decimal rounding with ties going to the larger value (Rust's `{:.d}`
/// rounds ties to even).
pub fn to_fixed(x: f64, d: usize) -> String {
    if !x.is_finite() {
        return if x.is_nan() {
            "NaN".into()
        } else if x > 0.0 {
            "Infinity".into()
        } else {
            "-Infinity".into()
        };
    }
    let x = if x == 0.0 { 0.0 } else { x }; // (-0).toFixed() has no sign
    let scaled = x * 10f64.powi(d as i32);
    // an exact tie is representable (a dyadic rational), so the fraction test is exact
    if scaled.is_finite() && scaled.abs() < 1e15 && (scaled - scaled.floor()) == 0.5 {
        let n = scaled.floor() + 1.0; // the larger of the two candidates
        let neg = n < 0.0;
        let digits = format!("{}", n.abs() as u64);
        let digits = if digits.len() <= d {
            format!("{}{}", "0".repeat(d + 1 - digits.len()), digits)
        } else {
            digits
        };
        let (int, frac) = digits.split_at(digits.len() - d);
        let body = if d == 0 {
            int.to_string()
        } else {
            format!("{int}.{frac}")
        };
        if neg { format!("-{body}") } else { body }
    } else {
        format!("{x:.d$}")
    }
}

/// `String(Math.round(n))` for the magnitudes we format.
pub fn round_str(n: f64) -> String {
    let r = (n + 0.5).floor();
    if r == 0.0 {
        return "0".into();
    }
    format!("{r}")
}

pub fn u16len(s: &str) -> usize {
    s.chars().map(char::len_utf16).sum()
}

/// `s.padEnd(n)` (UTF-16 length, like JS).
pub fn pad_end(s: &str, n: usize) -> String {
    let l = u16len(s);
    format!("{s}{}", " ".repeat(n.saturating_sub(l)))
}

/// `s.padStart(n)`.
pub fn pad_start(s: &str, n: usize) -> String {
    let l = u16len(s);
    format!("{}{s}", " ".repeat(n.saturating_sub(l)))
}

/// `s.slice(0, n)` counted in UTF-16 units; a cut inside a surrogate pair drops the pair.
pub fn slice_u16(s: &str, n: usize) -> &str {
    let mut used = 0;
    for (i, c) in s.char_indices() {
        let l = c.len_utf16();
        if used + l > n {
            return &s[..i];
        }
        used += l;
    }
    s
}

/// Collapse every run of whitespace to one space and trim (`s.replace(/\s+/g,' ').trim()`).
pub fn one_line(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for w in s
        .split(synthesis_tui_base::text::is_js_space)
        .filter(|w| !w.is_empty())
    {
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(w);
    }
    out
}

/// parser.js `short`.
pub fn short(text: &str, max: usize) -> String {
    let s = one_line(text);
    if u16len(&s) > max {
        format!("{}…", slice_u16(&s, max.saturating_sub(1)))
    } else {
        s
    }
}

/// `String(p).split(/[\\/]/).filter(Boolean).pop() || ''`.
pub fn basename(p: &str) -> String {
    p.split(['\\', '/'])
        .rfind(|s| !s.is_empty())
        .unwrap_or("")
        .to_string()
}

pub fn clamp_int(v: i64, lo: i64, hi: i64) -> i64 {
    v.min(hi).max(lo)
}

/// render.js `fmtNum`.
pub fn fmt_num(n: Option<f64>) -> String {
    let Some(n) = n else { return "-".into() };
    if n >= 1e9 {
        format!("{}B", to_fixed(n / 1e9, 2))
    } else if n >= 1e6 {
        format!("{}M", to_fixed(n / 1e6, 2))
    } else if n >= 1e3 {
        format!("{}k", to_fixed(n / 1e3, 1))
    } else {
        round_str(n)
    }
}

pub fn fmt_n(n: u64) -> String {
    fmt_num(Some(n as f64))
}

/// render.js `fmtDur`.
pub fn fmt_dur(ms: Option<f64>) -> String {
    let Some(ms) = ms.filter(|m| *m >= 0.0) else {
        return "-".into();
    };
    let s = (ms / 1000.0).floor() as u64;
    if s < 60 {
        return format!("{s}s");
    }
    let m = s / 60;
    if m < 60 {
        return format!("{m}m{:02}s", s % 60);
    }
    let h = m / 60;
    if h < 24 {
        return format!("{h}h{:02}m", m % 60);
    }
    format!("{}d{}h", h / 24, h % 24)
}

pub fn fmt_dur_ms(ms: i64) -> String {
    fmt_dur(Some(ms as f64))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn to_fixed_matches_js() {
        assert_eq!(to_fixed(2.5, 0), "3");
        assert_eq!(to_fixed(0.5, 0), "1");
        assert_eq!(to_fixed(1.005, 2), "1.00"); // 1.005 is below the tie in binary
        assert_eq!(to_fixed(0.125, 2), "0.13");
        assert_eq!(to_fixed(-0.001, 2), "-0.00");
        assert_eq!(to_fixed(-0.0, 1), "0.0");
        assert_eq!(to_fixed(1500.0 / 1e3, 1), "1.5");
        assert_eq!(to_fixed(113.0, 0), "113");
    }

    #[test]
    fn short_counts_utf16() {
        assert_eq!(short("  a   b ", 10), "a b");
        assert_eq!(short("abcdef", 4), "abc…");
        assert_eq!(basename("C:\\x\\y.json"), "y.json");
    }
}
