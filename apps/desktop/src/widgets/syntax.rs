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

/// The coloured byte ranges of one shell line: command names (first word, and after `| & ; ( `), flags, strings,
/// operators, `$vars`, paths and `#` comments. shortcut: no heredocs, `case` or `$(...)` nesting across lines;
/// upgrade to a real grammar only if blocks outgrow one-liners.
pub fn shell_highlights(line: &str) -> Vec<(Range<usize>, HighlightStyle)> {
    let (mut i, mut out, mut cmd) = (0, Vec::new(), true);
    let mut push = |r: Range<usize>, c: Hsla| out.push((r, HighlightStyle::color(c)));
    while i < line.len() {
        let c = line[i..].chars().next().unwrap_or(' ');
        let start = i;
        i += c.len_utf8();
        match c {
            c if c.is_whitespace() => {}
            '#' => {
                i = line.len();
                push(start..i, hsla(t::MUTED));
            }
            '\'' | '"' => {
                while i < line.len() {
                    let d = line[i..].chars().next().unwrap_or(' ');
                    i += d.len_utf8();
                    if d == '\\' && c == '"' {
                        i += line[i..].chars().next().map_or(0, char::len_utf8);
                    } else if d == c {
                        break;
                    }
                }
                push(start..i.min(line.len()), hsla(t::DIFF_ADD));
                cmd = false;
            }
            '|' | '&' | ';' | '<' | '>' | '(' | ')' => {
                while i < line.len() && matches!(line.as_bytes()[i], b'|' | b'&' | b'>') {
                    i += 1;
                }
                push(start..i, hsla(t::ACCENT));
                cmd = matches!(c, '|' | '&' | ';' | '(');
            }
            _ => {
                while let Some(d) = line[i..].chars().next() {
                    if d.is_whitespace() || "|&;<>()'\"".contains(d) {
                        break;
                    }
                    i += d.len_utf8();
                }
                let w = &line[start..i];
                let colour = if w.starts_with('$') {
                    Some(hsla(t::WARN))
                } else if w.starts_with('-') && w.len() > 1 {
                    Some(hsla(t::INFO))
                } else if w.contains('=') && cmd {
                    None // VAR=value stays plain and keeps the command position
                } else if cmd {
                    Some(hsla(t::ACCENT_LIGHT))
                } else if w.contains('/') || w.starts_with('~') || w.starts_with('.') {
                    Some(hsla(t::CYAN))
                } else {
                    None
                };
                if let Some(col) = colour {
                    push(start..i, col);
                }
                cmd = cmd && w.contains('=') && !w.starts_with('-');
            }
        }
    }
    out
}

/// The colouring of a block line: shell for commands and shell-like languages, the generic scanner for code.
pub fn block_highlights(
    command: bool,
    lang: &str,
    line: &str,
) -> Vec<(Range<usize>, HighlightStyle)> {
    if command || matches!(lang, "sh" | "bash" | "zsh" | "shell" | "console") {
        shell_highlights(line)
    } else {
        highlights(line)
    }
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
    fn a_shell_line_colours_command_flags_strings_operators_paths_and_comments() {
        let l = "FOO=1 git commit -m \"a b\" ./x | grep -q $HOME && ls ~/d # done";
        let r: Vec<_> = shell_highlights(l)
            .into_iter()
            .map(|(r, _)| &l[r])
            .collect();
        assert_eq!(
            r,
            [
                "git", "-m", "\"a b\"", "./x", "|", "grep", "-q", "$HOME", "&&", "ls", "~/d",
                "# done"
            ]
        );
        // a quote that never closes colours to the end of the line without panicking
        assert_eq!(shell_highlights("echo 'a \u{e9}").len(), 2);
    }

    #[test]
    fn multibyte_text_never_splits_a_char() {
        let l = "let s = 'h\u{e9}llo \u{4e16}\u{754c}'; \u{2713}";
        for (r, _) in highlights(l) {
            assert!(l.is_char_boundary(r.start) && l.is_char_boundary(r.end));
        }
    }
}
