//! The person's own terminal (docs/specs/terminal-browser.md 1.2, 1.10): a personal shell on a PTY owned by
//! the desktop, parsed into an `alacritty_terminal::Term` that the TUI view draws. Everything here is plain
//! Rust without GPUI (tested without a window); the element and the pane are `tui_view/term_pane.rs`.
//!
//! The personal shell is the person's: unconfined, never visible to the agent, never an event, never in a
//! log, so it does not go through `synthesis-terminal`'s `TerminalRuntime` (that one logs and is the
//! agent's). Only the reply allow-list of its `Term` is shared (`synthesis_terminal::Reply`).

pub mod keys;
pub mod launch;
pub mod session;

/// A terminal title (OSC 0/2) as the tab may show it: control, bidi, tag and blank-glyph characters are
/// dropped (the same set `sanitize_for_display` makes visible), clipped to 80 characters.
pub fn sanitize_title(raw: &str) -> String {
    let kept: String = raw
        .chars()
        .take(512)
        .filter(|c| {
            let s = c.to_string();
            synthesis_harness::sanitize_for_display(&s) == s
        })
        .take(80)
        .collect();
    kept.trim().to_string()
}

/// The bytes of a `--pty-script` file: text with `\e` (ESC), `\a` (BEL), `\r`, `\n`, `\t`, `\\` and `\xNN`; a real newline
/// in the file is not a byte (the file is one line per `\n` escape), so scripts stay readable and exact.
pub fn unescape(script: &str) -> Vec<u8> {
    let mut out = Vec::new();
    let mut it = script
        .chars()
        .filter(|c| *c != '\n' && *c != '\r')
        .peekable();
    while let Some(c) = it.next() {
        if c != '\\' {
            out.extend(c.to_string().as_bytes());
            continue;
        }
        match it.next() {
            Some('e') => out.push(0x1b),
            Some('a') => out.push(7),
            Some('r') => out.push(b'\r'),
            Some('n') => out.push(b'\n'),
            Some('t') => out.push(b'\t'),
            Some('\\') => out.push(b'\\'),
            Some('x') => {
                let hex: String = (0..2)
                    .filter_map(|_| it.next_if(char::is_ascii_hexdigit))
                    .collect();
                out.extend(u8::from_str_radix(&hex, 16));
            }
            Some(o) => out.extend(format!("\\{o}").as_bytes()),
            None => out.push(b'\\'),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_script_unescapes_to_exact_bytes() {
        assert_eq!(
            unescape("a\\e[1mb\\r\\n\\x41\\\\\n\\q"),
            b"a\x1b[1mb\r\nA\\\\q"
        );
    }

    #[test]
    fn a_title_loses_escapes_bidi_and_length() {
        // ESC, BEL, a right-to-left override and an isolate, a zero-width space
        let t = sanitize_title("\u{1b}[31mvim\u{7} \u{202e}gpj.exe\u{2066}\u{200b}\n- x");
        assert_eq!(t, "[31mvim gpj.exe- x");
        assert!(!t.chars().any(|c| c.is_control()));
        assert_eq!(sanitize_title(&"a".repeat(500)).chars().count(), 80);
        assert_eq!(sanitize_title("\u{202e}\u{1b}"), "");
    }
}
