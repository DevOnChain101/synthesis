//! `synthesis.verification` of settings.json (schema: docs/specs/config.md §6) -> gate configuration.
//! synthesis-settings only carries the raw object; unknown or invalid entries are ignored with a
//! diagnostic that names the key, never the value.

use serde_json::{Map, Value};
use synthesis_verification::{Criterion, GateConfig, InferredOnlyPolicy};

#[derive(Debug, Default)]
pub struct VerificationSettings {
    pub inferred_only: InferredOnlyPolicy,
    pub max_attempts: Option<u32>,
    /// Extra required checks: user-source criteria, so they gate completion like `--criteria-file`.
    pub checks: Vec<Criterion>,
    pub diagnostics: Vec<String>,
}

impl VerificationSettings {
    pub fn gate(&self, budget_attempts: u32) -> GateConfig {
        GateConfig {
            max_verify_attempts: self.max_attempts.unwrap_or(budget_attempts),
            inferred_only: self.inferred_only,
        }
    }
}

pub fn parse(m: &Map<String, Value>) -> VerificationSettings {
    let mut v = VerificationSettings::default();
    for (k, x) in m {
        match k.as_str() {
            "inferred_only" => match x.as_str() {
                Some("cap_unverified") => v.inferred_only = InferredOnlyPolicy::CapUnverified,
                Some("allow_passed") => v.inferred_only = InferredOnlyPolicy::AllowPassed,
                _ => v.diagnostics.push(
                    "synthesis.verification.inferred_only must be \"cap_unverified\" or \"allow_passed\", ignored".into(),
                ),
            },
            "max_attempts" => match x.as_u64().filter(|n| (1..=100).contains(n)) {
                Some(n) => v.max_attempts = Some(n as u32),
                None => v
                    .diagnostics
                    .push("synthesis.verification.max_attempts must be an integer 1..100, ignored".into()),
            },
            "checks" => parse_checks(x, &mut v),
            other => v
                .diagnostics
                .push(format!("synthesis.verification.{other}: unknown key, ignored")),
        }
    }
    v
}

fn parse_checks(x: &Value, v: &mut VerificationSettings) {
    let Some(list) = x.as_array() else {
        v.diagnostics
            .push("synthesis.verification.checks must be an array, ignored".into());
        return;
    };
    for (i, c) in list.iter().enumerate() {
        let cmd = c
            .as_str()
            .or_else(|| c.get("command").and_then(Value::as_str))
            .map(str::trim)
            .filter(|s| !s.is_empty() && s.len() <= 4096 && !s.contains('\0'));
        let Some(cmd) = cmd else {
            v.diagnostics.push(format!(
                "synthesis.verification.checks[{i}] needs a non-empty \"command\", ignored"
            ));
            continue;
        };
        let field = |k: &str| c.get(k).and_then(Value::as_str).filter(|s| !s.is_empty());
        let id = field("id").map_or_else(|| format!("vs{}", i + 1), String::from);
        let text = field("text").unwrap_or(cmd);
        v.checks.push(Criterion::user(&id, text, Some(cmd)));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn p(v: Value) -> VerificationSettings {
        parse(v.as_object().unwrap())
    }

    #[test]
    fn maps_policy_attempts_and_checks() {
        let v = p(json!({"inferred_only":"allow_passed","max_attempts":5,
            "checks":["make lint",{"id":"t","text":"unit","command":"make test"}]}));
        assert!(v.diagnostics.is_empty(), "{:?}", v.diagnostics);
        assert_eq!(v.inferred_only, InferredOnlyPolicy::AllowPassed);
        let g = v.gate(3);
        assert_eq!(g.max_verify_attempts, 5);
        assert_eq!(v.checks.len(), 2);
        assert_eq!(v.checks[0].id, "vs1");
        assert_eq!(v.checks[1].check.as_deref(), Some("make test"));
        assert_eq!(p(json!({})).gate(3).max_verify_attempts, 3);
    }

    #[test]
    fn bad_entries_are_ignored_with_a_value_free_diagnostic() {
        let v = p(
            json!({"inferred_only":"SECRET-ish","max_attempts":0,"checks":[{"text":"x"}],"zzz":1}),
        );
        assert_eq!(v.inferred_only, InferredOnlyPolicy::AllowPassed);
        assert!(v.max_attempts.is_none() && v.checks.is_empty());
        assert_eq!(v.diagnostics.len(), 4);
        assert!(v.diagnostics.iter().all(|d| !d.contains("SECRET")));
    }
}
