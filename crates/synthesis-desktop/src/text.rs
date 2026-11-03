//! Cleaning of strings that came from the UI or from agent-desktop.

use synthesis_browser::redact_text;

/// Control characters (newlines included) become spaces, secret-shaped strings are redacted,
/// and the result is cut to `max` characters.
pub fn clean(s: &str, max: usize) -> String {
    let flat: String = s
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    let red = redact_text(&flat);
    let mut out: String = red.chars().take(max).collect();
    if red.chars().count() > max {
        out.push_str("...");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flattens_cuts_and_redacts() {
        assert_eq!(clean("a\nb\tc\u{1b}d", 50), "a b c d");
        assert_eq!(clean(&"x".repeat(30), 10), format!("{}...", "x".repeat(10)));
        let t = clean("token sk-ant-api03-abcdefghijklmnopqrstuvwxyz0123456789", 200);
        assert!(!t.contains("abcdefghijklmnopqrstuvwxyz0123456789"), "{t}");
    }
}
