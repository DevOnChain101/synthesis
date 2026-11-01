//! Raw-mode stdin bytes to key events (port of `keys.js`).
//!
//! Lengths that Node measures in UTF-16 units (the 4096 paste window, the "more than 2
//! characters" paste heuristic) are measured the same way here. Mouse coordinates saturate at
//! `u32::MAX` instead of becoming a JS double.

macro_rules! named_keys {
    ($($v:ident = $s:literal),* $(,)?) => {
        /// Keys without a payload; `as_str` is the Node event name.
        #[derive(Clone, Copy, Debug, PartialEq, Eq)]
        pub enum Named { $($v),* }
        impl Named {
            pub fn as_str(self) -> &'static str { match self { $(Self::$v => $s),* } }
        }
    };
}

named_keys! {
    Up = "up", Down = "down", Left = "left", Right = "right", Home = "home", End = "end",
    Delete = "delete", PageUp = "pageup", PageDown = "pagedown", ShiftTab = "shift-tab",
    ShiftEnter = "shift-enter", CtrlUp = "ctrl-up", CtrlDown = "ctrl-down",
    CtrlLeft = "ctrl-left", CtrlRight = "ctrl-right", CtrlPageUp = "ctrl-pageup",
    CtrlPageDown = "ctrl-pagedown", AltPageUp = "alt-pageup", AltPageDown = "alt-pagedown",
    AltUp = "alt-up", AltDown = "alt-down", AltLeft = "alt-left", AltRight = "alt-right",
    AltEnter = "alt-enter", AltA = "alt-a", AltV = "alt-v", AltBackspace = "alt-backspace",
    AltDelete = "alt-delete", AltD = "alt-d", AltT = "alt-t",
    CtrlA = "ctrl-a", CtrlC = "ctrl-c", CtrlD = "ctrl-d", CtrlE = "ctrl-e", CtrlJ = "ctrl-j",
    CtrlK = "ctrl-k", CtrlL = "ctrl-l", CtrlN = "ctrl-n", CtrlO = "ctrl-o", CtrlP = "ctrl-p",
    CtrlR = "ctrl-r", CtrlU = "ctrl-u", CtrlV = "ctrl-v", CtrlW = "ctrl-w",
    Enter = "enter", Tab = "tab", Backspace = "backspace", Escape = "escape",
}

use Named::*;

const SEQ: &[(&str, Named)] = &[
    ("\x1b[A", Up),
    ("\x1b[B", Down),
    ("\x1b[C", Right),
    ("\x1b[D", Left),
    ("\x1bOA", Up),
    ("\x1bOB", Down),
    ("\x1bOC", Right),
    ("\x1bOD", Left),
    ("\x1b[H", Home),
    ("\x1b[F", End),
    ("\x1bOH", Home),
    ("\x1bOF", End),
    ("\x1b[1~", Home),
    ("\x1b[4~", End),
    ("\x1b[7~", Home),
    ("\x1b[8~", End),
    ("\x1b[3~", Delete),
    ("\x1b[5~", PageUp),
    ("\x1b[6~", PageDown),
    ("\x1b[Z", ShiftTab),
    ("\x1b[13;2u", ShiftEnter),
    ("\x1b[27;2;13~", ShiftEnter), // xterm modifyOtherKeys
    ("\x1b[1;5A", CtrlUp),
    ("\x1b[1;5B", CtrlDown),
    ("\x1b[1;5C", CtrlRight),
    ("\x1b[1;5D", CtrlLeft),
    ("\x1b[5;5~", CtrlPageUp),
    ("\x1b[6;5~", CtrlPageDown),
    ("\x1b[5;3~", AltPageUp),
    ("\x1b[6;3~", AltPageDown),
    ("\x1b[1;3A", AltUp),
    ("\x1b[1;3B", AltDown),
    ("\x1b[1;3D", AltLeft),
    ("\x1b[1;3C", AltRight),
    ("\x1b\r", AltEnter),
    ("\x1bb", CtrlLeft),        // macOS option+left
    ("\x1bf", CtrlRight),       // macOS option+right
    ("\x1b\x7f", AltBackspace), // alt/option+backspace (also Windows terminals)
    ("\x1b\x08", AltBackspace),
    ("\x1b[127;3u", AltBackspace), // CSI-u alt/option+backspace
    ("\x1b[127;5u", CtrlW),        // CSI-u ctrl+backspace
    ("\x1b[127;9u", CtrlU),        // CSI-u cmd+backspace: delete to line start
    ("\x1b[3;3~", AltDelete),
    ("\x1b[3;9~", AltDelete),
    ("\x1bd", AltD),
    ("\x1b[100;3u", AltD),
    ("\x1ba", AltA),
    ("\x1bv", AltV),
    ("\x1bt", AltT),
    ("\x1b[116;3u", AltT),
    ("\x1b[118;9u", AltV),
    ("\x1b[118;3u", AltV),
];

fn ctrl(c: char) -> Option<Named> {
    Some(match c {
        '\x03' => CtrlC,
        '\x01' => CtrlA,
        '\x04' => CtrlD,
        '\x05' => CtrlE,
        '\x0b' => CtrlK,
        '\x0c' => CtrlL,
        '\x0e' => CtrlN,
        '\x0f' => CtrlO,
        '\x10' => CtrlP,
        '\x16' => CtrlV,
        '\x12' => CtrlR,
        '\x15' => CtrlU,
        '\x17' => CtrlW,
        '\x0a' => CtrlJ, // also shift+enter in some terminals
        '\r' => Enter,
        '\t' => Tab,
        '\x7f' | '\x08' => Backspace,
        _ => return None,
    })
}

const PASTE_START: &str = "\x1b[200~";
const PASTE_END: &str = "\x1b[201~";
const MAX_PARTIAL: usize = 32; // give up buffering runaway garbage
const MAX_STRING_SEQ: usize = 4096; // an unterminated OSC/DCS is held this long, then released
const HOLD_MS: i64 = 50; // ... or this long, whichever comes first
const MAX_PASTE: usize = 1 << 20; // bytes kept of one bracketed paste; the excess is dropped
const PASTE_IDLE_MS: i64 = 2000; // a paste with no end marker and no bytes for this long ends
const PASTE_MAX_MS: i64 = 10_000; // ABSOLUTE cap from paste start: a stream of bytes cannot keep one open
const CTRL_C_GAP_MS: i64 = 100; // a lone ^C this long after the last paste byte is the user, not payload
const PASTE_GRACE_MS: i64 = 50; // bytes right after an end marker are still the paste (forged end)

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Mods {
    pub shift: bool,
    pub alt: bool,
    pub ctrl: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MouseAction {
    Down,
    Up,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WheelDir {
    Up,
    Down,
    Left,
    Right,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Key {
    Named(Named),
    Text(String),
    Paste(String),
    /// SGR mouse press/release; `x`/`y` are 1-based cells.
    Mouse {
        x: u32,
        y: u32,
        button: u32,
        action: MouseAction,
        mods: Mods,
    },
    /// Motion with a button held (mode 1002).
    MouseDrag {
        x: u32,
        y: u32,
        button: u32,
        mods: Mods,
    },
    Wheel {
        dir: WheelDir,
        x: u32,
        y: u32,
        mods: Mods,
    },
    /// Reply to `ESC[6n`.
    CursorPosition {
        row: u32,
        col: u32,
    },
}

impl Key {
    /// Node's event `name`.
    pub fn name(&self) -> &'static str {
        match self {
            Key::Named(n) => n.as_str(),
            Key::Text(_) => "text",
            Key::Paste(_) => "paste",
            Key::Mouse { .. } => "mouse",
            Key::MouseDrag { .. } => "mouse-drag",
            Key::Wheel { dir, .. } => match dir {
                WheelDir::Up => "wheel-up",
                WheelDir::Down => "wheel-down",
                WheelDir::Left => "wheel-left",
                WheelDir::Right => "wheel-right",
            },
            Key::CursorPosition { .. } => "cursor-position",
        }
    }
}

/// Pending bytes of a sequence cut off at the end of a read. One per input stream.
#[derive(Debug, Default)]
pub struct KeyState {
    buf: String,
    /// Injected clock (ms) of the read being parsed; 0 for `parse_keys`.
    pub now: i64,
    /// `buf` is an unterminated `ESC ]|P|_|^|X` string seen since `held_at`.
    held_str: bool,
    held_at: i64,
    /// Inside a bracketed paste: every byte is data until `ESC[201~`, whatever the read size.
    in_paste: bool,
    paste: String,
    /// Tail of the stream that may be the start of an end marker split across reads.
    win: String,
    paste_at: i64,
    paste_start: i64,
    /// Paste-related activity (start, bytes, end, grace swallow, held sequence) since the last
    /// [`KeyState::take_paste_activity`]: the app keeps approval dialogs unarmed after it.
    paste_touched: bool,
    /// Until this time everything is swallowed: it is the rest of a paste that forged its end.
    grace_until: i64,
}

impl KeyState {
    pub fn pending(&self) -> &str {
        &self.buf
    }

    /// True once after any paste activity; bytes swallowed without producing a key count.
    pub fn take_paste_activity(&mut self) -> bool {
        std::mem::take(&mut self.paste_touched)
    }

    /// Something is held that only a later read or the clock can resolve.
    pub fn busy(&self) -> bool {
        self.held_str || self.in_paste
    }
}

/// `parse_keys` over raw bytes: carries an incomplete UTF-8 tail between reads (Node's
/// `setEncoding` does this for it) and replaces invalid bytes with U+FFFD.
#[derive(Debug, Default)]
pub struct KeyParser {
    pub state: KeyState,
    tail: Vec<u8>,
}

impl KeyParser {
    pub fn feed(&mut self, bytes: &[u8]) -> Vec<Key> {
        self.feed_at(bytes, 0)
    }

    /// No bytes, time passed: releases held sequences and ends a stalled paste.
    pub fn tick(&mut self, now: i64) -> Vec<Key> {
        self.feed_at(&[], now)
    }

    pub fn feed_at(&mut self, bytes: &[u8], now: i64) -> Vec<Key> {
        self.state.now = now;
        self.tail.extend_from_slice(bytes);
        let mut text = String::new();
        let mut rest = self.tail.as_slice();
        loop {
            match std::str::from_utf8(rest) {
                Ok(s) => {
                    text.push_str(s);
                    rest = &[];
                    break;
                }
                Err(e) => {
                    let (ok, bad) = rest.split_at(e.valid_up_to());
                    text.push_str(std::str::from_utf8(ok).expect("validated prefix"));
                    match e.error_len() {
                        None => {
                            rest = bad; // incomplete character at the end: wait for more
                            break;
                        }
                        Some(n) => {
                            text.push('\u{fffd}');
                            rest = &bad[n..];
                        }
                    }
                }
            }
        }
        self.tail = rest.to_vec();
        parse_keys(&text, &mut self.state)
    }
}

fn utf16_len(s: &str) -> usize {
    s.encode_utf16().count()
}

/// The text has a line break strictly inside it (`/[^\r\n][\r\n]+[^\r\n]/`).
fn has_inner_newline(s: &str) -> bool {
    s.trim_matches(['\r', '\n']).contains(['\r', '\n'])
}

fn digits(b: &[u8], mut j: usize) -> Option<(u32, usize)> {
    let start = j;
    let mut v: u32 = 0;
    while let Some(d @ b'0'..=b'9') = b.get(j) {
        v = v.saturating_mul(10).saturating_add(u32::from(d - b'0'));
        j += 1;
    }
    (j > start).then_some((v, j))
}

fn expect(b: &[u8], j: usize, c: u8) -> Option<usize> {
    (b.get(j) == Some(&c)).then_some(j + 1)
}

/// `ESC [ < b ; x ; y (M|m)` at `i`.
fn mouse_at(s: &str, i: usize) -> Option<(Key, usize)> {
    let b = s.as_bytes();
    let j = expect(b, expect(b, i, 0x1b)?, b'[')?;
    let (btn, j) = digits(b, expect(b, j, b'<')?)?;
    let (x, j) = digits(b, expect(b, j, b';')?)?;
    let (y, j) = digits(b, expect(b, j, b';')?)?;
    let letter = *b.get(j).filter(|c| matches!(c, b'M' | b'm'))?;
    let mods = Mods {
        shift: btn & 4 != 0,
        alt: btn & 8 != 0,
        ctrl: btn & 16 != 0,
    };
    let base = btn & !(4 | 8 | 16);
    let wheel = |dir| Key::Wheel { dir, x, y, mods };
    let key = match base {
        64 => wheel(WheelDir::Up),
        65 => wheel(WheelDir::Down),
        66 => wheel(WheelDir::Left),
        67 => wheel(WheelDir::Right),
        _ if base & 32 != 0 => Key::MouseDrag {
            x,
            y,
            button: base & 3,
            mods,
        },
        _ => Key::Mouse {
            x,
            y,
            button: base & 3,
            action: if letter == b'M' {
                MouseAction::Down
            } else {
                MouseAction::Up
            },
            mods,
        },
    };
    Some((key, j + 1))
}

/// `ESC [ row ; col R` at `i`.
fn cursor_position_at(s: &str, i: usize) -> Option<(Key, usize)> {
    let b = s.as_bytes();
    let j = expect(b, expect(b, i, 0x1b)?, b'[')?;
    let (row, j) = digits(b, j)?;
    let (col, j) = digits(b, expect(b, j, b';')?)?;
    let j = expect(b, j, b'R')?;
    Some((Key::CursorPosition { row, col }, j))
}

/// `ESC [ [0-9;?]* [ -/]* [@-~]` at `i`: any other CSI, ignored.
fn unknown_csi_len(s: &str, i: usize) -> Option<usize> {
    let b = s.as_bytes();
    let mut j = expect(b, expect(b, i, 0x1b)?, b'[')?;
    while matches!(b.get(j), Some(b'0'..=b'9' | b';' | b'?')) {
        j += 1;
    }
    while matches!(b.get(j), Some(0x20..=0x2f)) {
        j += 1;
    }
    matches!(b.get(j), Some(0x40..=0x7e)).then_some(j + 1 - i)
}

/// `^ESC(\[|O)[0-9;:<=>?]*$`: could still grow into a full sequence.
fn is_partial_escape(rest: &str) -> bool {
    let b = rest.as_bytes();
    matches!(b, [0x1b, b'[' | b'O', tail @ ..] if tail.iter().all(|c| matches!(c, b'0'..=b'9' | b';' | b':' | b'<'..=b'?')))
}

enum StrSeq {
    Done(usize),
    /// The terminator has not arrived yet.
    Held,
    /// Ctrl-C or Esc-Esc inside it: the user is typing, not the terminal replying.
    Broken,
}

/// Bytes of an `ESC ] | P | _ | ^ | X` string (OSC / DCS / APC / PM / SOS: terminal replies such
/// as an OSC 52 clipboard answer) up to and including BEL or `ESC \`.
fn string_seq_len(s: &str, i: usize) -> Option<StrSeq> {
    let b = s.as_bytes();
    let intro = expect(b, i, 0x1b)?;
    let osc = *b
        .get(intro)
        .filter(|c| matches!(c, b']' | b'P' | b'_' | b'^' | b'X'))?
        == b']';
    let mut j = intro + 1;
    while j < b.len() {
        if (osc && b[j] == 0x07) || (b[j] == 0x1b && b.get(j + 1) == Some(&b'\\')) {
            return Some(StrSeq::Done(j + 1 + usize::from(b[j] == 0x1b) - i));
        }
        if b[j] == 0x03 || (b[j] == 0x1b && b.get(j + 1) == Some(&0x1b)) {
            return Some(StrSeq::Broken);
        }
        j += 1;
    }
    Some(StrSeq::Held)
}

/// Bidi controls and zero-width marks: invisible, they can make pasted text read differently
/// from what it is.
fn is_bidi_or_zero_width(c: char) -> bool {
    matches!(c, '\u{200b}'..='\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2060}'..='\u{2064}' | '\u{2066}'..='\u{2069}' | '\u{061c}' | '\u{feff}')
}

/// Pasted text is data: no escape bytes, other C0/C1 controls, bidi marks or paste markers inside
/// it; CRLF becomes LF and the result is capped at 1 MiB. One routine for bracketed paste and
/// clipboard paste.
pub fn clean_paste(p: &str) -> String {
    let mut capped = String::new();
    push_capped(&mut capped, p);
    capped
        .replace(PASTE_START, "")
        .replace(PASTE_END, "")
        .replace("\r\n", "\n")
        .chars()
        .filter(|c| {
            matches!(c, '\n' | '\r' | '\t') || !(c.is_control() || is_bidi_or_zero_width(*c))
        })
        .collect()
}

/// Parse one read. Sequences cut off at the end of `data` stay in `state` for the next call.
/// A bracketed paste is data until its end marker, however many reads it spans; bytes that follow
/// an end marker straight away (a paste that forged its own end) are dropped, never keystrokes.
pub fn parse_keys(data: &str, state: &mut KeyState) -> Vec<Key> {
    parse_raw(data, state)
}

/// Pasted bytes so far, capped; the excess is dropped.
fn push_capped(dst: &mut String, src: &str) {
    let mut n = MAX_PASTE.saturating_sub(dst.len()).min(src.len());
    while !src.is_char_boundary(n) {
        n -= 1;
    }
    dst.push_str(&src[..n]);
}

fn end_paste(st: &mut KeyState, grace: bool) -> Key {
    st.in_paste = false;
    st.paste_touched = true;
    st.win.clear();
    if grace {
        st.grace_until = st.now + PASTE_GRACE_MS;
    }
    Key::Paste(clean_paste(&std::mem::take(&mut st.paste)))
}

/// Feed paste bytes; `Some` when the end marker is in them. The last marker of a read ends the
/// paste: an earlier one is the paste's own payload.
fn paste_step(data: &str, st: &mut KeyState) -> Option<Key> {
    st.paste_at = st.now;
    st.paste_touched = true;
    let all = format!("{}{}", std::mem::take(&mut st.win), data);
    if let Some(p) = all.rfind(PASTE_END) {
        push_capped(&mut st.paste, &all[..p]);
        return Some(end_paste(st, true));
    }
    let keep = (1..PASTE_END.len())
        .rev()
        .find(|&n| all.as_bytes().ends_with(&PASTE_END.as_bytes()[..n]))
        .unwrap_or(0);
    let cut = all.len() - keep;
    push_capped(&mut st.paste, &all[..cut]);
    st.win = all[cut..].to_string();
    None
}

fn parse_raw(data: &str, st: &mut KeyState) -> Vec<Key> {
    if st.in_paste {
        if st.now - st.paste_at >= PASTE_IDLE_MS {
            // no end marker: leave paste mode, keep what arrived as paste data, never as keys
            let k = end_paste(st, false);
            return [k].into_iter().chain(parse_plain(data, st)).collect();
        }
        if data == "\x03" && st.now - st.paste_at >= CTRL_C_GAP_MS {
            // ^C on its own, after the paste went quiet: the user breaks an unterminated paste.
            // (Paste payload is data, so a ^C that arrives with or right after paste bytes is not.)
            let k = end_paste(st, true);
            return vec![k, Key::Named(CtrlC)];
        }
        if st.now - st.paste_start >= PASTE_MAX_MS {
            // too long however busy the stream: what arrived is the paste, then it is over
            let k = paste_step(data, st).unwrap_or_else(|| end_paste(st, true));
            return vec![k];
        }
        return if data.is_empty() {
            vec![]
        } else {
            paste_step(data, st).into_iter().collect()
        };
    }
    if st.now < st.grace_until && !data.is_empty() {
        st.paste_touched = true;
        st.grace_until = st.now + PASTE_GRACE_MS;
        return vec![];
    }
    parse_plain(data, st)
}

fn parse_plain(data: &str, state: &mut KeyState) -> Vec<Key> {
    // a string sequence held too long is typing (Alt+<key> then text), not a terminal reply
    if state.held_str && state.now - state.held_at >= HOLD_MS {
        state.buf.drain(..2);
        state.held_str = false;
    }
    let was_held = std::mem::take(&mut state.held_str);
    let s = format!("{}{}", std::mem::take(&mut state.buf), data);
    // Terminals without bracketed paste deliver pasted text in one chunk; a newline in the
    // middle of it would otherwise submit the prompt early.
    if !s.contains('\x1b') && utf16_len(&s) > 2 && has_inner_newline(&s) {
        return vec![Key::Paste(clean_paste(s.trim_end_matches(['\r', '\n'])))];
    }
    let mut keys = Vec::new();
    let mut text = String::new();
    fn flush(keys: &mut Vec<Key>, text: &mut String) {
        if !text.is_empty() {
            keys.push(Key::Text(std::mem::take(text)));
        }
    }
    let mut i = 0;
    while i < s.len() {
        if s[i..].starts_with(PASTE_START) {
            flush(&mut keys, &mut text);
            state.in_paste = true;
            state.paste_start = state.now;
            if let Some(k) = paste_step(&s[i + PASTE_START.len()..], state) {
                keys.push(k); // the rest of this read is dropped: see `parse_keys`
            }
            return keys;
        }
        if s.as_bytes()[i] == 0x1b {
            flush(&mut keys, &mut text);
            if let Some((k, next)) = mouse_at(&s, i).or_else(|| cursor_position_at(&s, i)) {
                keys.push(k);
                i = next;
                continue;
            }
            // longest sequence wins
            if let Some((seq, name)) = SEQ
                .iter()
                .filter(|(seq, _)| s[i..].starts_with(seq))
                .max_by_key(|(seq, _)| seq.len())
            {
                keys.push(Key::Named(*name));
                i += seq.len();
                continue;
            }
            match string_seq_len(&s, i) {
                Some(StrSeq::Done(n)) => {
                    i += n; // a terminal reply, not typing
                    continue;
                }
                Some(StrSeq::Held) if s.len() - i <= MAX_STRING_SEQ => {
                    state.buf = s[i..].to_string();
                    state.held_str = true;
                    state.paste_touched = true;
                    if !was_held {
                        state.held_at = state.now;
                    }
                    return keys;
                }
                Some(StrSeq::Held | StrSeq::Broken) => {
                    i += 2; // release: what follows is typing
                    continue;
                }
                None => {}
            }
            if let Some(n) = unknown_csi_len(&s, i) {
                i += n;
                continue;
            }
            // Nothing matched: an in-progress sequence at the tail is held for the next read
            // instead of being misread as a bare escape key or stray text.
            let rest = &s[i..];
            if rest.len() <= MAX_PARTIAL && is_partial_escape(rest) {
                state.buf = rest.to_string();
                return keys;
            }
            keys.push(Key::Named(Escape));
            i += 1;
            continue;
        }
        let ch = s[i..].chars().next().expect("in bounds");
        if let Some(n) = ctrl(ch) {
            flush(&mut keys, &mut text);
            keys.push(Key::Named(n));
        } else if ch as u32 >= 32 && !('\u{7f}'..='\u{9f}').contains(&ch) {
            text.push(ch);
        }
        i += ch.len_utf8();
    }
    flush(&mut keys, &mut text);
    keys
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(s: &str) -> Vec<String> {
        parse_keys(s, &mut KeyState::default())
            .iter()
            .map(|k| match k {
                Key::Text(t) | Key::Paste(t) => format!("{}:{t}", k.name()),
                k => k.name().to_string(),
            })
            .collect()
    }

    // The tests below are translated from Node test/editor.test.js (keys section).
    #[test]
    fn special_keys_text_paste() {
        assert_eq!(names("ab\r"), ["text:ab", "enter"]);
        assert_eq!(
            names("\x1b[A\x1b[Z\x1b[5~\x1b"),
            ["up", "shift-tab", "pageup", "escape"]
        );
        assert_eq!(
            names("\x1b[200~line1\nline2\x1b[201~"),
            ["paste:line1\nline2"]
        );
        assert_eq!(names("one\ntwo\n"), ["paste:one\ntwo"]);
        assert_eq!(
            names("\x03\x7f\t\x0f\x1ba\x1bv"),
            ["ctrl-c", "backspace", "tab", "ctrl-o", "alt-a", "alt-v"]
        );
        assert_eq!(
            names("\x17\x1b\x7f\x1b\x08\x1b[127;3u\x1b[127;5u"),
            [
                "ctrl-w",
                "alt-backspace",
                "alt-backspace",
                "alt-backspace",
                "ctrl-w"
            ]
        );
        assert_eq!(
            names("\x1b[3;3~\x1bd\x1b[100;3u\x1b[127;9u\x1b[3~"),
            ["alt-delete", "alt-d", "alt-d", "ctrl-u", "delete"]
        );
        assert_eq!(names("xin chào"), ["text:xin chào"]);
    }

    #[test]
    fn clean_paste_is_the_one_sanitiser() {
        assert_eq!(
            clean_paste("a\r\nb\tc\x1b[31m\u{202e}d\u{200b}\0e"),
            "a\nb\tc[31mde"
        );
        let big = "x".repeat(MAX_PASTE + 10);
        assert_eq!(clean_paste(&big).len(), MAX_PASTE);
        // a bracketed paste goes through the same routine
        assert_eq!(names("\x1b[200~a\u{202e}b\x1b[201~"), ["paste:ab"]);
    }

    #[test]
    fn paste_cannot_smuggle_keystrokes() {
        // a payload that carries its own end marker, then commands, then the real end marker
        assert_eq!(
            names("\x1b[200~echo\x1b[201~\nrm -rf /\r\x03\x1b[201~"),
            ["paste:echo\nrm -rf /\r"]
        );
        // the real end marker arrives in a later read: what followed the early one is dropped
        let mut st = KeyState::default();
        let ks = parse_keys("\x1b[200~x\x1b[201~y\r\x03\x1b[A\x1b", &mut st);
        assert_eq!(
            ks,
            [Key::Paste("x".into())],
            "nothing after an early end marker in the same read: no Enter, ctrl-c, arrows, text"
        );
        assert!(parse_keys("\x1b[201~", &mut st).is_empty());
        // control bytes and escapes inside a paste are dropped
        assert_eq!(
            names("\x1b[200~a\x1b]52;c;x\x07b\x1b[6n\u{9b}c\x08\x1b[200~d\x1b[201~"),
            ["paste:a]52;c;xb[6ncd"]
        );
        // a split read keeps the whole payload inside the paste
        let mut st = KeyState::default();
        assert!(parse_keys("\x1b[200~ab\x1b[20", &mut st).is_empty());
        assert_eq!(
            parse_keys("1~cd\x03\x1b[201~", &mut st),
            [Key::Paste("abcd".into())]
        );
    }

    #[test]
    fn terminal_replies_are_not_typing() {
        // OSC 52 clipboard reply (BEL and ST), DCS (DECRQSS), APC, DA, CPR, C1 introducers
        for reply in [
            "\x1b]52;c;ZXZpbA==\x07",
            "\x1b]52;c;ZXZpbA==\x1b\\",
            "\x1bP1$r0m\x1b\\",
            "\x1b_Gi=1;OK\x1b\\",
            "\x1b[?64;1;2c",
            "\u{9b}6n\u{9d}0;x\u{9c}",
        ] {
            let ks = parse_keys(&format!("a{reply}b"), &mut KeyState::default());
            let text: String = ks
                .iter()
                .map(|k| match k {
                    Key::Text(t) => t.clone(),
                    k => format!("<{}>", k.name()),
                })
                .collect();
            assert!(
                !text.contains("<escape>") && !text.contains("52;") && !text.contains("$r"),
                "{reply:?} -> {text:?}"
            );
        }
        // split across reads
        let mut st = KeyState::default();
        assert_eq!(
            parse_keys("a\x1b]52;c;ZXZ", &mut st),
            [Key::Text("a".into())]
        );
        assert!(parse_keys("pbA==", &mut st).is_empty());
        assert_eq!(parse_keys("\x07z", &mut st), [Key::Text("z".into())]);
        // an unsolicited cursor report only ever means "size", never an action
        assert_eq!(
            names("\x1b[5;5R"),
            ["cursor-position"],
            "handled by the app as a size hint only"
        );
    }

    #[test]
    fn mouse_wheel_and_clicks() {
        let mut st = KeyState::default();
        let none = Mods::default();
        assert_eq!(
            parse_keys("\x1b[<64;12;8M", &mut st),
            [Key::Wheel {
                dir: WheelDir::Up,
                x: 12,
                y: 8,
                mods: none
            }]
        );
        assert_eq!(
            parse_keys("\x1b[<68;12;8M", &mut st),
            [Key::Wheel {
                dir: WheelDir::Up,
                x: 12,
                y: 8,
                mods: Mods {
                    shift: true,
                    ..none
                }
            }]
        );
        assert_eq!(
            parse_keys("\x1b[<16;7;9M", &mut st),
            [Key::Mouse {
                x: 7,
                y: 9,
                button: 0,
                action: MouseAction::Down,
                mods: Mods { ctrl: true, ..none }
            }]
        );
        assert_eq!(
            names("hi\x1b[<64;1;1Mthere"),
            ["text:hi", "wheel-up", "text:there"]
        );
    }

    #[test]
    fn cursor_position_report() {
        assert_eq!(
            parse_keys("\x1b[44;160R", &mut KeyState::default()),
            [Key::CursorPosition { row: 44, col: 160 }]
        );
    }

    #[test]
    fn split_mouse_sequence_is_buffered() {
        let mut st = KeyState::default();
        assert_eq!(
            parse_keys("hello\x1b[<65;", &mut st),
            [Key::Text("hello".into())]
        );
        assert_eq!(st.pending(), "\x1b[<65;");
        assert_eq!(
            parse_keys("20;10M", &mut st),
            [Key::Wheel {
                dir: WheelDir::Down,
                x: 20,
                y: 10,
                mods: Mods::default()
            }]
        );
        assert_eq!(parse_keys("\x1b", &mut st), [Key::Named(Escape)]);
    }

    #[test]
    fn bytes_split_inside_a_utf8_character() {
        let mut p = KeyParser::default();
        let smile = "😀".as_bytes();
        assert!(p.feed(&smile[..2]).is_empty());
        assert_eq!(p.feed(&smile[2..]), [Key::Text("😀".into())]);
        assert_eq!(p.feed(b"\xffa"), [Key::Text("\u{fffd}a".into())]);
    }

    fn keys_of(p: &mut KeyParser, data: &[u8], now: i64) -> Vec<Key> {
        p.feed_at(data, now)
    }

    #[test]
    fn huge_paste_in_small_reads_never_becomes_keystrokes() {
        // 3 MiB with Enters, digits and forged end markers, delivered in 4096-byte reads
        let mut payload = String::from("\x1b[200~");
        while payload.len() < 3 << 20 {
            payload.push_str("line one\r1\r2\r3 yes\x1b[201~\r1\n\x03\x1b[A");
        }
        payload.push_str("\x1b[201~");
        for step in [0, 1] {
            let mut p = KeyParser::default();
            let mut all = Vec::new();
            for (n, c) in payload.as_bytes().chunks(4096).enumerate() {
                all.extend(keys_of(&mut p, c, 10 + step * n as i64));
            }
            all.extend(p.tick(10_000));
            assert!(
                all.iter()
                    .all(|k| matches!(k, Key::Paste(t) if t.len() <= MAX_PASTE)),
                "only paste data, never keys: {:?}",
                all.iter().map(Key::name).collect::<Vec<_>>()
            );
            assert!(!p.state.busy());
        }
        // an honest large paste: one Paste, capped at 1 MiB, rest dropped
        let mut p = KeyParser::default();
        let mut all = keys_of(&mut p, b"\x1b[200~", 0);
        for _ in 0..(3 << 20) / 4096 {
            all.extend(keys_of(&mut p, &[b'a'; 4096], 0));
        }
        all.extend(keys_of(&mut p, b"\x1b[201~", 0));
        assert_eq!(all.len(), 1);
        assert!(matches!(&all[0], Key::Paste(t) if t.len() == MAX_PASTE));
    }

    #[test]
    fn a_paste_cannot_stay_open_forever_by_keeping_the_idle_timer_alive() {
        let mut p = KeyParser::default();
        assert!(keys_of(&mut p, b"\x1b[200~start ", 0).is_empty());
        // a byte every second keeps resetting the 2 s idle timer
        for t in 1..10 {
            assert!(keys_of(&mut p, b"x", t * 1000).is_empty(), "t={t}");
            assert!(p.state.busy());
        }
        // the absolute cap: pending bytes come out as Paste data, the paste is over
        assert_eq!(
            keys_of(&mut p, b"tail\r1", PASTE_MAX_MS),
            [Key::Paste("start xxxxxxxxxtail\r1".into())]
        );
        assert!(!p.state.busy());
        // and the rest of that stream is swallowed for the grace window, not typed
        assert!(keys_of(&mut p, b"\r1\r", PASTE_MAX_MS + 10).is_empty());
        assert_eq!(
            keys_of(&mut p, b"\r", PASTE_MAX_MS + 10 + PASTE_GRACE_MS),
            [Key::Named(Enter)]
        );
        // the clock alone also ends it
        let mut p = KeyParser::default();
        keys_of(&mut p, b"\x1b[200~ab", 0);
        for t in 1..10 {
            keys_of(&mut p, b"c", t * 1000);
        }
        assert_eq!(p.tick(PASTE_MAX_MS), [Key::Paste("abccccccccc".into())]);
    }

    #[test]
    fn lone_ctrl_c_breaks_an_unterminated_paste() {
        let mut p = KeyParser::default();
        keys_of(&mut p, b"\x1b[200~echo hi", 0);
        // inside the paste burst a ^C byte is payload (and is stripped like any control)
        assert!(keys_of(&mut p, b"\x03", 1).is_empty() && p.state.busy());
        // a ^C on its own after the paste went quiet is the user
        assert_eq!(
            keys_of(&mut p, b"\x03", 1 + CTRL_C_GAP_MS),
            [Key::Paste("echo hi".into()), Key::Named(CtrlC)]
        );
        assert!(!p.state.busy());
    }

    #[test]
    fn paste_activity_is_reported_once_per_burst() {
        let mut p = KeyParser::default();
        assert!(!p.state.take_paste_activity());
        keys_of(&mut p, b"abc", 0);
        assert!(
            !p.state.take_paste_activity(),
            "typing is not paste activity"
        );
        keys_of(&mut p, b"\x1b[200~a", 0);
        assert!(p.state.take_paste_activity() && !p.state.take_paste_activity());
        keys_of(&mut p, b"b\x1b[201~", 1);
        assert!(p.state.take_paste_activity());
        keys_of(&mut p, b"\r1", 2); // swallowed by the grace window
        assert!(p.state.take_paste_activity());
        keys_of(&mut p, b"\x1bPhello", 1000); // held string
        assert!(p.state.take_paste_activity());
    }

    #[test]
    fn end_marker_split_across_reads_and_stalled_paste() {
        let mut p = KeyParser::default();
        assert!(keys_of(&mut p, b"\x1b[200~ab\x1b[20", 0).is_empty());
        assert_eq!(keys_of(&mut p, b"1~", 5), [Key::Paste("ab".into())]);
        // no end marker: after the idle limit the paste ends as data and typing works again
        let mut p = KeyParser::default();
        assert!(keys_of(&mut p, b"\x1b[200~abc\r1", 0).is_empty());
        assert!(p.tick(PASTE_IDLE_MS - 1).is_empty() && p.state.busy());
        assert_eq!(p.tick(PASTE_IDLE_MS), [Key::Paste("abc\r1".into())]);
        assert!(!p.state.busy());
        assert_eq!(
            keys_of(&mut p, b"\r", PASTE_IDLE_MS + 1),
            [Key::Named(Enter)],
            "typing resumes, buffered bytes are not replayed"
        );
    }

    #[test]
    fn held_string_sequences_release_and_ctrl_c_breaks_out() {
        // Alt+P then text: released after HOLD_MS as text
        let mut p = KeyParser::default();
        assert!(keys_of(&mut p, b"\x1bPhello", 0).is_empty() && p.state.busy());
        assert!(p.tick(HOLD_MS - 1).is_empty());
        assert_eq!(p.tick(HOLD_MS), [Key::Text("hello".into())]);
        assert!(!p.state.busy());
        // also released by the next read once the time is up, and keys work after
        let mut p = KeyParser::default();
        keys_of(&mut p, b"\x1bXab", 0);
        assert_eq!(
            keys_of(&mut p, b"c\x03", 100),
            [Key::Text("abc".into()), Key::Named(CtrlC)]
        );
        // Ctrl-C and Esc-Esc break out at once, even inside a held sequence
        for (inp, want) in [
            (&b"\x1b]52;c;ab\x03"[..], Key::Named(CtrlC)),
            (b"\x1bPab\x1b\x1b", Key::Named(Escape)),
            (b"\x1b]0;t\x07\x1b_zz\x03", Key::Named(CtrlC)),
        ] {
            let ks = keys_of(&mut KeyParser::default(), inp, 0);
            assert!(ks.contains(&want), "{inp:?} -> {ks:?}");
        }
        // longer than the cap: released as text, not held
        let mut p = KeyParser::default();
        let long = format!("\x1b]{}", "a".repeat(MAX_STRING_SEQ + 10));
        assert!(
            matches!(&keys_of(&mut p, long.as_bytes(), 0)[..], [Key::Text(t)] if t.len() > MAX_STRING_SEQ)
        );
        assert!(!p.state.busy());
        // a complete reply is still dropped
        assert!(keys_of(&mut KeyParser::default(), b"\x1b]52;c;x\x07", 0).is_empty());
    }
}
