//! `meta.action` of a browser tool -> `browser.action`. Only whitelisted fields leave: the action is one of a
//! fixed set of names (never the tool's own string), the point is range-checked, the target is a short
//! sanitized and redacted label (the word `secret` for `browser_fill_secret`). Never typed text or keys.

use serde_json::Value;
use synthesis_events::Event;

use crate::util::sanitize_for_display;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Click,
    Hover,
    Type,
    Scroll,
    Navigate,
    Press,
    Other,
}

impl Kind {
    fn parse(s: &str) -> Self {
        match s {
            "click" => Self::Click,
            "hover" => Self::Hover,
            "type" => Self::Type,
            "scroll" => Self::Scroll,
            "navigate" => Self::Navigate,
            "press" => Self::Press,
            _ => Self::Other,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Click => "click",
            Self::Hover => "hover",
            Self::Type => "type",
            Self::Scroll => "scroll",
            Self::Navigate => "navigate",
            Self::Press => "press",
            Self::Other => "other",
        }
    }
}

pub(crate) fn event(tool: &str, a: &Value, redact: &dyn Fn(&str) -> String) -> Event {
    let num = |k: &str| a.get(k).and_then(Value::as_i64);
    let target = if tool == "browser_fill_secret" {
        Some("secret".to_string())
    } else {
        a.get("target")
            .and_then(Value::as_str)
            .map(|t| redact(&sanitize_for_display(t)).chars().take(80).collect())
    };
    Event::BrowserAction {
        action: Kind::parse(a.get("action").and_then(Value::as_str).unwrap_or(""))
            .label()
            .into(),
        x: num("x").and_then(|v| i32::try_from(v).ok()),
        y: num("y").and_then(|v| i32::try_from(v).ok()),
        w: num("w").and_then(|v| u32::try_from(v).ok()),
        h: num("h").and_then(|v| u32::try_from(v).ok()),
        target,
        frame: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn ev(tool: &str, a: Value) -> (String, Option<i32>, Option<u32>, Option<String>) {
        match event(tool, &a, &|s| s.replace("sk-secret", "[REDACTED]")) {
            Event::BrowserAction {
                action,
                x,
                w,
                target,
                ..
            } => (action, x, w, target),
            _ => unreachable!(),
        }
    }

    #[test]
    fn the_action_is_one_of_a_fixed_set_never_the_tools_string() {
        assert_eq!(ev("browser_click", json!({"action": "click"})).0, "click");
        assert_eq!(
            ev(
                "browser_click",
                json!({"action": "Bearer sk-abc\u{1b}[31m"})
            )
            .0,
            "other"
        );
        assert_eq!(ev("browser_click", json!({})).0, "other");
    }

    #[test]
    fn the_target_is_sanitized_redacted_and_clipped_and_numbers_never_wrap() {
        let (_, _, _, t) = ev(
            "browser_click",
            json!({"action": "click", "target": "button \u{1b}[31mpay sk-secret\u{7}"}),
        );
        let t = t.unwrap();
        assert!(
            !t.contains('\u{1b}') && !t.contains('\u{7}') && !t.contains("sk-secret"),
            "{t:?}"
        );
        let (_, x, w, _) = ev(
            "browser_click",
            json!({"action": "click", "x": 5_000_000_000i64, "w": -1}),
        );
        assert_eq!((x, w), (None, None), "out of range is dropped, not wrapped");
        let (_, x, w, _) = ev("browser_click", json!({"x": 12, "w": 800}));
        assert_eq!((x, w), (Some(12), Some(800)));
    }

    #[test]
    fn a_secret_fill_reports_only_the_word_secret() {
        let (_, _, _, t) = ev(
            "browser_fill_secret",
            json!({"action": "type", "target": "Password hunter2"}),
        );
        assert_eq!(t.as_deref(), Some("secret"));
    }
}
