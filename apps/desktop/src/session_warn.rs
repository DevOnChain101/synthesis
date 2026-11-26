//! The soft warning when a NEW session is started while many are open (`synt.desktop.sessions.warn_at`).
//! Owner decision 2026-11-25: sessions are never limited, only warned about; the conductor's own
//! spawn cap (`synt.conductor.max_sessions`) is a different thing.

use std::path::Path;

pub const DEFAULT_WARN_AT: usize = 6;

/// `synt.desktop.sessions.warn_at` of the user settings file (integer >= 1); anything else is the default.
pub fn read_warn_at(home: &Path) -> usize {
    synthesis_settings::read_json(&home.join("settings.json"), "user settings")
        .ok()
        .flatten()
        .and_then(|d| d["synt"]["desktop"]["sessions"]["warn_at"].as_u64())
        .filter(|n| *n >= 1)
        .map_or(DEFAULT_WARN_AT, |n| n as usize)
}

/// The notice for a new session when `live` sessions are already open and `warn_at` is the threshold.
pub fn new_session_warning(live: usize, warn_at: usize) -> Option<String> {
    (live >= warn_at)
        .then(|| format!("You have {live} sessions open; each keeps an engine running"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn warns_from_the_threshold_on_and_never_below() {
        assert_eq!(new_session_warning(5, 6), None);
        assert!(
            new_session_warning(6, 6)
                .unwrap()
                .contains("6 sessions open")
        );
        assert!(new_session_warning(9, 6).is_some());
        assert!(new_session_warning(1, 1).is_some());
    }

    #[test]
    fn the_setting_is_an_integer_of_at_least_one() {
        let d = tempfile::tempdir().unwrap();
        assert_eq!(read_warn_at(d.path()), 6);
        for (json, want) in [
            (r#"{"synt":{"desktop":{"sessions":{"warn_at":3}}}}"#, 3),
            (r#"{"synt":{"desktop":{"sessions":{"warn_at":0}}}}"#, 6),
            (r#"{"synt":{"desktop":{"sessions":{"warn_at":"x"}}}}"#, 6),
        ] {
            std::fs::write(d.path().join("settings.json"), json).unwrap();
            assert_eq!(read_warn_at(d.path()), want, "{json}");
        }
    }
}
