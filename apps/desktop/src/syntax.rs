//! A tiny line tokenizer for the code viewer: comments, strings, numbers and keywords, with one
//! generic keyword list. shortcut: per line (a block comment or a multi-line string is coloured
//! only on the line that opens it), no per-language grammar; upgrade to tree-sitter if wanted.

use std::ops::Range;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    Keyword,
    Str,
    Comment,
    Number,
}

const KEYWORDS: &[&str] = &[
    "as",
    "async",
    "await",
    "break",
    "class",
    "const",
    "continue",
    "def",
    "else",
    "enum",
    "export",
    "false",
    "fn",
    "for",
    "from",
    "function",
    "if",
    "impl",
    "import",
    "in",
    "interface",
    "let",
    "loop",
    "match",
    "mod",
    "new",
    "null",
    "pub",
    "return",
    "self",
    "static",
    "struct",
    "true",
    "type",
    "use",
    "var",
    "while",
    "yield",
];

/// Coloured byte ranges of `line`, in order; everything else is plain text.
pub fn tokens(line: &str) -> Vec<(Range<usize>, Kind)> {
    let b = line.as_bytes();
    let mut out = vec![];
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        if c == b'/' && b.get(i + 1) == Some(&b'/') || c == b'#' {
            out.push((i..b.len(), Kind::Comment));
            break;
        } else if matches!(c, b'"' | b'\'' | b'`') {
            let mut j = i + 1;
            while j < b.len() && b[j] != c {
                j += if b[j] == b'\\' { 2 } else { 1 };
            }
            let end = (j + 1).min(b.len());
            out.push((i..end, Kind::Str));
            i = end;
        } else if c.is_ascii_alphanumeric() || c == b'_' {
            let mut j = i;
            while j < b.len() && (b[j].is_ascii_alphanumeric() || b[j] == b'_') {
                j += 1;
            }
            let word = &line[i..j];
            if c.is_ascii_digit() {
                out.push((i..j, Kind::Number));
            } else if KEYWORDS.contains(&word) {
                out.push((i..j, Kind::Keyword));
            }
            i = j;
        } else {
            i += 1;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn colours_keywords_strings_numbers_and_comments() {
        let l = "const port = 3000 // 'x'";
        let t = tokens(l);
        let of = |k| {
            t.iter()
                .filter(|(_, x)| *x == k)
                .map(|(r, _)| &l[r.clone()])
                .collect::<Vec<_>>()
        };
        assert_eq!(of(Kind::Keyword), ["const"]);
        assert_eq!(of(Kind::Number), ["3000"]);
        assert_eq!(of(Kind::Comment), ["// 'x'"]);
        assert!(of(Kind::Str).is_empty(), "the quote is inside the comment");
    }

    #[test]
    fn strings_keep_escapes_and_unterminated_ones_end_at_the_line() {
        let l = r#"import { a } from 'node:http' + "a\"b" + 'open"#;
        let s: Vec<_> = tokens(l)
            .into_iter()
            .filter(|(_, k)| *k == Kind::Str)
            .map(|(r, _)| &l[r])
            .collect();
        assert_eq!(s, ["'node:http'", r#""a\"b""#, "'open"]);
    }

    #[test]
    fn non_ascii_text_does_not_panic() {
        assert!(tokens("let é = \"ü\" // ñ").len() >= 3);
    }
}
