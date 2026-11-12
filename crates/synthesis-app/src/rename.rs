//! `/rename <title>`: the title of the session in the store, for every frontend (CLI, desktop).
//! The store only holds titles a person set; the first prompt that names an untitled session is
//! derived when a list is read (`synthesis-view`), never written, so it cannot overwrite one.

use synthesis_config::is_invisible_or_blank;
use synthesis_events::Redactor;

use crate::events::Store;

pub const MAX_TITLE: usize = 120;
pub const USAGE: &str = "usage: /rename <title>  (up to 120 characters)";

/// The title as it will be stored: newlines and tabs become one space, runs of blanks collapse.
/// Refuses an empty or long title, hidden characters (controls, bidi, zero-width) and anything
/// that looks like a secret.
pub fn clean_title(raw: &str) -> Result<String, String> {
    let spaced: String = raw
        .chars()
        .map(|c| {
            if matches!(c, '\n' | '\r' | '\t') {
                ' '
            } else {
                c
            }
        })
        .collect();
    if spaced.chars().any(is_invisible_or_blank) {
        return Err("/rename: the title has control, bidi or zero-width characters".into());
    }
    let t = spaced.split_whitespace().collect::<Vec<_>>().join(" ");
    if t.is_empty() {
        return Err(USAGE.into());
    }
    if t.chars().count() > MAX_TITLE {
        return Err(format!("/rename: the title is over {MAX_TITLE} characters"));
    }
    if Redactor::new([]).redact_str(&t) != t {
        return Err("/rename: the title looks like it contains a secret; not stored".into());
    }
    Ok(t)
}

/// Validates and stores the title of the open session; returns what was stored.
pub fn rename(store: &Store, raw: &str) -> Result<String, String> {
    let t = clean_title(raw)?;
    store
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .set_title(&t)
        .map_err(|e| format!("/rename: {e}"))?;
    Ok(t)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn titles_are_cleaned_or_refused() {
        assert_eq!(clean_title("  fix\nthe\tbuild  ").unwrap(), "fix the build");
        assert!(clean_title("   ").is_err());
        assert!(clean_title(&"x".repeat(121)).is_err());
        assert!(clean_title(&"x".repeat(120)).is_ok());
        for bad in ["a\u{1b}[31mb", "a\u{202e}b", "a\u{200b}b", "a\u{0}b"] {
            assert!(clean_title(bad).is_err(), "{bad:?}");
        }
        assert!(clean_title("use sk-ant-api03-abcdefghijklmnopqrstuvwxyz0123456789").is_err());
        assert!(clean_title("Bearer abcdefghijklmnop").is_err());
    }
}
