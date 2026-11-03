//! The JSON envelope every agent-desktop command prints (its docs/json-output.md):
//! `{version, ok, command, data | error}`. Parsed strictly where it matters (shape, major
//! version, the command we asked for); unknown fields are ignored so a newer minor still works.

use serde_json::Value;

use crate::error::{DesktopError, Result};
use crate::text::clean;

/// Envelope major version this client understands.
pub const SUPPORTED_MAJOR: &str = "2";

/// What the CLI knows about whether the input reached the app (`disposition.delivery`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Delivery {
    Unknown,
    NotDelivered,
    DeliveryUncertain,
    DeliveredUnverified,
    DeliveredVerified,
}

impl Delivery {
    pub fn parse(s: &str) -> Self {
        match s {
            "not_delivered" => Self::NotDelivered,
            "delivery_uncertain" => Self::DeliveryUncertain,
            "delivered_unverified" => Self::DeliveredUnverified,
            "delivered_verified" => Self::DeliveredVerified,
            _ => Self::Unknown,
        }
    }
    pub fn delivered(self) -> bool {
        matches!(self, Self::DeliveredUnverified | Self::DeliveredVerified)
    }
    pub fn verified(self) -> bool {
        self == Self::DeliveredVerified
    }
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Unknown => "unknown",
            Self::NotDelivered => "not_delivered",
            Self::DeliveryUncertain => "delivery_uncertain",
            Self::DeliveredUnverified => "delivered_unverified",
            Self::DeliveredVerified => "delivered_verified",
        }
    }
}

/// `disposition.delivery` of a result, `Unknown` when absent.
pub fn delivery_of(data: &Value) -> Delivery {
    data["disposition"]["delivery"]
        .as_str()
        .map_or(Delivery::Unknown, Delivery::parse)
}

/// Checks the envelope and returns `data` of a success. `expect` is the command we ran.
pub fn parse(stdout: &[u8], expect: &str) -> Result<Value> {
    let bad = |m: &str| DesktopError::Protocol(format!("agent-desktop {expect}: {m}"));
    let v: Value = serde_json::from_slice(stdout).map_err(|_| bad("output is not JSON"))?;
    let Some(version) = v["version"].as_str() else {
        return Err(bad("envelope has no version"));
    };
    if version.split('.').next() != Some(SUPPORTED_MAJOR) {
        return Err(bad(&format!(
            "envelope version {} is not supported (this client reads {SUPPORTED_MAJOR}.x)",
            clean(version, 20)
        )));
    }
    if v["command"].as_str() != Some(expect) {
        return Err(bad("envelope answers a different command"));
    }
    match v["ok"].as_bool() {
        Some(true) => {
            if v.get("data").is_none() {
                return Err(bad("ok envelope without data"));
            }
            Ok(v["data"].clone())
        }
        Some(false) => Err(error_of(&v["error"], expect)),
        None => Err(bad("envelope has no ok flag")),
    }
}

fn error_of(e: &Value, expect: &str) -> DesktopError {
    let code = e["code"]
        .as_str()
        .filter(|c| {
            !c.is_empty() && c.len() <= 40 && c.chars().all(|c| c.is_ascii_uppercase() || c == '_')
        })
        .unwrap_or("INTERNAL")
        .to_owned();
    let message = clean(e["message"].as_str().unwrap_or("(no message)"), 300);
    // only an explicit "safe" allows a retry; a missing or unknown disposition does not
    let retry_safe = e["disposition"]["retry"].as_str() == Some("safe");
    match code.as_str() {
        "STALE_REF" | "ELEMENT_NOT_FOUND" if expect != "snapshot" => {
            DesktopError::StaleRef(format!(
                "{code}: {message}. Take a new snapshot and use its refs{}",
                if retry_safe {
                    " (nothing was delivered)"
                } else {
                    " (input may already have been delivered: check the UI before repeating)"
                }
            ))
        }
        _ => DesktopError::Tool {
            code,
            message,
            retry_safe,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // envelopes copied from the reference docs (README.md, docs/json-output.md)
    const OK_CLICK: &str = r#"{"version":"2.5","ok":true,"command":"click","data":{"action":"click","disposition":{"delivery":"delivered_unverified","retry":"unsafe"}}}"#;
    const STALE: &str = r#"{"version":"2.5","ok":false,"command":"click","error":{"code":"STALE_REF","message":"@s8f3k2p9:e7 not found in current RefMap","suggestion":"Re-run a snapshot","recovery":{"strategy":"refresh_snapshot_then_retry_original","retryable":true,"requires_fresh_snapshot":true},"disposition":{"delivery":"not_delivered","retry":"safe"}}}"#;

    #[test]
    fn a_success_returns_data_and_its_delivery() {
        let d = parse(OK_CLICK.as_bytes(), "click").unwrap();
        assert_eq!(delivery_of(&d), Delivery::DeliveredUnverified);
        assert!(delivery_of(&d).delivered() && !delivery_of(&d).verified());
    }

    #[test]
    fn stale_ref_is_a_stale_error_with_the_delivery_hint() {
        let e = parse(STALE.as_bytes(), "click").unwrap_err();
        assert_eq!(e.kind(), "stale_ref");
        assert!(e.to_string().contains("nothing was delivered"));
        let unsafe_ = STALE.replace(r#""retry":"safe""#, r#""retry":"unsafe""#);
        let e = parse(unsafe_.as_bytes(), "click").unwrap_err();
        assert!(e.to_string().contains("may already have been delivered"));
    }

    #[test]
    fn ambiguous_target_and_unknown_codes_stay_tool_errors() {
        let amb = r#"{"version":"2.5","ok":false,"command":"snapshot","error":{"code":"AMBIGUOUS_TARGET","message":"several windows","disposition":{"delivery":"not_delivered","retry":"safe"},"details":{"candidates":["w-1"]}}}"#;
        let e = parse(amb.as_bytes(), "snapshot").unwrap_err();
        assert_eq!(e.kind(), "ambiguous_target");
        // a missing disposition is never "safe"
        let none = r#"{"version":"2.5","ok":false,"command":"click","error":{"code":"ACTION_FAILED","message":"x"}}"#;
        assert!(matches!(
            parse(none.as_bytes(), "click").unwrap_err(),
            DesktopError::Tool {
                retry_safe: false,
                ..
            }
        ));
    }

    #[test]
    fn strict_where_it_matters_lenient_elsewhere() {
        assert!(parse(b"not json", "click").is_err());
        let wrong_cmd = OK_CLICK.replace(r#""command":"click""#, r#""command":"type""#);
        assert!(parse(wrong_cmd.as_bytes(), "click").is_err());
        let v3 = OK_CLICK.replace(r#""version":"2.5""#, r#""version":"3.0""#);
        assert!(
            parse(v3.as_bytes(), "click")
                .unwrap_err()
                .to_string()
                .contains("not supported")
        );
        let no_data = r#"{"version":"2.5","ok":true,"command":"click"}"#;
        assert!(parse(no_data.as_bytes(), "click").is_err());
        let extra = OK_CLICK.replace(r#""ok":true"#, r#""ok":true,"future":{"x":1}"#);
        assert!(parse(extra.as_bytes(), "click").is_ok());
        let minor = OK_CLICK.replace("2.5", "2.9");
        assert!(parse(minor.as_bytes(), "click").is_ok());
    }
}
