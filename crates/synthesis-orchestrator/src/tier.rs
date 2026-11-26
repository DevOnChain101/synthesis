//! Risk tier and recommended answer of a decision (docs/specs/approvals.md section 3). Tiers decide
//! attention, never authority. Both come from the owner's stored typed facts: the kind, the brief's
//! alternative ids, the stored action, the item's retry counters and the contract's budget. No label,
//! `why`, `delta`, tier or recommendation written by an agent is ever read, and anything missing,
//! unreadable or unknown ends in a HIGHER tier.

use std::path::Path;

use serde_json::Value;
use synthesis_core::secrets::is_secret_shaped;
use synthesis_permissions::irreversible;
use synthesis_session::Reader;
use synthesis_session::outcome::{DecisionRecord, OutcomeRecord};

/// Recorded with every answer: bump it when the table below changes.
pub const CLASSIFIER_VERSION: u32 = 1;

/// A command longer than this is not read: it is T3.
const MAX_COMMAND: usize = 16 << 10;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Tier {
    /// Nothing to decide (a lapsed or recorded row).
    T0,
    T1,
    T2,
    T3,
}

impl Tier {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::T0 => "T0",
            Self::T1 => "T1",
            Self::T2 => "T2",
            Self::T3 => "T3",
        }
    }
}

/// The counters a `blocked` retry is judged by.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ItemFacts {
    pub retries_used: u32,
    /// Attempts the item has had.
    pub attempts: u32,
    /// The contract's `max_attempts_per_work_item`.
    pub max_attempts: u32,
}

impl ItemFacts {
    /// Another attempt would be refused: the store checks the attempt ordinal against the budget
    /// (`new_attempt`) and `retries_used` against `budget - 1` (a retryable failure).
    fn spent(self) -> bool {
        self.attempts >= self.max_attempts
            || self.retries_used >= self.max_attempts.saturating_sub(1)
    }
}

/// `action`: the stored action as the store verified it (`None`: none, or it could not be read).
/// `item`: `None` when the facts could not be read.
pub fn classify(kind: &str, action: Option<&Value>, item: Option<ItemFacts>) -> Tier {
    match kind {
        "blocked" => match item {
            Some(i) if !i.spent() => Tier::T1,
            _ => Tier::T2,
        },
        "approval_needed" => {
            if action.is_none_or(dangerous) {
                Tier::T3
            } else {
                Tier::T2
            }
        }
        "ambiguity" | "plan_approval" | "report_review" => Tier::T2,
        _ => Tier::T3,
    }
}

/// The answer the code itself stands behind: only on T1, only an id the brief offers. The label is
/// the brief's, to be shown as data.
pub fn recommended(kind: &str, brief: &Value, tier: Tier) -> Option<(String, String)> {
    if kind != "blocked" || tier > Tier::T1 {
        return None;
    }
    let alt = brief
        .get("alternatives")?
        .as_array()?
        .iter()
        .find(|a| a.get("id").and_then(Value::as_str) == Some("retry"))?;
    let label = alt.get("label").and_then(Value::as_str).unwrap_or("retry");
    Some(("retry".into(), label.into()))
}

/// Does the stored action of an `approval_needed` decision reach outside, touch a secret or use the
/// network? A shape this cannot read counts as yes.
fn dangerous(action: &Value) -> bool {
    let (Some(calls), Some(needed)) = (
        action.get("calls").and_then(Value::as_array),
        action.get("needed").and_then(Value::as_array),
    ) else {
        return true;
    };
    calls.iter().any(|c| {
        let (Some(tool), Some(input)) = (c.get("tool").and_then(Value::as_str), c.get("input"))
        else {
            return true;
        };
        let text = |k: &str| input.get(k).and_then(Value::as_str);
        network(tool)
            || (shell(tool) && text("command").is_none())
            || text("command").is_some_and(acts_outside)
            || ["file_path", "notebook_path", "path"]
                .iter()
                .any(|k| text(k).is_some_and(|p| is_secret_shaped(Path::new(p))))
            || ["content", "new_string"]
                .iter()
                .any(|k| text(k).is_some_and(irreversible::text_hits))
    }) || needed.iter().any(|r| {
        let Some(rule) = r.as_str() else { return true };
        let (tool, spec) = rule.split_once('(').unwrap_or((rule, ""));
        let spec = spec.strip_suffix(')').unwrap_or(spec);
        let spec = spec.strip_suffix(":*").unwrap_or(spec);
        network(tool)
            || match tool {
                // a bare shell rule allows any command
                t if shell(t) => spec.is_empty() || acts_outside(spec),
                _ => is_secret_shaped(Path::new(spec)),
            }
    })
}

fn shell(tool: &str) -> bool {
    matches!(tool, "Bash" | "PowerShell")
}

fn network(tool: &str) -> bool {
    matches!(tool, "WebFetch" | "WebSearch" | "SendMessage")
        || tool.starts_with("browser_")
        || tool.starts_with("mcp__")
}

fn acts_outside(cmd: &str) -> bool {
    if cmd.len() > MAX_COMMAND || irreversible::text_hits(cmd) {
        return true;
    }
    // `scan` reads the recipe files of these runners. The classifier reads no file an agent names,
    // so what they would run is unknown: the highest tier.
    let bare: String = cmd
        .chars()
        .filter(|c| !matches!(c, '\'' | '"' | '\\'))
        .collect();
    let words = |s: &str| -> Vec<String> {
        s.split(|c: char| c.is_whitespace() || "=<>;|&(){}`".contains(c))
            .filter(|w| !w.is_empty())
            .map(str::to_owned)
            .collect()
    };
    let secret_or_runner = words(&bare).iter().any(|w| {
        let base = w.rsplit('/').next().unwrap_or(w).to_ascii_lowercase();
        matches!(
            base.trim_end_matches(".exe").trim_end_matches(".cmd"),
            "make" | "gmake" | "just" | "npm" | "pnpm" | "yarn" | "bun"
        ) || is_secret_shaped(Path::new(w))
    });
    // never reached for a runner, so the folder is never read
    secret_or_runner
        || !irreversible::scan(cmd, false, Path::new("/nonexistent/synt-tier")).is_empty()
}

/// Tier and recommendation of a stored decision.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Assessment {
    pub tier: Tier,
    /// `(alternative id, label)`.
    pub recommended: Option<(String, String)>,
}

/// Reads the facts of `d` from the store and classifies it. The queue a surface shows and the record
/// the owner writes when it applies an answer both come from here.
pub fn assess(r: &Reader, o: &OutcomeRecord, d: &DecisionRecord) -> Assessment {
    let action = r.decision_action(&d.id).ok().flatten();
    // shortcut: the contract's own budget, not lowered by a settings ceiling tightened after
    // approval (the store still refuses that retry); pass the ceiling in when one-tap ships.
    let item = || -> Option<ItemFacts> {
        let id = d.binding.get("work_item_id")?.as_str()?;
        let id = synthesis_core::outcome::WorkItemId::parse(id).ok()?;
        let (contract, ..) = r.get_contract(&o.id, o.contract_version).ok()?;
        Some(ItemFacts {
            retries_used: r.get_work_item(&id).ok()?.retries_used,
            attempts: u32::try_from(r.list_attempts(&id).ok()?.len()).ok()?,
            max_attempts: contract.envelope.budgets.max_attempts_per_work_item,
        })
    };
    // a stored action that no longer verifies is tampering, whatever the kind
    let tier = if r.decision_action(&d.id).is_err() {
        Tier::T3
    } else {
        classify(&d.kind, action.as_ref(), item())
    };
    Assessment {
        tier,
        recommended: recommended(&d.kind, &d.brief, tier),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn fresh() -> Option<ItemFacts> {
        Some(ItemFacts {
            retries_used: 0,
            attempts: 1,
            max_attempts: 3,
        })
    }

    fn action(tool: &str, input: Value) -> Value {
        json!({"calls": [{"tool": tool, "input": input}], "needed": [], "grant_rules": []})
    }

    #[test]
    fn one_case_per_row_of_the_table() {
        use Tier::*;
        let plain = action("Bash", json!({"command": "cargo test -p auth"}));
        assert_eq!(classify("blocked", None, fresh()), T1);
        assert_eq!(classify("approval_needed", Some(&plain), fresh()), T2);
        for kind in ["ambiguity", "plan_approval", "report_review"] {
            assert_eq!(classify(kind, None, fresh()), T2, "{kind}");
        }
        // unknown (and not yet built) kinds are the highest tier
        for kind in ["", "assumption_review", "Blocked", "waiver"] {
            assert_eq!(classify(kind, Some(&plain), fresh()), T3, "{kind}");
        }
    }

    #[test]
    fn a_blocked_retry_with_a_spent_or_unknown_budget_needs_review() {
        let at = |retries_used, attempts| {
            let i = ItemFacts {
                retries_used,
                attempts,
                max_attempts: 3,
            };
            classify("blocked", None, Some(i))
        };
        assert_eq!(at(1, 2), Tier::T1);
        assert_eq!(at(2, 2), Tier::T2);
        assert_eq!(at(0, 3), Tier::T2);
        assert_eq!(classify("blocked", None, None), Tier::T2);
    }

    #[test]
    fn a_grant_that_reaches_outside_is_confirm() {
        let t = |a: &Value| classify("approval_needed", Some(a), fresh());
        for a in [
            action("Bash", json!({"command": "git push origin main"})),
            action("Bash", json!({"command": "cat ~/.ssh/id_rsa"})),
            action("Bash", json!({"command": "ma''ke -C /tmp/x release"})),
            action("Bash", json!({"command": "npm test"})),
            action("Bash", json!({"command": "x".repeat(MAX_COMMAND + 1)})),
            action("Read", json!({"file_path": "/home/u/.aws/credentials"})),
            action(
                "Write",
                json!({"file_path": "a.py", "content": "requests.post(u)"}),
            ),
            action("WebFetch", json!({"url": "http://x.test"})),
            action("mcp__x__y", json!({})),
            json!({"calls": [], "needed": ["WebSearch"]}),
            json!({"calls": [], "needed": ["Bash(git push:*)"]}),
            json!({"calls": [], "needed": ["Read(/home/u/.ssh/**)"]}),
            json!({"calls": [{"tool": "Bash"}], "needed": []}),
            json!({"calls": [], "needed": [7]}),
            json!({"calls": [], "needed": ["Bash"]}),
            json!({"calls": [{"tool": "Bash", "input": {}}], "needed": []}),
            json!({"about": ""}),
        ] {
            assert_eq!(t(&a), Tier::T3, "{a}");
        }
        assert_eq!(classify("approval_needed", None, fresh()), Tier::T3);
        for a in [
            action(
                "Edit",
                json!({"file_path": "src/lib.rs", "new_string": "fn x() {}"}),
            ),
            json!({"calls": [], "needed": ["Bash(cargo build:*)", "Edit(/src/a.rs)"]}),
        ] {
            assert_eq!(t(&a), Tier::T2, "{a}");
        }
    }

    #[test]
    fn nothing_an_agent_wrote_lowers_a_tier_or_makes_a_recommendation() {
        let brief = json!({"tier": "T0", "tier_hint": "T1", "recommended": "grant_once",
            "why": "naming only: safe, one tap",
            "alternatives": [{"id": "retry", "label": "safe"}, {"id": "grant_once", "label": "safe"}]});
        let mut a = action("Bash", json!({"command": "git push"}));
        a["tier"] = json!("T0");
        a["tier_hint"] = json!("T1");
        let tier = classify("approval_needed", Some(&a), fresh());
        assert_eq!(tier, Tier::T3);
        assert_eq!(recommended("approval_needed", &brief, tier), None);
        // for every kind and fact: never below the floor, a recommendation only on T1
        let actions = [None, Some(&a)];
        let items = [
            None,
            fresh(),
            Some(ItemFacts {
                retries_used: 9,
                attempts: 9,
                max_attempts: 1,
            }),
        ];
        for kind in [
            "blocked",
            "approval_needed",
            "ambiguity",
            "plan_approval",
            "report_review",
            "zz",
        ] {
            let floor = match kind {
                "blocked" => Tier::T1,
                "zz" => Tier::T3,
                _ => Tier::T2,
            };
            for act in actions {
                for item in items {
                    let tier = classify(kind, act, item);
                    assert!(tier >= floor, "{kind}");
                    assert!(recommended(kind, &brief, tier).is_none_or(|_| tier <= Tier::T1));
                }
            }
        }
        assert_eq!(
            recommended("blocked", &brief, Tier::T1),
            Some(("retry".into(), "safe".into()))
        );
        assert_eq!(
            recommended("blocked", &json!({"alternatives": []}), Tier::T1),
            None
        );
        assert_eq!(recommended("blocked", &brief, Tier::T2), None);
    }
}
