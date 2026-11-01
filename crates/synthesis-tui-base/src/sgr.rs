//! Output encoder for rendered lines (port of `sgr.js`): colour/attribute codes are only emitted
//! when they change, blank cells skip foreground changes, and 24-bit colours are downgraded for
//! terminals without truecolor.
//!
//! Additions over Node: [`ColorMode::Ansi16`] and [`ColorMode::Mono`] (NO_COLOR); detection reads
//! only the injected [`Env`], never the process environment. An incomplete `38;2;...` or
//! `38;5;...` at the end of a parameter list is ignored (Node would emit `undefined`).

use crate::Platform;
use synthesis_config::Env;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ColorMode {
    TrueColor,
    Ansi256,
    Ansi16,
    /// NO_COLOR: foreground/background colours are dropped, attributes stay.
    Mono,
}

impl ColorMode {
    /// Terminal colour depth from the environment.
    ///
    /// Order: `CLAUDE_MONITOR_COLOR` (truecolor|24bit|256|16, the Node override) beats
    /// `NO_COLOR` (non-empty value, <https://no-color.org>) beats `COLORTERM`
    /// (truecolor|24bit), `TERM_PROGRAM`, `WT_SESSION`, `TERM`; a basic `TERM` (linux, ansi,
    /// vt100, xterm, screen, ...) gives 16 colours; otherwise truecolor on Windows, 256 elsewhere.
    pub fn detect(env: &Env, platform: Platform) -> Self {
        let lower = |name| env.get_nonempty(name).map(str::to_lowercase);
        match lower("CLAUDE_MONITOR_COLOR").as_deref() {
            Some("truecolor" | "24bit") => return Self::TrueColor,
            Some("256") => return Self::Ansi256,
            Some("16") => return Self::Ansi16,
            _ => {}
        }
        if env.get_nonempty("NO_COLOR").is_some() {
            return Self::Mono;
        }
        if matches!(lower("COLORTERM").as_deref(), Some("truecolor" | "24bit")) {
            return Self::TrueColor;
        }
        let program = env.get("TERM_PROGRAM");
        if program == Some("Apple_Terminal") {
            return Self::Ansi256;
        }
        if env.get_nonempty("WT_SESSION").is_some() // Windows Terminal
            || matches!(
                program,
                Some("iTerm.app" | "vscode" | "WezTerm" | "ghostty" | "Hyper" | "Tabby" | "rio")
            )
        {
            return Self::TrueColor;
        }
        let term = env.get("TERM").unwrap_or("");
        if [
            "xterm-kitty",
            "xterm-ghostty",
            "alacritty",
            "foot",
            "wezterm",
        ]
        .iter()
        .any(|p| term.starts_with(p))
        {
            return Self::TrueColor;
        }
        if matches!(
            term,
            "linux" | "ansi" | "vt100" | "vt220" | "xterm" | "screen" | "rxvt" | "cons25"
        ) {
            return Self::Ansi16;
        }
        if platform == Platform::Windows {
            Self::TrueColor
        } else {
            Self::Ansi256
        }
    }
}

/// A colour: 24-bit components (Node key `"r;g;b"`) or a palette index (`"#n"`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Colour {
    Rgb(u32, u32, u32),
    Indexed(u32),
}

impl std::fmt::Display for Colour {
    /// Node's colour key.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Colour::Rgb(r, g, b) => write!(f, "{r};{g};{b}"),
            Colour::Indexed(n) => write!(f, "#{n}"),
        }
    }
}

pub const ITALIC: u8 = 1;
pub const UNDERLINE: u8 = 2;
pub const INVERSE: u8 = 4;
pub const STRIKE: u8 = 8;
const ATTR_CODES: [(u8, &str); 4] = [
    (ITALIC, "3"),
    (UNDERLINE, "4"),
    (INVERSE, "7"),
    (STRIKE, "9"),
];

/// `int`: 0 normal, 1 bold, 2 dim. `attr`: ITALIC | UNDERLINE | INVERSE | STRIKE.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SgrState {
    pub fg: Option<Colour>,
    pub bg: Option<Colour>,
    pub int: u8,
    pub attr: u8,
}

const CUBE: [i64; 6] = [0, 95, 135, 175, 215, 255];

fn dist(a: (i64, i64, i64), b: (i64, i64, i64)) -> i64 {
    (a.0 - b.0).pow(2) + (a.1 - b.1).pow(2) + (a.2 - b.2).pow(2)
}

/// Nearest xterm-256 index: 6x6x6 cube (16-231) or the grey ramp (232-255).
pub fn to256(r: u32, g: u32, b: u32) -> u32 {
    let (r, g, b) = (i64::from(r), i64::from(g), i64::from(b));
    let level = |v: i64| -> usize {
        if v < 48 {
            0
        } else if v < 115 {
            1
        } else {
            (((v - 35) / 40) as usize).min(5)
        }
    };
    let (ri, gi, bi) = (level(r), level(g), level(b));
    let cube_d = dist((r, g, b), (CUBE[ri], CUBE[gi], CUBE[bi]));
    let avg = (r + g + b) as f64 / 3.0;
    let gi2 = if avg > 238.0 {
        23
    } else {
        ((avg - 8.0) / 10.0 + 0.5).floor().max(0.0) as i64
    };
    let gv = 8 + gi2 * 10;
    if dist((r, g, b), (gv, gv, gv)) < cube_d {
        232 + gi2 as u32
    } else {
        (16 + 36 * ri + 6 * gi + bi) as u32
    }
}

const ANSI16: [(i64, i64, i64); 16] = [
    (0, 0, 0),
    (205, 0, 0),
    (0, 205, 0),
    (205, 205, 0),
    (0, 0, 238),
    (205, 0, 205),
    (0, 205, 205),
    (229, 229, 229),
    (127, 127, 127),
    (255, 0, 0),
    (0, 255, 0),
    (255, 255, 0),
    (92, 92, 255),
    (255, 0, 255),
    (0, 255, 255),
    (255, 255, 255),
];

fn to16(r: i64, g: i64, b: i64) -> u32 {
    (0..16)
        .min_by_key(|&i| dist((r, g, b), ANSI16[i]))
        .expect("non-empty") as u32
}

fn indexed_rgb(n: u32) -> (i64, i64, i64) {
    match n {
        0..=15 => ANSI16[n as usize],
        16..=231 => {
            let n = (n - 16) as usize;
            (CUBE[n / 36], CUBE[n / 6 % 6], CUBE[n % 6])
        }
        _ => {
            let v = 8 + i64::from(n.min(255) - 232) * 10;
            (v, v, v)
        }
    }
}

/// SGR parameter for palette colour `n` (< 16) in the 16-colour set; `base` is 38 or 48.
fn short16(n: u32, base: u8) -> String {
    let (lo, hi) = if base == 38 { (30, 90) } else { (40, 100) };
    if n < 8 {
        format!("{}", lo + n)
    } else {
        format!("{}", hi + n - 8)
    }
}

fn colour_code(c: Colour, base: u8, mode: ColorMode) -> String {
    match (c, mode) {
        (Colour::Indexed(n), ColorMode::Ansi16) if n < 16 => short16(n, base),
        (Colour::Indexed(n), ColorMode::Ansi16) => {
            let (r, g, b) = indexed_rgb(n);
            short16(to16(r, g, b), base)
        }
        (Colour::Indexed(n), _) => format!("{base};5;{n}"),
        (Colour::Rgb(r, g, b), ColorMode::TrueColor) => format!("{base};2;{r};{g};{b}"),
        (Colour::Rgb(r, g, b), ColorMode::Ansi16) => {
            short16(to16(r.into(), g.into(), b.into()), base)
        }
        (Colour::Rgb(r, g, b), _) => format!("{base};5;{}", to256(r, g, b)),
    }
}

/// Apply one SGR parameter list (the part between `ESC[` and `m`) to `st`.
pub fn apply_sgr(st: &mut SgrState, params: &str) {
    // JS Number(): "" is 0; anything else here is digits (saturated), or garbage that matches nothing.
    let num = |s: &str| -> u32 {
        if s.is_empty() {
            0
        } else {
            s.parse().unwrap_or(u32::MAX)
        }
    };
    let p: Vec<u32> = if params.is_empty() {
        vec![0]
    } else {
        params.split(';').map(num).collect()
    };
    let mut i = 0;
    while i < p.len() {
        let v = p[i];
        match v {
            0 => *st = SgrState::default(),
            1 => st.int = 1,
            2 => st.int = 2,
            22 => st.int = 0,
            3 => st.attr |= ITALIC,
            23 => st.attr &= !ITALIC,
            4 => st.attr |= UNDERLINE,
            24 => st.attr &= !UNDERLINE,
            7 => st.attr |= INVERSE,
            27 => st.attr &= !INVERSE,
            9 => st.attr |= STRIKE,
            29 => st.attr &= !STRIKE,
            39 => st.fg = None,
            49 => st.bg = None,
            38 | 48 => {
                let colour = match p.get(i + 1) {
                    Some(2) => match p.get(i + 2..i + 5) {
                        Some(&[r, g, b]) => {
                            i += 4;
                            Some(Colour::Rgb(r, g, b))
                        }
                        _ => {
                            i = p.len();
                            None
                        }
                    },
                    Some(5) => match p.get(i + 2) {
                        Some(&n) => {
                            i += 2;
                            Some(Colour::Indexed(n))
                        }
                        None => {
                            i = p.len();
                            None
                        }
                    },
                    _ => None,
                };
                if v == 38 {
                    st.fg = colour
                } else {
                    st.bg = colour
                }
            }
            30..=37 => st.fg = Some(Colour::Indexed(v - 30)),
            90..=97 => st.fg = Some(Colour::Indexed(v - 82)),
            40..=47 => st.bg = Some(Colour::Indexed(v - 40)),
            100..=107 => st.bg = Some(Colour::Indexed(v - 92)),
            _ => {}
        }
        i += 1;
    }
}

/// Codes that move the terminal from `cur` to `want` for the next cell; updates `cur`.
fn transition(cur: &mut SgrState, want: &SgrState, ch: char, mode: ColorMode) -> String {
    // on a blank cell only the background is visible, unless an attribute
    // (inverse, underline, strike) makes the foreground show
    let blank = ch == ' ' && want.attr & (INVERSE | UNDERLINE | STRIKE) == 0;
    let fg = if blank { cur.fg } else { want.fg };
    let int = if blank { cur.int } else { want.int };
    let mut codes: Vec<String> = Vec::new();
    if want.attr != cur.attr {
        // attributes changed: reset and rebuild the whole state
        codes.push("0".into());
        codes.extend(
            ATTR_CODES
                .iter()
                .filter(|(bit, _)| want.attr & bit != 0)
                .map(|(_, c)| (*c).to_string()),
        );
        if want.int != 0 {
            codes.push(want.int.to_string());
        }
        if let Some(c) = want.fg {
            codes.push(colour_code(c, 38, mode));
        }
        if let Some(c) = want.bg {
            codes.push(colour_code(c, 48, mode));
        }
        *cur = SgrState {
            fg: want.fg,
            bg: want.bg,
            int: want.int,
            attr: want.attr,
        };
        return format!("\x1b[{}m", codes.join(";"));
    }
    if int != cur.int {
        if int == 0 || cur.int != 0 {
            codes.push("22".into());
        }
        if int != 0 {
            codes.push(int.to_string());
        }
        cur.int = int;
    }
    if fg != cur.fg {
        codes.push(fg.map_or_else(|| "39".into(), |c| colour_code(c, 38, mode)));
        cur.fg = fg;
    }
    if want.bg != cur.bg {
        codes.push(
            want.bg
                .map_or_else(|| "49".into(), |c| colour_code(c, 48, mode)),
        );
        cur.bg = want.bg;
    }
    if codes.is_empty() {
        String::new()
    } else {
        format!("\x1b[{}m", codes.join(";"))
    }
}

/// `ESC [ [0-9;]* m` at byte `i`: (parameters, length).
fn sgr_at(line: &str, i: usize) -> Option<(&str, usize)> {
    let b = line.as_bytes();
    if b.get(i + 1) != Some(&b'[') {
        return None;
    }
    let mut j = i + 2;
    while matches!(b.get(j), Some(b'0'..=b'9' | b';')) {
        j += 1;
    }
    (b.get(j) == Some(&b'm')).then(|| (&line[i + 2..j], j + 1 - i))
}

/// Re-encode a line of text with SGR sequences so codes are only sent when they change. SGR is
/// the only escape sequence that comes out; other controls are replaced by visible symbols.
pub fn encode(line: &str, mode: ColorMode) -> String {
    let mut cur = SgrState::default();
    let mut want = SgrState::default();
    let mut out = String::with_capacity(line.len());
    let mut i = 0;
    while i < line.len() {
        if line.as_bytes()[i] == 0x1b
            && let Some((params, n)) = sgr_at(line, i)
        {
            apply_sgr(&mut want, params);
            if mode == ColorMode::Mono {
                want.fg = None;
                want.bg = None;
            }
            i += n;
            continue;
        }
        let orig = line[i..].chars().next().expect("in bounds");
        // The renderer only ever produces SGR. Any other control (ESC of OSC 52 / queries / DCS,
        // C1 0x9b 0x9d, CR, BS, newline) in item text is shown, not sent to the terminal.
        let ch = match orig {
            '\x7f' => '\u{2421}',
            c if c < ' ' => char::from_u32(0x2400 + c as u32).expect("control picture"),
            '\u{80}'..='\u{9f}'
            | '\u{61c}'
            | '\u{200e}'
            | '\u{200f}'
            | '\u{202a}'..='\u{202e}'
            | '\u{2066}'..='\u{2069}' => '\u{fffd}',
            c => c,
        };
        out.push_str(&transition(&mut cur, &want, ch, mode));
        out.push(ch);
        i += orig.len_utf8();
    }
    if cur != SgrState::default() {
        out.push_str("\x1b[0m");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(pairs: &[(&str, &str)]) -> Env {
        Env::from_pairs(pairs.iter().copied())
    }

    #[test]
    fn detect_ignores_process_env_and_follows_standards() {
        let d = |p: &[(&str, &str)]| ColorMode::detect(&env(p), Platform::Linux);
        assert_eq!(d(&[]), ColorMode::Ansi256);
        assert_eq!(d(&[("NO_COLOR", "1")]), ColorMode::Mono);
        assert_eq!(d(&[("NO_COLOR", "")]), ColorMode::Ansi256); // empty counts as unset
        assert_eq!(
            d(&[("NO_COLOR", "1"), ("CLAUDE_MONITOR_COLOR", "256")]),
            ColorMode::Ansi256
        );
        assert_eq!(
            d(&[("NO_COLOR", "1"), ("COLORTERM", "truecolor")]),
            ColorMode::Mono
        );
        assert_eq!(d(&[("TERM", "linux")]), ColorMode::Ansi16);
        assert_eq!(d(&[("TERM", "xterm-256color")]), ColorMode::Ansi256);
        assert_eq!(d(&[("CLAUDE_MONITOR_COLOR", "16")]), ColorMode::Ansi16);
    }

    #[test]
    fn sixteen_colour_output() {
        let line = "\x1b[38;2;250;10;10mR\x1b[48;5;12mB\x1b[38;5;200mC";
        assert_eq!(
            encode(line, ColorMode::Ansi16),
            "\x1b[91mR\x1b[104mB\x1b[95mC\x1b[0m"
        );
    }

    #[test]
    fn only_sgr_escapes_survive_encoding() {
        let evil = "a\x1b]52;c;ZXZpbA==\x07\x1b]0;t\x1b\\\x1b]8;;http://x\x07\x1b[?1049h\x1b[6n\x1bP$qm\x1b\\\x1b_G\x1b\\\u{9b}6n\u{9d}0;x\u{9c}\r\n\x08\x7f\u{202e}";
        let out = encode(&format!("\x1b[31m{evil}"), ColorMode::TrueColor);
        let rest = crate::text::strip_ansi(&out); // our own SGR runs
        assert!(
            !rest.chars().any(|c| c.is_control() || c == '\u{202e}'),
            "{out:?}"
        );
        assert!(out.contains("\u{241b}]52;c;ZXZpbA=="));
    }

    #[test]
    fn mono_keeps_attributes_only() {
        assert_eq!(
            encode("\x1b[1;31mx\x1b[0m", ColorMode::Mono),
            "\x1b[1mx\x1b[0m"
        );
    }
}
