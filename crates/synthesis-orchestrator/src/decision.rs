//! A blocked attempt becomes a durable Decision (ADR-019 sections 2 and 4), never a live ask.
//!
//! The worker has no one to ask: a tool call that needs a permission the contract does not give,
//! or a question to the person, ends the task `blocked` (exit 4) with the reason and the rules it
//! needed in its `task.ended` event. When the runner settles such an attempt it reads that event
//! from the attempt's session and stores a Decision any surface can list and answer, also after a
//! restart. The text in the brief comes from the model and tool output: it is data, truncated and
//! stripped of control characters, and it never carries a pre-selected choice.

use synthesis_core::outcome::{glob_matches, hidden_char};
use synthesis_events::{Event, Record};
use synthesis_session::outcome::{
    AttemptRecord, DecisionRecord, NewDecision, OutcomeWriter, WorkItemRecord,
};
use synthesis_session::{Error as StoreError, Store};
use time::OffsetDateTime;

use crate::compile_envelope;

const MAX_TEXT: usize = 300;
const MAX_RULES: usize = 10;

pub(crate) fn clean(s: &str) -> String {
    let t: String = s.chars().filter(|c| !hidden_char(*c)).collect();
    if t.chars().count() > MAX_TEXT {
        format!("{}...", t.chars().take(MAX_TEXT).collect::<String>())
    } else {
        t
    }
}

fn ts(t: OffsetDateTime) -> String {
    let fmt = time::macros::format_description!(
        "[year]-[month]-[day]T[hour]:[minute]:[second].[subsecond digits:3]Z"
    );
    t.format(&fmt).expect("format utc timestamp")
}

/// What the blocked worker said, from its session's events.
struct Blocked {
    reason: String,
    /// Untruncated, as the worker reported them: the action hash is over these.
    needed: Vec<String>,
    question: Option<(String, Vec<String>)>,
    /// The worker asked to leave plan mode (`ExitPlanMode`).
    plan: bool,
    /// Calls that needed an approval nobody could give (tool, raw input).
    asked: Vec<(String, serde_json::Value)>,
    /// Calls the contract's own rules refused (tool, raw input).
    denied: Vec<(String, serde_json::Value)>,
}

fn blocked_of(store: &Store, session_id: &str) -> Result<Option<Blocked>, StoreError> {
    // all of the session's events, a page at a time (a long session must not lose its `task.ended`)
    let reader = store.reader()?;
    let mut records = vec![];
    let mut cursor = 0u64;
    loop {
        let page = reader.read_events(session_id, cursor, 5_000)?;
        let Some(last) = page.last() else { break };
        cursor = match last {
            Record::Known(e) => e.seq,
            Record::Unknown(r) => r.seq,
        };
        records.extend(page);
    }
    let (mut ended, mut question) = (None, None);
    let mut requested = std::collections::BTreeMap::new();
    let (mut asked, mut denied) = (vec![], vec![]);
    let mut plan = false;
    for r in records {
        if let Record::Known(e) = r {
            match e.event {
                Event::TaskEnded {
                    reason,
                    needed_permissions,
                    ..
                } => ended = Some((reason, needed_permissions)),
                Event::ToolRequested {
                    tool_call_id,
                    name,
                    input,
                    ..
                } => {
                    requested.insert(tool_call_id, (name, input));
                }
                Event::ToolCompleted {
                    tool_call_id,
                    not_run: true,
                    preview,
                    ..
                } => {
                    if let Some((name, input)) = requested.get(&tool_call_id) {
                        let call = (name.clone(), input.clone());
                        if preview.trim_start().starts_with("Permission denied") {
                            denied.push(call);
                        } else {
                            asked.push(call);
                        }
                    }
                }
                Event::ToolApprovalRequired {
                    dialog,
                    prompt,
                    options,
                    ..
                } if dialog == "question" => question = Some((prompt, options)),
                Event::ToolApprovalRequired { dialog, .. } if dialog == "plan" => plan = true,
                _ => {}
            }
        }
    }
    Ok(ended.map(|(reason, needed)| Blocked {
        reason,
        needed,
        question,
        plan,
        asked,
        denied,
    }))
}

/// Longest needed rule (or deny spec) the coverage check will look at: beyond it (or when the glob
/// matcher's work budget is spent) the answer is "covered", i.e. no grant is offered.
const MAX_RULE_LEN: usize = 1_000;

/// Is the rule the worker needed one that must not be offered as a grant: covered by a deny rule
/// of the compiled contract or a tool that does not exist in an attempt? Covered means:
/// identical; a bare deny of the tool (the edit tools count as one family); a path deny whose
/// glob matches the needed path (`Edit(/s/**)` covers `Edit(/s/k)`); a command-prefix deny
/// (`Bash(git push:*)` covers `Bash(git push origin:*)`); and the other way round, a BROADER need
/// that contains a deny (`Bash(git:*)` contains the deny of `git push`, `Read(/**)` contains the
/// deny of `/s/**`, a bare `Edit` contains any edit deny). Case is folded like the engine does on
/// case-insensitive file systems, and anything too long or too costly to check counts as covered.
/// This errs toward NOT offering a grant; it is a best-effort FILTER for what the brief offers,
/// not an authority: PR-10 must check a grant against the contract itself.
pub(crate) fn rule_denied(deny: &[String], rule: &str) -> bool {
    let family = |t: &str| match t {
        "Write" | "MultiEdit" | "NotebookEdit" => "Edit".to_owned(),
        t => t.to_owned(),
    };
    let split = |r: &str| -> (String, String) {
        let (t, rest) = r.split_once('(').unwrap_or((r, ""));
        (
            family(t).to_lowercase(),
            rest.strip_suffix(')').unwrap_or(rest).to_lowercase(),
        )
    };
    if rule.len() > MAX_RULE_LEN {
        return true;
    }
    let (tool, need) = split(rule);
    // an attempt has no browser and no MCP tools: a grant could never make them exist
    if tool.starts_with("browser_") || tool.starts_with("mcp__") {
        return true;
    }
    let prefix = |s: &str| s.strip_suffix(":*").map(str::to_owned);
    deny.iter().any(|d| {
        let (dt, spec) = split(d);
        if d.to_lowercase() == rule.to_lowercase() {
            return true;
        }
        if dt != tool {
            return false;
        }
        if spec.is_empty() || need.is_empty() || spec == need || spec.len() > MAX_RULE_LEN {
            return true; // a bare deny, or a bare need (which contains every deny of the tool)
        }
        // the deny's glob matches the needed path, or the needed glob matches the deny's text
        // (a broader need contains the deny)
        let covers = |p: &str, t: &str| glob_matches(p.as_bytes(), t.as_bytes()).unwrap_or(true);
        if covers(&spec, &need) || covers(&need, &spec) {
            return true;
        }
        match (prefix(&spec), prefix(&need)) {
            (Some(dp), Some(np)) => np.starts_with(&dp) || dp.starts_with(&np),
            (Some(dp), None) => need.starts_with(&dp),
            _ => false,
        }
    })
}

/// The exact allow rule for one refused call, derived from its raw input (never from text a
/// person or a model wrote for the brief): an edit becomes `Edit(/<path relative to the
/// worktree>)`, a shell command `Bash(<that command>)` (an exact rule, not a prefix). `None` for
/// anything else, a path outside the worktree, or text the rule syntax cannot carry safely.
pub(crate) fn scoped_rule(
    tool: &str,
    input: &serde_json::Value,
    worktree: &std::path::Path,
) -> Option<String> {
    let printable =
        |t: &str| !t.is_empty() && t.len() <= 300 && t.chars().all(|c| matches!(c, ' '..='~'));
    match tool {
        "Write" | "Edit" | "MultiEdit" | "NotebookEdit" => {
            let p = input
                .get("file_path")
                .or_else(|| input.get("notebook_path"))?
                .as_str()?;
            let path = std::path::Path::new(p);
            let rel = if path.is_absolute() {
                path.strip_prefix(worktree)
                    .map(std::path::Path::to_path_buf)
                    .or_else(|_| {
                        let canon = std::fs::canonicalize(worktree).map_err(|_| ())?;
                        path.strip_prefix(&canon)
                            .map(std::path::Path::to_path_buf)
                            .map_err(|_| ())
                    })
                    .ok()?
            } else {
                path.to_path_buf()
            };
            if !rel
                .components()
                .all(|c| matches!(c, std::path::Component::Normal(_)))
            {
                return None;
            }
            let text = rel.to_str()?.to_owned();
            let plain = printable(&text)
                && !text.chars().any(|c| {
                    matches!(
                        c,
                        '(' | ')' | '[' | ']' | '{' | '}' | '!' | '*' | '?' | '\\'
                    )
                })
                && !text.is_empty();
            if !plain {
                return None;
            }
            // protected paths (.git, .synt, AGENTS.md, CI and shell startup files ...) are never granted
            let full = worktree.join(&rel);
            if synthesis_permissions::is_protected_path(&[worktree.to_path_buf()], &full) {
                return None;
            }
            // a link on the way (or as the file) would make the lexical rule mean another place
            let mut walk = worktree.to_path_buf();
            for c in rel.components() {
                walk.push(c);
                if std::fs::symlink_metadata(&walk).is_ok_and(|m| m.file_type().is_symlink()) {
                    return None;
                }
            }
            Some(format!("Edit(/{text})"))
        }
        "Bash" => {
            let c = input.get("command")?.as_str()?;
            // The engine reads `*` and a trailing `:*` in a Bash rule as a glob or a prefix: a
            // command containing one would grant more than that command. Empty and padded text too.
            (printable(c) && c == c.trim() && !c.contains(['(', ')', '*']))
                .then(|| format!("Bash({c})"))
        }
        _ => None,
    }
}

/// Stores (or finds) the Decision for a `blocked` attempt and makes it `pending`. `Ok(None)` when
/// the session says nothing about why it blocked.
pub fn record_blocked_decision(
    store: &Store,
    w: &mut OutcomeWriter,
    item: &WorkItemRecord,
    attempt: &AttemptRecord,
    subject_commit: Option<&str>,
    worktree: &std::path::Path,
    now: OffsetDateTime,
) -> Result<Option<DecisionRecord>, StoreError> {
    let Some(b) = blocked_of(store, &attempt.session_id)? else {
        return Ok(None);
    };
    // the caller's record may predate the start (the policy binding is written then)
    let fresh = w
        .list_attempts(&item.id)?
        .into_iter()
        .find(|a| a.id == attempt.id)
        .unwrap_or_else(|| attempt.clone());
    let attempt = &fresh;
    let outcome = w.get_outcome(w.id())?;
    // the contract and policy the attempt STARTED under (an outcome that moved on since is stale)
    let contract_version = attempt
        .bound_contract_version
        .unwrap_or(outcome.contract_version);
    let (contract, ..) = w.get_contract(w.id(), contract_version)?;
    let compiled = compile_envelope(&contract.envelope, &[]).ok();
    let policy_hash = attempt.bound_policy_hash.clone().unwrap_or_else(|| {
        compiled
            .as_ref()
            .map_or_else(|| "uncompiled".to_owned(), |c| c.policy_hash())
    });
    // a rule the contract itself denies is never offered as a grant
    let denied_by_contract =
        |rule: &str| compiled.as_ref().is_none_or(|c| rule_denied(&c.deny, rule));
    let rules: Vec<String> = b.needed.iter().take(MAX_RULES).cloned().collect();
    // every needed rule is checked, not only the first few shown; and a grant is not offered for
    // a list too long to review
    // the rules a grant would carry: one exact rule per call that asked, from the raw calls; a call
    // that has none (another tool, a path outside the worktree) makes the whole action ungrantable
    let scoped: Vec<Option<String>> = b
        .asked
        .iter()
        .map(|(t, i)| scoped_rule(t, i, worktree))
        .collect();
    let mut grant_rules: Vec<String> = scoped.iter().flatten().cloned().collect();
    grant_rules.sort();
    grant_rules.dedup();
    let grantable = !b.needed.is_empty()
        && b.needed.len() <= MAX_RULES
        && b.needed.iter().map(String::len).sum::<usize>() <= 10 * MAX_RULE_LEN
        && !b.needed.iter().any(|r| denied_by_contract(r))
        && !b.asked.is_empty()
        && scoped.iter().all(Option::is_some)
        && !grant_rules.is_empty()
        && grant_rules.len() <= MAX_RULES
        && !grant_rules.iter().any(|r| denied_by_contract(r));
    let shown_rules: Vec<String> = rules.iter().map(|r| clean(r)).collect();
    let refused: Vec<String> = b
        .asked
        .iter()
        .chain(&b.denied)
        .take(MAX_RULES)
        .map(|(t, i)| clean(&format!("{t}: {i}")))
        .collect();
    // The FULL action is stored with the decision (the store hashes it into the binding); the brief
    // shows a cleaned, shortened view. A grant is derived from the stored action, never from the
    // brief or from anything a caller says.
    let full_action = |extra: &str| {
        serde_json::json!({
            "about": extra,
            "needed": b.needed,
            "calls": b.asked.iter().map(|(t, i)| serde_json::json!({"tool": t, "input": i, "asked": true}))
                .chain(b.denied.iter().map(|(t, i)| serde_json::json!({"tool": t, "input": i, "asked": false})))
                .collect::<Vec<_>>(),
            "grant_rules": if grantable { grant_rules.clone() } else { vec![] },
        })
    };
    let (kind, why, delta, alternatives, action) = match (&b.question, b.reason.as_str()) {
        (Some((prompt, options)), _) if b.reason == "needs_user_input" => (
            "ambiguity",
            "the worker needs an answer to continue".to_owned(),
            clean(prompt),
            if options.is_empty() {
                vec![serde_json::json!({"id": "answer", "label": "answer the question"})]
            } else {
                options
                    .iter()
                    .take(8)
                    .map(|o| serde_json::json!({"id": clean(o), "label": clean(o)}))
                    .collect()
            },
            serde_json::json!({"question": prompt, "options": options}),
        ),
        _ if b.plan && b.reason == "needs_user_input" => (
            "blocked",
            "the worker asked to leave plan mode, which an attempt cannot do".to_owned(),
            String::new(),
            vec![serde_json::json!({"id": "retry", "label": "run the work item again"})],
            full_action("ExitPlanMode"),
        ),
        _ if b.reason == "approval_unavailable" || !rules.is_empty() => {
            let alternatives = if grantable {
                vec![serde_json::json!({
                    "id": "grant_once",
                    "label": format!("allow exactly {} in one new attempt", grant_rules.iter().map(|r| clean(r)).collect::<Vec<_>>().join(", "))
                })]
            } else {
                vec![serde_json::json!({"id": "retry", "label": "run the work item again"})]
            };
            (
                "approval_needed",
                if grantable {
                    "the worker needed a permission the contract does not give".to_owned()
                } else {
                    "the worker needed something the contract denies: a grant is not offered; change the contract (a new version) instead".to_owned()
                },
                format!(
                    "not allowed by the contract: {} (refused calls: {})",
                    shown_rules.join(", "),
                    refused.join("; ")
                ),
                alternatives,
                full_action(""),
            )
        }
        _ => (
            "blocked",
            format!("the worker ended blocked: {}", clean(&b.reason)),
            String::new(),
            vec![serde_json::json!({"id": "retry", "label": "run the work item again"})],
            full_action(&b.reason),
        ),
    };
    let truncated = b.needed.len() > MAX_RULES
        || b.needed
            .iter()
            .any(|t| clean(t).chars().count() != t.chars().count())
        || b.asked.iter().chain(&b.denied).any(|(t, i)| {
            let text = format!("{t}: {i}");
            clean(&text).chars().count() != text.chars().count()
        });
    let brief = serde_json::json!({
        "why": why,
        "delta_from_contract": delta,
        "alternatives": alternatives,
        "evidence": {"work_item": item.key, "attempt_ordinal": attempt.ordinal,
                     "attempt_session": attempt.session_id, "reason": clean(&b.reason),
                     "refused_calls": refused,
                     "text_shortened_or_cleaned": truncated},
        "unknowns": [],
        "safe_default": "HOLD",
    });
    // `action_hash` is the store's: it overwrites this placeholder with the hash of the stored action
    let binding = crate::grants::decision_binding(
        contract_version,
        outcome.plan_version.unwrap_or(item.plan_version),
        subject_commit,
        &policy_hash,
        "",
        attempt.ordinal,
        &item.id,
    );
    let hours = i64::from(w.decision_expiry_hours().min(24));
    let mut key = format!("{}:{}:{kind}", item.key, attempt.ordinal);
    key.truncate(128);
    let (d, fresh) = w.new_decision(&NewDecision {
        kind: kind.into(),
        dedupe_key: key,
        brief,
        binding: binding.clone(),
        expires_at: ts(now + time::Duration::hours(hours)),
        action: Some(action),
    })?;
    if !fresh {
        return Ok(Some(d));
    }
    w.transition_decision(
        &d.id,
        d.revision,
        synthesis_core::outcome::DecisionState::Pending,
        &d.binding,
        &synthesis_session::outcome::Effects::default(),
    )?;
    Ok(w.list_decisions(w.id())?.into_iter().find(|x| x.id == d.id))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn invisible_and_direction_changing_characters_are_removed() {
        let evil = "a\u{202E}b\u{2066}c\u{200B}d\u{FEFF}e\u{2028}f\u{E0041}g\u{1b}[31mh\u{00AD}i";
        assert_eq!(clean(evil), "abcdefg[31mhi");
        assert!(clean(&"x".repeat(1000)).chars().count() < 310);
    }

    #[test]
    fn a_rule_the_contract_denies_is_not_grantable() {
        let deny: Vec<String> = ["Bash(git push:*)", "Edit", "Read(/s/**)"]
            .map(String::from)
            .to_vec();
        assert!(rule_denied(&deny, "Bash(git push:*)"));
        assert!(rule_denied(&deny, "Write"), "the edit tools are one family");
        assert!(rule_denied(&deny, "Edit(/src/x)"));
        assert!(rule_denied(&deny, "NotebookEdit(/n)"));
        assert!(rule_denied(&deny, "Read(/s/**)"));
        // path and command-prefix coverage
        assert!(rule_denied(&deny, "Read(/s/deep/k)"));
        assert!(rule_denied(&deny, "Bash(git push origin main:*)"));
        assert!(!rule_denied(&deny, "Read(/t/k)"));
        assert!(!rule_denied(&deny, "Bash(git status:*)"));
        let globs: Vec<String> = vec!["Edit(/src/*.rs)".into(), "Read(/a/**/k)".into()];
        assert!(rule_denied(&globs, "Write(/src/lib.rs)"));
        assert!(
            !rule_denied(&globs, "Write(/src/sub/lib.rs)"),
            "* stays in a segment"
        );
        assert!(rule_denied(&globs, "Read(/a/b/c/k)"));
        // a broader need contains a deny; case is folded
        let d2: Vec<String> = vec!["Bash(git push:*)".into(), "Read(/s/**)".into()];
        assert!(rule_denied(&d2, "Bash(git:*)"));
        assert!(rule_denied(&d2, "Read(/**)"));
        assert!(rule_denied(&d2, "Read(/S/K)"));
        assert!(rule_denied(&d2, "BASH(GIT PUSH:*)"));
        assert!(rule_denied(&["Edit(/src/x)".to_string()], "Edit"));
        assert!(!rule_denied(&d2, "Bash(cargo:*)"));
        assert!(!rule_denied(&d2, "Read(/other/k)"));
        assert!(!rule_denied(&deny, "Read(/other)"));
        assert!(!rule_denied(&deny, "Bash(cargo test:*)"));
        assert!(rule_denied(&deny, "browser_navigate(domain:x)"));
        assert!(rule_denied(&deny, "mcp__s__t"));
    }

    #[test]
    fn a_pathological_glob_costs_bounded_time_and_counts_as_covered() {
        let t0 = std::time::Instant::now();
        // ~170 wildcard groups against a long near-miss text: backtracking would never finish
        let pat = format!("/{}z", "*a".repeat(170));
        let text = format!("/{}", "a".repeat(900));
        let deny = vec![format!("Edit({pat})")];
        // within budget the answer is exact (no match), over budget it is "covered"
        let _ = rule_denied(&deny, &format!("Edit({text})"));
        // far over the text cap: covered without looking
        assert!(rule_denied(&deny, &format!("Edit({})", "a".repeat(5_000))));
        // collapsed wildcards give the same answers as the plain ones
        assert_eq!(glob_matches(b"/s/***/k", b"/s/a/b/k"), Some(true));
        assert_eq!(glob_matches(b"/s/*/k", b"/s/a/b/k"), Some(false));
        assert_eq!(glob_matches(b"/s/**/k", b"/s/a/b/k"), Some(true));
        assert_eq!(glob_matches(b"a?c", b"abc"), Some(true));
        assert_eq!(glob_matches(b"a?c", b"a/c"), Some(false));
        assert!(
            t0.elapsed() < std::time::Duration::from_secs(5),
            "{:?}",
            t0.elapsed()
        );
    }

    // attempts run on unix (native Windows refuses them), and `/w/tree` is not an absolute path there
    #[cfg(unix)]
    #[test]
    fn a_grant_rule_is_exact_and_comes_from_the_raw_call() {
        use serde_json::json;
        let wt = std::path::Path::new("/w/tree");
        let edit = |p: &str| scoped_rule("Write", &json!({"file_path": p}), wt);
        assert_eq!(edit("outside.txt").as_deref(), Some("Edit(/outside.txt)"));
        assert_eq!(edit("/w/tree/src/a.rs").as_deref(), Some("Edit(/src/a.rs)"));
        assert_eq!(
            scoped_rule("NotebookEdit", &json!({"notebook_path": "n/x.ipynb"}), wt).as_deref(),
            Some("Edit(/n/x.ipynb)")
        );
        // outside the worktree, climbing out, globs, rule syntax, odd text: no rule
        for bad in [
            "/etc/passwd",
            "../x",
            "a/../../x",
            "a/*.rs",
            "src/(x)",
            "a[1]",
            "a\\b",
            "",
            "bad\u{1b}",
        ] {
            assert_eq!(edit(bad), None, "{bad:?}");
        }
        // a command becomes an exact rule; anything the syntax cannot carry does not
        let bash = |c: &str| scoped_rule("Bash", &json!({"command": c}), wt);
        assert_eq!(bash("cargo test").as_deref(), Some("Bash(cargo test)"));
        // `*` and a trailing `:*` are read by the engine as a glob and a prefix: never an exact rule
        for bad in [
            "",
            "echo (x)",
            "a\nb",
            &"x".repeat(301),
            "python:*",
            "*",
            "ls *",
            "git * main",
            "ls*",
            " ls",
            "ls ",
        ] {
            assert_eq!(bash(bad), None, "{bad:?}");
        }
        // protected paths are never granted
        for p in [
            ".git/config",
            ".git/hooks/pre-commit",
            "sub/.git/config",
            ".synt/settings.json",
            "AGENTS.md",
            ".envrc",
            ".github/workflows/x.yml",
        ] {
            assert_eq!(edit(p), None, "{p}");
        }
        // other tools are never grantable
        for t in ["Read", "WebFetch", "browser_navigate", "mcp__s__t", "Task"] {
            assert_eq!(
                scoped_rule(
                    t,
                    &json!({"file_path": "a", "command": "b", "url": "c"}),
                    wt
                ),
                None
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_link_inside_the_worktree_makes_an_edit_ungrantable() {
        use serde_json::json;
        let d = tempfile::tempdir().unwrap();
        let wt = d.path();
        std::fs::create_dir_all(wt.join("real")).unwrap();
        std::fs::write(wt.join("a.txt"), "x").unwrap();
        std::os::unix::fs::symlink("/etc", wt.join("dirlink")).unwrap();
        std::os::unix::fs::symlink(wt.join("a.txt"), wt.join("filelink")).unwrap();
        let edit = |p: &str| scoped_rule("Write", &json!({"file_path": p}), wt);
        // a plain directory, an existing file and a new file are fine (the control)
        assert_eq!(edit("real/new.txt").as_deref(), Some("Edit(/real/new.txt)"));
        assert_eq!(edit("a.txt").as_deref(), Some("Edit(/a.txt)"));
        // a link as a parent directory or as the file itself: the lexical rule would mean another place
        assert_eq!(edit("dirlink/x"), None);
        assert_eq!(edit("filelink"), None);
    }
}
