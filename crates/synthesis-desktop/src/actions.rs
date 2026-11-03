//! `desktop_act` actions and their agent-desktop spelling. Headless semantic actions only:
//! never `--headed`, coordinates or the clipboard.

use crate::error::{DesktopError, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Click,
    DoubleClick,
    RightClick,
    SetValue,
    Type,
    Clear,
    Select,
    Toggle,
    Check,
    Uncheck,
    Expand,
    Collapse,
    Focus,
    ScrollTo,
    Press,
}

/// Longest `value` (text to type, option to select, key combo).
pub const MAX_VALUE: usize = 4096;

impl Action {
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "click" => Self::Click,
            "double_click" => Self::DoubleClick,
            "right_click" => Self::RightClick,
            "set_value" => Self::SetValue,
            "type" => Self::Type,
            "clear" => Self::Clear,
            "select" => Self::Select,
            "toggle" => Self::Toggle,
            "check" => Self::Check,
            "uncheck" => Self::Uncheck,
            "expand" => Self::Expand,
            "collapse" => Self::Collapse,
            "focus" => Self::Focus,
            "scroll_to" => Self::ScrollTo,
            "press" => Self::Press,
            _ => return None,
        })
    }

    fn command(self) -> &'static str {
        match self {
            Self::Click => "click",
            Self::DoubleClick => "double-click",
            Self::RightClick => "right-click",
            Self::SetValue => "set-value",
            Self::Type => "type",
            Self::Clear => "clear",
            Self::Select => "select",
            Self::Toggle => "toggle",
            Self::Check => "check",
            Self::Uncheck => "uncheck",
            Self::Expand => "expand",
            Self::Collapse => "collapse",
            Self::Focus => "focus",
            Self::ScrollTo => "scroll-to",
            Self::Press => "press",
        }
    }

    /// The command name the envelope answers with.
    pub fn envelope_command(self) -> &'static str {
        self.command()
    }

    pub fn needs_value(self) -> bool {
        matches!(
            self,
            Self::SetValue | Self::Type | Self::Select | Self::Press
        )
    }

    /// `press` targets a window, every other action a ref.
    pub fn needs_ref(self) -> bool {
        self != Self::Press
    }

    /// Arguments after the binary name. `window_id` scopes `press`; ref actions are scoped by
    /// the window their ref belongs to.
    pub fn cli_args(
        self,
        r: Option<&str>,
        value: Option<&str>,
        window_id: &str,
    ) -> Result<Vec<String>> {
        let bad = |m: &str| DesktopError::Refused(format!("desktop_act {}: {m}", self.command()));
        let mut a = vec![self.command().to_owned()];
        if self.needs_ref() {
            let r = r.ok_or_else(|| bad("\"ref\" is required"))?;
            if !valid_ref(r) {
                return Err(bad(
                    "\"ref\" must look like @<snapshot>:eN, copied from a snapshot",
                ));
            }
            a.push(r.to_owned());
        }
        if self.needs_value() {
            let v = value.ok_or_else(|| bad("\"value\" is required"))?;
            if v.chars().count() > MAX_VALUE || v.contains('\0') {
                return Err(bad("\"value\" is too long or contains NUL"));
            }
            if self == Self::Press && !valid_combo(v) {
                return Err(bad(
                    "\"value\" must be a key combo such as return, escape or cmd+c",
                ));
            }
            a.push(v.to_owned());
        } else if value.is_some() {
            return Err(bad("takes no \"value\""));
        }
        if self == Self::Press {
            a.extend(["--window-id".into(), window_id.to_owned()]);
        }
        Ok(a)
    }
}

/// `@<snapshot id>:e<N>`
pub fn valid_ref(r: &str) -> bool {
    let Some(rest) = r.strip_prefix('@') else {
        return false;
    };
    let Some((sid, n)) = rest.split_once(":e") else {
        return false;
    };
    (1..=24).contains(&sid.len())
        && sid.bytes().all(|b| b.is_ascii_alphanumeric())
        && (1..=8).contains(&n.len())
        && n.bytes().all(|b| b.is_ascii_digit())
}

fn valid_combo(c: &str) -> bool {
    !c.starts_with('-')
        && (1..=40).contains(&c.len())
        && c.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'+' || b == b'_' || b == b'-')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ref_actions_carry_the_ref_and_never_a_headed_or_app_flag() {
        for name in [
            "click",
            "double_click",
            "right_click",
            "set_value",
            "type",
            "clear",
            "select",
            "toggle",
            "check",
            "uncheck",
            "expand",
            "collapse",
            "focus",
            "scroll_to",
        ] {
            let a = Action::parse(name).unwrap();
            let v = a.needs_value().then_some("x");
            let args = a.cli_args(Some("@s8f3k2p9:e8"), v, "w-1").unwrap();
            assert!(args.contains(&"@s8f3k2p9:e8".to_string()), "{args:?}");
            assert!(!args.iter().any(|x| x.starts_with("--")), "{args:?}");
        }
        assert_eq!(
            Action::Press.cli_args(None, Some("cmd+c"), "w-9").unwrap(),
            ["press", "cmd+c", "--window-id", "w-9"]
        );
    }

    #[test]
    fn bad_input_is_refused_before_a_process_starts() {
        let c = |a: Action, r: Option<&str>, v: Option<&str>| a.cli_args(r, v, "w");
        let long = "a".repeat(MAX_VALUE + 1);
        assert!(c(Action::Click, Some("e8"), None).is_err());
        assert!(c(Action::Click, Some("@s:e8 --headed"), None).is_err());
        assert!(c(Action::Click, None, None).is_err());
        assert!(c(Action::Click, Some("@s:e8"), Some("v")).is_err());
        assert!(c(Action::Type, Some("@s:e8"), None).is_err());
        assert!(c(Action::Press, None, Some("--force")).is_err());
        assert!(c(Action::Type, Some("@s:e8"), Some(&long)).is_err());
        // a value that looks like a flag is passed as the value, not parsed as one
        assert!(c(Action::Type, Some("@s:e8"), Some("--headed")).is_ok());
        assert!(Action::parse("mouse_click").is_none() && Action::parse("headed").is_none());
    }
}
