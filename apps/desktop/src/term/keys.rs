//! Keystroke and paste encoding for the person's terminal (spec 1.10). Pure: no GPUI types, so it is tested
//! without a window. The kitty keyboard protocol is off in v1.

use alacritty_terminal::term::TermMode;

/// The modifiers of a keystroke.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Mods {
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
    /// Cmd on macOS, the Windows key elsewhere: never sent, the app's shortcuts.
    pub platform: bool,
}

/// What a keystroke becomes.
#[derive(Debug, PartialEq, Eq)]
pub enum Key {
    /// Bytes for the shell.
    Bytes(Vec<u8>),
    /// Plain text: left to the platform's input method (it composes, then commits UTF-8).
    Text,
    /// Not for the terminal (an app shortcut, a lone modifier).
    Pass,
}

/// xterm modifier parameter: 1 + shift + 2 alt + 4 ctrl.
fn param(m: Mods) -> u8 {
    1 + u8::from(m.shift) + 2 * u8::from(m.alt) + 4 * u8::from(m.ctrl)
}

fn csi(final_byte: char, num: Option<u8>, m: Mods) -> Vec<u8> {
    match (num, param(m)) {
        (None, 1) => format!("\x1b[{final_byte}"),
        (None, p) => format!("\x1b[1;{p}{final_byte}"),
        (Some(n), 1) => format!("\x1b[{n}{final_byte}"),
        (Some(n), p) => format!("\x1b[{n};{p}{final_byte}"),
    }
    .into_bytes()
}

/// `key` is GPUI's key name (`"a"`, `"enter"`, `"up"`, `"f5"`), `key_char` what the key types, `mac` whether
/// Option composes characters (macOS) instead of acting as Meta.
pub fn encode(key: &str, key_char: Option<&str>, m: Mods, mode: TermMode, mac: bool) -> Key {
    let app = mode.contains(TermMode::APP_CURSOR);
    let plain_mods = !m.ctrl && !m.alt && !m.shift;
    let named: Option<Vec<u8>> = match key {
        "enter" if m.alt => Some(b"\x1b\r".to_vec()),
        "enter" => Some(b"\r".to_vec()),
        "escape" => Some(b"\x1b".to_vec()),
        "tab" if m.shift => Some(b"\x1b[Z".to_vec()),
        "tab" => Some(b"\t".to_vec()),
        "backspace" if m.alt => Some(b"\x1b\x7f".to_vec()),
        "backspace" => Some(if m.ctrl {
            b"\x08".to_vec()
        } else {
            b"\x7f".to_vec()
        }),
        "up" | "down" | "left" | "right" | "home" | "end" => {
            let c = match key {
                "up" => 'A',
                "down" => 'B',
                "right" => 'C',
                "left" => 'D',
                "home" => 'H',
                _ => 'F',
            };
            Some(if app && plain_mods {
                format!("\x1bO{c}").into_bytes()
            } else {
                csi(c, None, m)
            })
        }
        "insert" => Some(csi('~', Some(2), m)),
        "delete" => Some(csi('~', Some(3), m)),
        "pageup" => Some(csi('~', Some(5), m)),
        "pagedown" => Some(csi('~', Some(6), m)),
        "f1" | "f2" | "f3" | "f4" => {
            let c = ['P', 'Q', 'R', 'S'][usize::from(key.as_bytes()[1] - b'1')];
            Some(if plain_mods {
                format!("\x1bO{c}").into_bytes()
            } else {
                csi(c, None, m)
            })
        }
        "f5" | "f6" | "f7" | "f8" | "f9" | "f10" | "f11" | "f12" => {
            let n = [15, 17, 18, 19, 20, 21, 23, 24][key[1..].parse::<usize>().unwrap_or(5) - 5];
            Some(csi('~', Some(n), m))
        }
        _ => None,
    };
    if let Some(b) = named {
        return if m.platform { Key::Pass } else { Key::Bytes(b) };
    }
    if m.platform {
        return Key::Pass;
    }
    let one = key.chars().next().filter(|_| key.chars().count() == 1);
    if m.ctrl && !(m.alt && mac) {
        let c = if key == "space" { Some(' ') } else { one };
        let byte = match c {
            Some(c @ 'a'..='z') => Some(c as u8 - b'a' + 1),
            Some(' ' | '@' | '2') => Some(0),
            Some('[' | '3') => Some(0x1b),
            Some('\\' | '4') => Some(0x1c),
            Some(']' | '5') => Some(0x1d),
            Some('^' | '6') => Some(0x1e),
            Some('_' | '-' | '/' | '7') => Some(0x1f),
            Some('8') => Some(0x7f),
            _ => None,
        };
        return match byte {
            Some(b) if m.alt => Key::Bytes(vec![0x1b, b]),
            Some(b) => Key::Bytes(vec![b]),
            None => Key::Pass,
        };
    }
    match key_char.filter(|t| !t.is_empty()) {
        // Option composes characters on macOS; elsewhere Alt is Meta (ESC prefix)
        Some(t) if m.alt && !mac => Key::Bytes([b"\x1b", t.as_bytes()].concat()),
        Some(_) => Key::Text,
        None if key == "space" && !m.alt => Key::Text,
        None => Key::Pass,
    }
}

/// Largest paste taken whole: the write queue holds 16 chunks of 64 KiB.
pub const MAX_PASTE: usize = 512 << 10;

/// The bytes of a paste. A program that asked for bracketed paste gets the text between the markers; ESC is
/// dropped either way, so the pasted text cannot carry the end marker or any other sequence. Without the
/// mode, newlines become Enter and the other controls (but Tab) go too.
pub fn paste(text: &str, mode: TermMode) -> Vec<u8> {
    let bracketed = mode.contains(TermMode::BRACKETED_PASTE);
    let mut body = String::with_capacity(text.len());
    let mut it = text.chars().peekable();
    while let Some(c) = it.next() {
        match c {
            '\r' if it.peek() == Some(&'\n') => {}
            '\n' if !bracketed => body.push('\r'),
            '\n' | '\r' | '\t' => body.push(c),
            c if c.is_control() => {}
            c => body.push(c),
        }
    }
    if bracketed {
        format!("\x1b[200~{body}\x1b[201~").into_bytes()
    } else {
        body.into_bytes()
    }
}

/// A mouse report for a program that enabled mouse reporting (`button`: 0 left, 1 middle, 2 right, 64 wheel up,
/// 65 wheel down; `col` and `row` 0-based). SGR (mode 1006) when asked, else the legacy form, which cannot
/// name a cell past 223 (`None`).
pub fn mouse(
    button: u8,
    press: bool,
    motion: bool,
    col: usize,
    row: usize,
    m: Mods,
    mode: TermMode,
) -> Option<Vec<u8>> {
    let b = button
        + 4 * u8::from(m.shift)
        + 8 * u8::from(m.alt)
        + 16 * u8::from(m.ctrl)
        + 32 * u8::from(motion);
    if mode.contains(TermMode::SGR_MOUSE) {
        let end = if press { 'M' } else { 'm' };
        return Some(format!("\x1b[<{b};{};{}{end}", col + 1, row + 1).into_bytes());
    }
    let b = if press { b } else { b - button + 3 };
    let (x, y) = (u8::try_from(col + 33).ok()?, u8::try_from(row + 33).ok()?);
    (col < 223 && row < 223).then(|| vec![0x1b, b'[', b'M', b + 32, x, y])
}

#[cfg(test)]
mod tests {
    use super::*;

    const NONE: Mods = Mods {
        ctrl: false,
        alt: false,
        shift: false,
        platform: false,
    };
    const CTRL: Mods = Mods { ctrl: true, ..NONE };
    const ALT: Mods = Mods { alt: true, ..NONE };

    fn b(key: &str, ch: Option<&str>, m: Mods, mode: TermMode, mac: bool) -> Vec<u8> {
        match encode(key, ch, m, mode, mac) {
            Key::Bytes(b) => b,
            k => panic!("{key}: {k:?}"),
        }
    }

    #[test]
    fn control_keys() {
        let n = TermMode::NONE;
        assert_eq!(b("c", None, CTRL, n, true), [3]);
        assert_eq!(b("d", Some("\u{4}"), CTRL, n, false), [4]);
        assert_eq!(b("z", None, CTRL, n, true), [26]);
        assert_eq!(b("[", None, CTRL, n, true), [0x1b]);
        assert_eq!(b("space", None, CTRL, n, true), [0]);
        assert_eq!(b("\\", None, CTRL, n, true), [0x1c]);
        assert_eq!(b("enter", None, NONE, n, true), b"\r");
        assert_eq!(b("backspace", None, NONE, n, true), [0x7f]);
        assert_eq!(
            b(
                "tab",
                None,
                Mods {
                    shift: true,
                    ..NONE
                },
                n,
                true
            ),
            b"\x1b[Z"
        );
        assert_eq!(b("escape", None, NONE, n, true), [0x1b]);
        // Ctrl with a key that has no control byte is not the terminal's
        assert_eq!(encode("1", None, CTRL, n, true), Key::Pass);
    }

    #[test]
    fn application_cursor_keys_and_modifiers() {
        let n = TermMode::NONE;
        let app = TermMode::APP_CURSOR;
        assert_eq!(b("up", None, NONE, n, true), b"\x1b[A");
        assert_eq!(b("up", None, NONE, app, true), b"\x1bOA");
        assert_eq!(b("home", None, NONE, app, true), b"\x1bOH");
        // a modified arrow is CSI 1 ; m, also in application mode
        assert_eq!(b("left", None, CTRL, app, true), b"\x1b[1;5D");
        assert_eq!(b("right", None, ALT, n, true), b"\x1b[1;3C");
        assert_eq!(b("pageup", None, NONE, n, true), b"\x1b[5~");
        assert_eq!(
            b(
                "delete",
                None,
                Mods {
                    shift: true,
                    ..NONE
                },
                n,
                true
            ),
            b"\x1b[3;2~"
        );
        assert_eq!(b("f1", None, NONE, n, true), b"\x1bOP");
        assert_eq!(b("f5", None, NONE, n, true), b"\x1b[15~");
        assert_eq!(b("f12", None, CTRL, n, true), b"\x1b[24;5~");
    }

    #[test]
    fn text_goes_to_the_input_method_and_alt_is_meta_off_the_mac() {
        let n = TermMode::NONE;
        assert_eq!(encode("a", Some("a"), NONE, n, true), Key::Text);
        assert_eq!(encode("space", Some(" "), NONE, n, true), Key::Text);
        assert_eq!(
            encode(
                "a",
                Some("A"),
                Mods {
                    shift: true,
                    ..NONE
                },
                n,
                false
            ),
            Key::Text
        );
        assert_eq!(b("b", Some("b"), ALT, n, false), b"\x1bb");
        // Option composes on the mac: the composed character is the input method's
        assert_eq!(encode("b", Some("\u{222b}"), ALT, n, true), Key::Text);
        assert_eq!(b("backspace", None, ALT, n, true), b"\x1b\x7f");
    }

    #[test]
    fn app_chords_are_not_sent() {
        let cmd = Mods {
            platform: true,
            ..NONE
        };
        let n = TermMode::NONE;
        for k in ["c", "v", "1", ",", "left", "enter"] {
            assert_eq!(encode(k, Some(k), cmd, n, true), Key::Pass, "{k}");
        }
    }

    #[test]
    fn mouse_reports() {
        let sgr = TermMode::SGR_MOUSE;
        assert_eq!(
            mouse(0, true, false, 4, 2, NONE, sgr).unwrap(),
            b"\x1b[<0;5;3M"
        );
        assert_eq!(
            mouse(0, false, false, 4, 2, NONE, sgr).unwrap(),
            b"\x1b[<0;5;3m"
        );
        assert_eq!(
            mouse(65, true, false, 0, 0, CTRL, sgr).unwrap(),
            b"\x1b[<81;1;1M"
        );
        assert_eq!(
            mouse(0, true, true, 0, 0, NONE, sgr).unwrap(),
            b"\x1b[<32;1;1M"
        );
        let legacy = TermMode::NONE;
        assert_eq!(
            mouse(0, true, false, 0, 0, NONE, legacy).unwrap(),
            [0x1b, b'[', b'M', 32, 33, 33]
        );
        assert_eq!(
            mouse(0, false, false, 0, 0, NONE, legacy).unwrap(),
            [0x1b, b'[', b'M', 35, 33, 33]
        );
        assert!(mouse(0, true, false, 300, 0, NONE, legacy).is_none());
    }

    #[test]
    fn bracketed_paste_wraps_and_cannot_carry_the_end_marker() {
        let on = TermMode::BRACKETED_PASTE;
        assert_eq!(paste("ls\npwd", on), b"\x1b[200~ls\npwd\x1b[201~");
        // an ESC in the text would close the bracket early: it is dropped
        let evil = paste("x\x1b[201~rm -rf /", on);
        assert_eq!(evil, b"\x1b[200~x[201~rm -rf /\x1b[201~");
        assert_eq!(evil.windows(6).filter(|w| *w == b"\x1b[201~").count(), 1);
    }

    #[test]
    fn plain_paste_sends_enter_for_newlines_and_drops_controls() {
        let off = TermMode::NONE;
        assert_eq!(paste("a\r\nb\nc\td\x03e\x1b[A", off), b"a\rb\rc\tde[A");
    }
}
