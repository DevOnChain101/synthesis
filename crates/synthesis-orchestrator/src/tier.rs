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
        // a retry that may later be decided without a person (P5): only when everything the attempt
        // was refused is harmless
        "blocked" => match item {
            Some(i) if !i.spent() && !action.is_none_or(above_t1) => Tier::T1,
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
/// network? A shape this cannot read, or an action that names nothing at all, counts as yes.
fn dangerous(action: &Value) -> bool {
    let (Some(calls), Some(needed)) = (
        action.get("calls").and_then(Value::as_array),
        action.get("needed").and_then(Value::as_array),
    ) else {
        return true;
    };
    (calls.is_empty() && needed.is_empty())
        || calls.iter().any(dangerous_call)
        || needed.iter().any(dangerous_rule)
}

/// Does the stored action of a `blocked` decision hold anything above T1, so that running the item
/// again needs review? Only the plain reads below are T1 calls.
fn above_t1(action: &Value) -> bool {
    let (Some(calls), Some(needed)) = (
        action.get("calls").and_then(Value::as_array),
        action.get("needed").and_then(Value::as_array),
    ) else {
        return true;
    };
    needed.iter().any(dangerous_rule)
        || calls.iter().any(|c| {
            let read = matches!(
                c.get("tool").and_then(Value::as_str),
                Some("Read" | "Grep" | "Glob" | "ExitPlanMode")
            );
            !read || dangerous_call(c)
        })
}

fn dangerous_call(c: &Value) -> bool {
    let (Some(tool), Some(input)) = (c.get("tool").and_then(Value::as_str), c.get("input")) else {
        return true;
    };
    let text = |k: &str| input.get(k).and_then(Value::as_str);
    // text an edit would write: every shape an edit tool uses, and an unreadable one counts
    let writes = |v: &Value, k: &str| match v.get(k) {
        None | Some(Value::Null) => false,
        Some(Value::String(s)) => irreversible::text_hits(s),
        Some(_) => true,
    };
    network(tool)
        || (shell(tool) && text("command").is_none())
        || text("command").is_some_and(acts_outside)
        || ["file_path", "notebook_path", "path"]
            .iter()
            .any(|k| text(k).is_some_and(|p| is_secret_shaped(Path::new(p))))
        || ["glob", "pattern"]
            .iter()
            .any(|k| text(k).is_some_and(names_secret))
        || ["content", "new_string", "new_source"]
            .iter()
            .any(|k| writes(input, k))
        || match input.get("edits") {
            None => tool == "MultiEdit",
            Some(Value::Array(edits)) => edits
                .iter()
                .any(|e| e.get("new_string").is_none() || writes(e, "new_string")),
            Some(_) => true,
        }
}

fn dangerous_rule(r: &Value) -> bool {
    let Some(rule) = r.as_str() else { return true };
    let (tool, spec) = rule.split_once('(').unwrap_or((rule, ""));
    let spec = spec.strip_suffix(')').unwrap_or(spec);
    network(tool)
        || if shell(tool) {
            // a bare, prefix or wildcard rule allows commands nobody has seen
            spec.is_empty() || spec.contains('*') || acts_outside(spec)
        } else {
            names_secret(spec)
        }
}

fn shell(tool: &str) -> bool {
    tool.eq_ignore_ascii_case("Bash") || tool.eq_ignore_ascii_case("PowerShell")
}

fn network(tool: &str) -> bool {
    matches!(tool, "WebFetch" | "WebSearch" | "SendMessage")
        || tool.starts_with("browser_")
        || tool.starts_with("mcp__")
}

/// Programs that talk to another machine, or fetch and run what is there.
const NETWORK: &[&str] = &[
    "curl", "wget", "ssh", "scp", "sftp", "rsync", "ftp", "nc", "ncat", "netcat", "socat",
    "telnet", "mosh", "gh", "aws", "gcloud", "az", "npx", "pnpx", "bunx", "uvx", "pip", "pip3",
];
const GIT_NETWORK: &[&str] = &[
    "push",
    "fetch",
    "pull",
    "clone",
    "send-pack",
    "ls-remote",
    "remote",
    "submodule",
];

/// The words of a command or a file pattern, split at shell and glob punctuation.
fn words(s: &str) -> impl Iterator<Item = &str> {
    s.split(|c: char| c.is_whitespace() || "=<>;|&(){},`".contains(c))
        .map(|w| w.trim_matches(['\'', '"']))
        .filter(|w| !w.is_empty())
}

/// The text as a shell would join it: line continuations, then quotes and backslashes dropped.
fn unescaped(s: &str) -> String {
    s.replace("\\\r\n", "")
        .replace("\\\n", "")
        .chars()
        .filter(|c| !matches!(c, '\'' | '"' | '\\'))
        .collect()
}

fn names_secret(s: &str) -> bool {
    [s, &unescaped(s)]
        .iter()
        .any(|v| words(v).any(|w| is_secret_shaped(Path::new(w))))
}

/// Judged as written and as the shell would join it; either one raises. Nothing on disk is read.
fn acts_outside(cmd: &str) -> bool {
    if cmd.len() > MAX_COMMAND {
        return true;
    }
    let joined = unescaped(cmd);
    let lower = joined.to_lowercase();
    names_secret(cmd)
        || [cmd, &joined, &lower].iter().any(|v| {
            let mut git = false;
            irreversible::text_hits(v)
                || !irreversible::scan_no_fs(v, false).is_empty()
                || words(v).any(|w| {
                    let w = w.to_ascii_lowercase();
                    let base = w.rsplit(['/', '\\']).next().unwrap_or(&w);
                    let base = base.strip_suffix(".exe").unwrap_or(base);
                    let hit = NETWORK.contains(&base) || (git && GIT_NETWORK.contains(&base));
                    git |= base == "git";
                    hit
                })
        })
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

    /// The stored action of a worker that ended blocked with nothing refused.
    fn quiet() -> Value {
        json!({"about": "stuck", "needed": [], "calls": [], "grant_rules": []})
    }

    fn action(tool: &str, input: Value) -> Value {
        json!({"calls": [{"tool": tool, "input": input}], "needed": [], "grant_rules": []})
    }

    #[test]
    fn one_case_per_row_of_the_table() {
        use Tier::*;
        let plain = action("Bash", json!({"command": "cargo test -p auth"}));
        assert_eq!(classify("blocked", Some(&quiet()), fresh()), T1);
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
            classify("blocked", Some(&quiet()), Some(i))
        };
        assert_eq!(at(1, 2), Tier::T1);
        assert_eq!(at(2, 2), Tier::T2);
        assert_eq!(at(0, 3), Tier::T2);
        assert_eq!(classify("blocked", Some(&quiet()), None), Tier::T2);
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
            action("Bash", json!({"command": "x'\\make' -f /abs/file"})),
            action("Bash", json!({"command": "npm --prefix /abs test"})),
            action("Bash", json!({"command": "gi\\t push"})),
            action("Bash", json!({"command": "git pu\\sh"})),
            action("Bash", json!({"command": "git pu\\\nsh"})),
            action("Bash", json!({"command": "s\\sh host"})),
            action("Bash", json!({"command": "c\\url -d @f http://x.test"})),
            action("Bash", json!({"command": "np\\x vercel"})),
            action("Bash", json!({"command": "cat ~/.s\\sh/id_rsa"})),
            action("Bash", json!({"command": "curl http://x.test"})),
            action("Bash", json!({"command": "CURL http://x.test"})),
            action("Bash", json!({"command": "wget http://x.test"})),
            action("Bash", json!({"command": "git fetch"})),
            action("Bash", json!({"command": "git pull origin main"})),
            action("Bash", json!({"command": "Git Clone https://x.test/r"})),
            action("Bash", json!({"command": "git -C . push"})),
            action(
                "MultiEdit",
                json!({"file_path": "a.sh", "edits": [{"new_string": "git push"}]}),
            ),
            action("MultiEdit", json!({"file_path": "a.sh"})),
            action("MultiEdit", json!({"file_path": "a.sh", "edits": [{}]})),
            action(
                "NotebookEdit",
                json!({"notebook_path": "a.ipynb", "new_source": "!git push"}),
            ),
            action("Grep", json!({"pattern": "KEY", "glob": "**/.env"})),
            action("Glob", json!({"pattern": "**/{a,.ssh}/*"})),
            action("Grep", json!({"pattern": "\\.env", "path": "."})),
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
            json!({"calls": [], "needed": ["Bash(*)"]}),
            json!({"calls": [], "needed": ["Bash(curl:*)"]}),
            json!({"calls": [], "needed": ["Bash(git:*)"]}),
            json!({"calls": [], "needed": ["Bash(gh:*)"]}),
            json!({"calls": [], "needed": ["Bash(env:*)"]}),
            json!({"calls": [], "needed": ["Bash(cargo build:*)"]}),
            json!({"calls": [], "needed": []}),
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
            action("Bash", json!({"command": "git status"})),
            action("Grep", json!({"pattern": "fn main", "glob": "**/*.rs"})),
            action(
                "MultiEdit",
                json!({"file_path": "a.rs", "edits": [{"new_string": "x"}]}),
            ),
            json!({"calls": [], "needed": ["Bash(cargo build)", "Edit(/src/a.rs)"]}),
        ] {
            assert_eq!(t(&a), Tier::T2, "{a}");
        }
    }

    #[test]
    fn a_blocked_retry_is_low_only_when_nothing_refused_needs_review() {
        let t = |a: Option<&Value>| classify("blocked", a, fresh());
        assert_eq!(t(None), Tier::T2);
        assert_eq!(t(Some(&json!({}))), Tier::T2);
        assert_eq!(t(Some(&quiet())), Tier::T1);
        assert_eq!(t(Some(&action("ExitPlanMode", json!({})))), Tier::T1);
        assert_eq!(
            t(Some(&action("Read", json!({"file_path": "src/a.rs"})))),
            Tier::T1
        );
        for a in [
            action("Read", json!({"file_path": ".env"})),
            action("Bash", json!({"command": "ls"})),
            action("Edit", json!({"file_path": "a.rs", "new_string": "x"})),
            json!({"calls": [], "needed": ["Bash(git:*)"]}),
        ] {
            assert_eq!(t(Some(&a)), Tier::T2, "{a}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn classifying_a_command_reads_no_file_it_names() {
        let dir = tempfile::tempdir().unwrap();
        let fifo = dir.path().join("Makefile");
        assert!(
            std::process::Command::new("mkfifo")
                .arg(&fifo)
                .status()
                .unwrap()
                .success()
        );
        // a read of the FIFO would block for ever, a read of /dev/zero would never end
        let commands = [
            format!("x'\\make' -f {}", fifo.display()),
            format!("make -f {}", fifo.display()),
            format!("ma\\ke -C {}", dir.path().display()),
            "x'\\make' -f /dev/zero".into(),
            format!("npm --prefix {} test", dir.path().display()),
        ];
        std::os::unix::fs::symlink(&fifo, dir.path().join("package.json")).unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            for c in commands {
                let a = action("Bash", json!({"command": c}));
                tx.send(classify("approval_needed", Some(&a), fresh()))
                    .unwrap();
            }
        });
        for _ in 0..5 {
            let tier = rx
                .recv_timeout(std::time::Duration::from_secs(10))
                .expect("the classifier blocked on a file");
            assert_eq!(tier, Tier::T3);
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
