//! Cell width and grapheme handling (port of the width part of `theme.js`).
//!
//! Differences from Node: `Emoji_Presentation` comes from `unicode-properties` (Unicode 17)
//! instead of V8's ICU; grapheme clusters come from `unicode-segmentation` (UAX #29) instead
//! of `Intl.Segmenter`. Both follow the same standard, so they only differ for characters
//! added between the two Unicode versions. `unicode-width` is deliberately not used: Node's own
//! table is what the golden cases pin, and it treats ambiguous-width characters as 1 cell.

use unicode_properties::emoji::{EmojiStatus, UnicodeEmoji};
use unicode_segmentation::UnicodeSegmentation;

const ZWJ: char = '\u{200d}';
const VS15: char = '\u{fe0e}';
const VS16: char = '\u{fe0f}';

/// JavaScript's `\s` (not `char::is_whitespace`: JS has U+FEFF and lacks U+0085).
pub fn is_js_space(c: char) -> bool {
    matches!(
        c,
        '\t' | '\n' | '\u{b}' | '\u{c}' | '\r' | ' ' | '\u{a0}' | '\u{1680}' | '\u{2000}'
            ..='\u{200a}'
                | '\u{2028}'
                | '\u{2029}'
                | '\u{202f}'
                | '\u{205f}'
                | '\u{3000}'
                | '\u{feff}'
    )
}

/// Byte length of the `ESC [ [0-9;?]* [A-Za-z]` sequence starting at byte `i`, if there is one.
pub fn csi_len(s: &str, i: usize) -> Option<usize> {
    let b = s.as_bytes();
    if b.get(i) != Some(&0x1b) || b.get(i + 1) != Some(&b'[') {
        return None;
    }
    let mut j = i + 2;
    while matches!(b.get(j), Some(b'0'..=b'9' | b';' | b'?')) {
        j += 1;
    }
    b.get(j)
        .filter(|c| c.is_ascii_alphabetic())
        .map(|_| j + 1 - i)
}

pub fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < s.len() {
        if let Some(n) = csi_len(s, i) {
            i += n;
            continue;
        }
        let c = s[i..].chars().next().expect("in bounds");
        out.push(c);
        i += c.len_utf8();
    }
    out
}

/// `s` without OSC 8 hyperlink sequences (`ESC ] 8 ; params ; uri ST|BEL`), for text snapshots.
pub fn strip_osc8(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find("\x1b]8;") {
        out.push_str(&rest[..i]);
        let tail = &rest[i..];
        // the sequence ends at BEL or ST (ESC \)
        match tail[1..].find(['\x07', '\x1b'].as_slice()).map(|j| {
            let j = j + 1;
            (j + if tail.as_bytes()[j] == 0x07 { 1 } else { 2 }).min(tail.len())
        }) {
            Some(end) => rest = &tail[end..],
            None => {
                out.push_str(tail);
                return out;
            }
        }
    }
    out.push_str(rest);
    out
}

fn is_emoji_presentation(c: char) -> bool {
    matches!(
        c.emoji_status(),
        EmojiStatus::EmojiPresentation
            | EmojiStatus::EmojiPresentationAndModifierBase
            | EmojiStatus::EmojiPresentationAndEmojiComponent
            | EmojiStatus::EmojiPresentationAndModifierAndEmojiComponent
    )
}

fn is_regional_indicator(c: char) -> bool {
    ('\u{1f1e6}'..='\u{1f1ff}').contains(&c)
}

/// Terminal cells of one code point: 0 combining/zero-width, 2 wide or emoji-presentation, else 1.
pub fn char_width(c: char) -> usize {
    let cp = c as u32;
    if cp == 0 {
        return 0;
    }
    if matches!(cp,
        0x0300..=0x036f | 0x1ab0..=0x1aff | 0x1dc0..=0x1dff | 0x200b..=0x200f
        | 0x20d0..=0x20ff | 0xfe00..=0xfe0f | 0x2060 | 0xfeff)
    {
        return 0;
    }
    if matches!(cp,
        0x1100..=0x115f | 0x2e80..=0xa4cf | 0xac00..=0xd7a3 | 0xf900..=0xfaff
        | 0xfe30..=0xfe4f | 0xff00..=0xff60 | 0xffe0..=0xffe6 | 0x1f300..=0x1f64f
        | 0x1f900..=0x1faff | 0x20000..=0x3fffd)
        && cp != 0x303f
    {
        return 2;
    }
    if cp > 0x2000 && is_emoji_presentation(c) {
        return 2;
    }
    1
}

/// Width of one grapheme cluster: ZWJ sequences, flags, base+VS16 and emoji presentation are
/// wide, base+VS15 narrow, otherwise the sum of the code point widths.
pub fn cluster_width(cluster: &str) -> usize {
    if cluster.contains(ZWJ) {
        return 2;
    }
    let cps: Vec<char> = cluster.chars().collect();
    if cps.len() == 2 && cps.iter().all(|&c| is_regional_indicator(c)) {
        return 2;
    }
    if cluster.contains(VS15) {
        return 1;
    }
    if cluster.contains(VS16) || cps.iter().any(|&c| is_emoji_presentation(c)) {
        return 2;
    }
    cps.iter().map(|&c| char_width(c)).sum()
}

/// Display width of a string; ANSI colour/cursor sequences count 0.
pub fn width(s: &str) -> usize {
    let stripped = strip_ansi(s);
    if stripped.bytes().all(|b| (0x20..=0x7e).contains(&b)) {
        return stripped.len();
    }
    stripped.graphemes(true).map(cluster_width).sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn widths() {
        assert_eq!(width("abc"), 3);
        assert_eq!(width("日本"), 4);
        assert_eq!(width("\x1b[31mab\x1b[0m"), 2);
        assert_eq!(char_width('\u{301}'), 0);
        assert_eq!(width("e\u{301}"), 1); // decomposed diacritic
        assert_eq!(width("😀"), 2);
        assert_eq!(width("✅"), 2); // single code point, Emoji_Presentation
        assert_eq!(width("👨‍👩‍👧"), 2); // ZWJ family
        assert_eq!(width("🇻🇳"), 2); // flag pair
        assert_eq!(width("❤\u{fe0f}"), 2); // base + VS16
        assert_eq!(width("❤\u{fe0e}"), 1); // base + VS15
        assert_eq!(width("\u{303f}"), 1);
    }

    #[test]
    fn js_space_matches_regex_s() {
        assert!(is_js_space('\u{feff}') && is_js_space('\u{a0}'));
        assert!(!is_js_space('\u{85}') && !is_js_space('a'));
    }
}
