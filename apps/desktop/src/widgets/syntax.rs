//! Syntax colouring of one code line (`cli.html` `hl()`: keywords, strings, numbers, comments, calls).
//! The scanner is ported from `synthesis-cli-ui/gpui/src/cli.rs` (`syntax_highlights`); the colours are the
//! platinum tokens instead of the design's blue/green/purple.

use std::ops::Range;

use gpui::{HighlightStyle, Hsla, StyledText};
use synthesis_cli_ui_render::theme as t;

use crate::theme::hsla;

const KEYWORDS: &[&str] = &[
    "import",
    "from",
    "export",
    "async",
    "await",
    "function",
    "const",
    "let",
    "var",
    "return",
    "if",
    "else",
    "new",
    "type",
    "interface",
    "throw",
    "try",
    "catch",
    "true",
    "false",
    "null",
];

/// The coloured byte ranges of `line` (everything else keeps the text colour).
pub fn highlights(line: &str) -> Vec<(Range<usize>, HighlightStyle)> {
    let bytes = line.as_bytes();
    let (mut i, mut out) = (0, Vec::new());
    let ch = |i: usize| line[i..].chars().next().map_or(1, char::len_utf8);
    while i < bytes.len() {
        let start = i;
        let colour: Option<Hsla> = if line[i..].starts_with("//") {
            i = bytes.len();
            Some(hsla(t::MUTED))
        } else if matches!(bytes[i], b'\'' | b'"' | b'`') {
            let quote = bytes[i];
            i += 1;
            while i < bytes.len() {
                if bytes[i] == b'\\' {
                    i += 1;
                    if i < bytes.len() {
                        i += ch(i);
                    }
                } else if bytes[i] == quote {
                    i += 1;
                    break;
                } else {
                    i += ch(i);
                }
            }
            Some(hsla(t::DIFF_ADD))
        } else if bytes[i].is_ascii_digit() {
            i += 1;
            while i < bytes.len() && (bytes[i].is_ascii_digit() || bytes[i] == b'.') {
                i += 1;
            }
            Some(hsla(t::WARN))
        } else if bytes[i].is_ascii_alphabetic() || matches!(bytes[i], b'_' | b'$') {
            i += 1;
            while i < bytes.len()
                && (bytes[i].is_ascii_alphanumeric() || matches!(bytes[i], b'_' | b'$'))
            {
                i += 1;
            }
            if KEYWORDS.contains(&&line[start..i]) {
                Some(hsla(t::ACCENT_LIGHT))
            } else if bytes.get(i) == Some(&b'(') {
                Some(hsla(t::INFO))
            } else {
                None
            }
        } else {
            i += ch(i);
            None
        };
        if let Some(c) = colour {
            out.push((start..i, HighlightStyle::color(c)));
        }
    }
    out
}

pub fn styled(line: &str) -> StyledText {
    StyledText::new(line.to_owned()).with_highlights(highlights(line))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keywords_strings_calls_numbers_and_comments_are_found() {
        let l = "const x = f('a', 12) // note";
        let r: Vec<_> = highlights(l).into_iter().map(|(r, _)| &l[r]).collect();
        assert_eq!(r, ["const", "f", "'a'", "12", "// note"]);
    }

    #[test]
    fn multibyte_text_never_splits_a_char() {
        let l = "let s = 'h\u{e9}llo \u{4e16}\u{754c}'; \u{2713}";
        for (r, _) in highlights(l) {
            assert!(l.is_char_boundary(r.start) && l.is_char_boundary(r.end));
        }
    }
}
