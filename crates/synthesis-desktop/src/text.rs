//! Cleaning of strings that came from the UI or from agent-desktop.

use synthesis_browser::redact_text;
use synthesis_permissions::desktop::is_spoofing;

/// Control characters (newlines included) become spaces, invisible and bidi characters (zero
/// width, Unicode tags, overrides ...) are dropped so a name cannot hide from the hard-deny match
/// or from the person, secret-shaped strings are redacted, and the result is cut to `max`.
pub fn clean(s: &str, max: usize) -> String {
    let flat: String = s
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .filter(|c| !is_spoofing(*c))
        .collect();
    let red = redact_text(&flat);
    let mut out: String = red.chars().take(max).collect();
    if red.chars().count() > max {
        out.push_str("...");
    }
    out
}

/// Most bytes one OSC/DCS/APC/PM/SOS sequence may swallow.
const MAX_ESCAPE: usize = 4096;

/// Text of an app's log: terminal escape sequences (CSI, OSC such as a title or a hyperlink, and
/// other two-byte escapes) and invisible or bidi characters removed, newlines and tabs kept, secret-shaped
/// strings redacted.
pub fn clean_log(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut it = s.chars().peekable();
    while let Some(c) = it.next() {
        match c {
            '\u{1b}' => match it.next() {
                // CSI: parameters and intermediates, then one final byte 0x40-0x7e
                Some('[') => {
                    for n in it.by_ref() {
                        if ('\u{40}'..='\u{7e}').contains(&n) {
                            break;
                        }
                    }
                }
                // OSC (and DCS, APC, PM, SOS): up to BEL or ST (ESC \)
                Some(']' | 'P' | '_' | '^' | 'X') => {
                    // An unterminated one must not eat the rest of the log: drop at most the cap.
                    let (mut taken, mut ended) = (0usize, false);
                    while taken < MAX_ESCAPE {
                        let Some(n) = it.next() else { break };
                        taken += n.len_utf8();
                        if n == '\u{7}' || (n == '\u{1b}' && it.next_if_eq(&'\\').is_some()) {
                            ended = true;
                            break;
                        }
                    }
                    if !ended && taken >= MAX_ESCAPE {
                        out.push_str("[unterminated escape removed]");
                    }
                }
                _ => {}
            },
            '\n' | '\t' => out.push(c),
            c if is_spoofing(c) => {}
            c => out.push(c),
        }
    }
    redact_text(&out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flattens_cuts_and_redacts() {
        assert_eq!(clean("a\nb\tc\u{1b}d", 50), "a b c d");
        assert_eq!(clean(&"x".repeat(30), 10), format!("{}...", "x".repeat(10)));
        let t = clean(
            "token sk-ant-api03-abcdefghijklmnopqrstuvwxyz0123456789",
            200,
        );
        assert!(!t.contains("abcdefghijklmnopqrstuvwxyz0123456789"), "{t}");
    }

    #[test]
    fn an_unterminated_escape_drops_only_a_capped_span() {
        let tail = "y".repeat(100);
        let t = clean_log(&format!("a\u{1b}]0;{}{tail}", "x".repeat(5000)));
        assert!(t.starts_with("a[unterminated escape removed]"), "{t}");
        assert!(t.ends_with(&"y".repeat(100)), "{t}");
        // a short unterminated one at the end of the input: nothing left to show
        assert_eq!(clean_log("a\u{1b}]0;title"), "a");
    }

    #[test]
    fn invisible_and_bidi_characters_are_dropped_so_a_name_cannot_hide() {
        assert_eq!(clean("Pass\u{200B}words\u{E0041}\u{202E}", 50), "Passwords");
    }

    #[test]
    fn logs_lose_escape_sequences_and_spoofing_but_keep_lines() {
        let raw = "ok\x1b[31mred\x1b[0m\n\x1b]0;fake title\x07\x1b]8;;http://x\x1b\\link\x1b]8;;\x1b\\\tZ\u{200B}W\u{202E}\x1bc!";
        assert_eq!(clean_log(raw), "okred\nlink\tZW!");
    }
}
