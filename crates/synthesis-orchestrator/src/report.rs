//! The report of a read-only goal's attempt (S1 section 2.3): the worker's final message, which is
//! untrusted data. `report:sha256:<hex>` is the attempt's subject. When the attempt settles,
//! [`open_review`] checks its citations against the goal folder (no model) and stores a
//! `report_review` decision; only the person's `accept_report` answer writes the `human_review`
//! evidence (in the store, in the transaction of the answer), and [`review_verdict`] reads that
//! back. Nothing here marks anything verified.

use sha2::{Digest, Sha256};
use synthesis_core::outcome::{ContractKind, DecisionState, TrustClass, hidden_char};
use synthesis_core::secrets::is_secret_shaped;
use synthesis_session::outcome::{
    AttemptRecord, Effects, NewDecision, NewEvidence, OutcomeWriter, WorkItemRecord,
};
use synthesis_session::{Result, Store};
use time::OffsetDateTime;

use crate::decision::{clean, ts};
use crate::sched::{SchedError, Verdict};

/// What the worker said last, as written to its session. Model text: data, never instructions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttemptReport {
    pub text: String,
    pub sha256: String,
}

impl AttemptReport {
    /// `attempts.subject_commit` of a read-only attempt: no commit exists, the report is the subject.
    pub fn subject(&self) -> String {
        format!("report:sha256:{}", self.sha256)
    }
}

/// The final assistant message of `session_id`. P2b: show exactly the hashed text, recheck the
/// hash when it is displayed, and treat it as untrusted (strip terminal escapes, no links, never
/// pass it to a model).
///
/// The final assistant message of `session_id`, `None` when it wrote none (or the store cannot say).
pub fn attempt_report(store: &Store, session_id: &str) -> Option<AttemptReport> {
    let text = crate::planrun::read_session(store, session_id).0?;
    let sha256 = Sha256::digest(text.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    Some(AttemptReport { text, sha256 })
}

/// Is the outcome a read-only goal? Read from the stored contract of the outcome's current version
/// (never from a flag a caller passes), so what runs is what the person approved.
pub fn is_read_only(w: &OutcomeWriter) -> Result<bool> {
    let v = w.get_outcome(w.id())?.contract_version;
    Ok(w.get_contract(w.id(), v)?.0.kind == ContractKind::ReadOnly)
}

const REPORT_MAX: usize = 64 * 1024;
const CITES_MAX: usize = 200;
const QUOTE_MAX: usize = 400;
const FILE_MAX: u64 = 4 << 20;
/// Most lines one citation may span (a citation is a pointer, not a copy of the file).
const SPAN_MAX: u64 = 1_000;

/// One cited place: model text, never trusted until [`check_citation`] read it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Citation {
    pub path: String,
    pub start_line: u64,
    pub end_line: u64,
    pub quote: String,
}

/// The deliverable: `{report, citations}` as JSON (a code fence around it is tolerated); anything
/// else is the report as it is, with no citations.
pub fn parse_report(text: &str) -> (String, Vec<Citation>) {
    let t = text.trim();
    let t = t
        .strip_prefix("```json")
        .or_else(|| t.strip_prefix("```"))
        .unwrap_or(t);
    let t = t.strip_suffix("```").unwrap_or(t).trim();
    let Some(o) = serde_json::from_str::<serde_json::Value>(t)
        .ok()
        .and_then(|v| v.as_object().cloned())
    else {
        return (text.to_owned(), vec![]);
    };
    let Some(report) = o.get("report").and_then(|r| r.as_str()) else {
        return (text.to_owned(), vec![]);
    };
    let cites = o
        .get("citations")
        .and_then(|c| c.as_array())
        .map(|a| {
            a.iter()
                .take(CITES_MAX)
                // a malformed entry stays in the list as an empty one, which fails its check
                .map(|c| Citation {
                    path: c
                        .get("path")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_owned(),
                    start_line: c.get("start_line").and_then(|v| v.as_u64()).unwrap_or(0),
                    end_line: c.get("end_line").and_then(|v| v.as_u64()).unwrap_or(0),
                    quote: c
                        .get("quote")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_owned(),
                })
                .collect()
        })
        .unwrap_or_default();
    (report.to_owned(), cites)
}

/// What the owner found when it read one citation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Checked {
    pub ok: bool,
    /// Why not (or `verified`), plain ASCII.
    pub why: &'static str,
    /// sha256 of the file as it was read at check time.
    pub sha256: String,
}

fn norm(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Opens `p` as a regular file without following a link at the last place (and without blocking).
fn open_plain(p: &std::path::Path) -> std::io::Result<std::fs::File> {
    let mut o = std::fs::OpenOptions::new();
    o.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        o.custom_flags((rustix::fs::OFlags::NOFOLLOW | rustix::fs::OFlags::NONBLOCK).bits() as i32);
    }
    o.open(p)
}

/// The owner's check of one citation (no model): a plain relative path that stays inside `root`
/// (no `..`, no absolute path, no link on the way or at the end), never a secret or a state path,
/// at most 4 MiB read, and the quote appears within the cited lines (whitespace normalized).
pub fn check_citation(root: &std::path::Path, c: &Citation) -> Checked {
    use std::io::Read;
    use std::path::Component;
    let no = |why| Checked {
        ok: false,
        why,
        sha256: String::new(),
    };
    let rel = std::path::Path::new(&c.path);
    if c.path.is_empty()
        || c.path.len() > 512
        || c.path
            .chars()
            .any(|ch| hidden_char(ch) || matches!(ch, '\\' | ':'))
        || rel.components().any(|x| !matches!(x, Component::Normal(_)))
    {
        return no("the path is not a plain path inside the folder");
    }
    let state = |x: Component| {
        x.as_os_str().to_str().is_none_or(|n| {
            matches!(
                n.to_ascii_lowercase().as_str(),
                ".git" | ".synt" | ".synthesis"
            )
        })
    };
    if rel.components().any(state) || is_secret_shaped(rel) {
        return no("a secret or state path is never cited");
    }
    let quote = norm(&c.quote);
    if quote.is_empty() || c.quote.chars().count() > QUOTE_MAX {
        return no("the quote is empty or longer than 400 characters");
    }
    if c.start_line == 0 || c.end_line < c.start_line || c.end_line - c.start_line >= SPAN_MAX {
        return no("the line range is not valid");
    }
    let mut p = root.to_path_buf();
    for part in rel.components() {
        p.push(part);
        match std::fs::symlink_metadata(&p) {
            Ok(m) if m.file_type().is_symlink() => return no("a symbolic link is not followed"),
            Ok(_) => {}
            Err(_) => return no("the file does not exist"),
        }
    }
    let Ok(f) = open_plain(&p) else {
        return no("the file cannot be opened");
    };
    if !f.metadata().is_ok_and(|m| m.is_file()) {
        return no("not a regular file");
    }
    let mut bytes = vec![];
    if f.take(FILE_MAX + 1).read_to_end(&mut bytes).is_err() {
        return no("the file cannot be read");
    }
    if bytes.len() as u64 > FILE_MAX {
        return no("the file is larger than 4 MiB");
    }
    let sha256: String = Sha256::digest(&bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    let text = String::from_utf8_lossy(&bytes);
    let window: Vec<&str> = text
        .lines()
        .skip(c.start_line as usize - 1)
        .take((c.end_line - c.start_line + 1) as usize)
        .collect();
    if norm(&window.join("\n")).contains(&quote) {
        Checked {
            ok: true,
            why: "verified",
            sha256,
        }
    } else {
        Checked {
            ok: false,
            why: "the quote is not in the cited lines",
            sha256,
        }
    }
}

/// Model text for a person to read: control and hidden characters replaced (and counted), newlines
/// and tabs kept, at most [`REPORT_MAX`] bytes.
fn show(text: &str) -> (String, usize, bool) {
    let mut hidden = 0;
    let mut out = String::new();
    for ch in text.chars() {
        if out.len() + ch.len_utf8() > REPORT_MAX {
            return (out, hidden, true);
        }
        if hidden_char(ch) && !matches!(ch, '\n' | '\t') {
            hidden += 1;
            out.push('\u{FFFD}');
        } else {
            out.push(ch);
        }
    }
    (out, hidden, false)
}

fn is_review_of(
    d: &synthesis_session::outcome::DecisionRecord,
    item: &WorkItemRecord,
    a: &AttemptRecord,
) -> bool {
    d.kind == "report_review"
        && d.binding.get("work_item_id").and_then(|v| v.as_str()) == Some(item.id.as_str())
        && d.binding.get("attempt_ordinal").and_then(|v| v.as_u64()) == Some(u64::from(a.ordinal))
        && d.binding.get("subject_commit").and_then(|v| v.as_str()) == a.subject_commit.as_deref()
}

/// The attempt settled with `report` as its subject: checks the citations, then stores the
/// citation-check evidence row and the `report_review` decision in ONE transaction and opens the
/// decision. Replays find the decision and do nothing (a crash leaves either nothing or all of
/// it; a decision still `preparing` is opened). Never asks a model, never marks anything verified.
pub fn open_review(
    w: &mut OutcomeWriter,
    item: &WorkItemRecord,
    attempt: &AttemptRecord,
    report: &AttemptReport,
    now: OffsetDateTime,
) -> Result<()> {
    // the caller's record may predate the end of the attempt (its subject is set then)
    let mut a = w
        .list_attempts(&item.id)?
        .into_iter()
        .find(|x| x.id == attempt.id)
        .unwrap_or_else(|| attempt.clone());
    a.subject_commit = Some(report.subject());
    let existing = w
        .list_decisions(w.id())?
        .into_iter()
        .find(|d| is_review_of(d, item, &a));
    if let Some(d) = existing {
        return open(w, &d);
    }
    let o = w.get_outcome(w.id())?;
    let (contract, ..) = w.get_contract(w.id(), o.contract_version)?;
    let (text, cites) = parse_report(&report.text);
    let (shown, hidden, cut) = show(&text);
    let checks: Vec<(Citation, Checked)> = cites
        .into_iter()
        .map(|c| {
            let r = check_citation(std::path::Path::new(&o.repo_root), &c);
            (c, r)
        })
        .collect();
    let passed = checks.iter().all(|(_, r)| r.ok);
    let ids: Vec<&str> = {
        let mut v: Vec<&str> = contract.criteria.iter().map(|c| c.id.as_str()).collect();
        v.sort_unstable();
        v
    };
    let action = serde_json::json!({
        "attempt": a.id, "report_sha256": report.sha256, "criteria": ids,
    });
    let policy = crate::compile_envelope(&contract.envelope, &[])
        .map_or_else(|_| "uncompiled".to_owned(), |c| c.policy_hash());
    let binding = crate::grants::decision_binding(
        o.contract_version,
        o.plan_version.unwrap_or(0),
        a.subject_commit.as_deref(),
        &policy,
        "",
        a.ordinal,
        &item.id,
    );
    let verified = checks.iter().filter(|(_, r)| r.ok).count();
    let alternatives = if passed {
        vec![serde_json::json!({"id": "accept_report",
            "label": "accept this report: it answers every rubric line above"})]
    } else {
        vec![]
    };
    let brief = serde_json::json!({
        "why": "a read-only worker finished. Its report is the worker's own, untrusted text: nothing is accepted until you say so",
        "delta_from_contract": "",
        "alternatives": alternatives,
        "report": shown,
        "rubric": contract.criteria.iter().map(|c| format!("{}: {}", c.id, clean(&c.text))).collect::<Vec<_>>(),
        "citations": checks.iter().map(|(c, r)| serde_json::json!({
            "path": clean(&c.path), "lines": format!("{}-{}", c.start_line, c.end_line),
            "quote": clean(&c.quote), "result": r.why, "file_sha256": r.sha256,
        })).collect::<Vec<_>>(),
        "evidence": {"work_item": item.key, "attempt_ordinal": a.ordinal,
                     "attempt_session": a.session_id, "report_sha256": report.sha256,
                     "checked_at": ts(now),
                     "citations_verified": format!("{verified} of {}", checks.len()),
                     "hidden_characters_replaced": hidden, "report_shortened": cut},
        "unknowns": [
            "the folder was not frozen: citations were read as it is now, at checked_at",
            if passed { "no independent check can judge a report: accepting it is your review" }
            else { "a citation did not check out: accepting is not offered; reject it, or ask for evidence" },
        ],
        "safe_default": "HOLD",
    });
    let hours = i64::from(w.decision_expiry_hours().min(24));
    let mut key = format!("report:{}:{}", item.key, a.ordinal);
    key.truncate(128);
    let nd = NewDecision {
        kind: "report_review".into(),
        dedupe_key: key,
        brief,
        binding,
        expires_at: ts(now + time::Duration::hours(hours)),
        action: Some(action),
    };
    let ev = NewEvidence {
        work_item_id: item.id.clone(),
        attempt_id: Some(a.id.clone()),
        trust_class: TrustClass::IndependentCheck,
        producer: "independent_runner".into(),
        subject_commit: report.subject(),
        check_id: "citation_check".into(),
        command_hash: None,
        criteria_ids: vec![],
        result: if passed { "passed" } else { "failed" }.into(),
        exit_code: None,
        env_fingerprint: "citation-check/1".into(),
        artifact: None,
        fence: a.fence,
        flags: vec![],
    };
    let d = w.atomically(|tx| {
        tx.record_citation_check(&ev)?;
        Ok(tx.new_decision(&nd)?.0)
    })?;
    open(w, &d)
}

/// `preparing` -> `pending` (a replay after a crash between the two steps).
fn open(w: &mut OutcomeWriter, d: &synthesis_session::outcome::DecisionRecord) -> Result<()> {
    if w.decision_state(&d.id)? == DecisionState::Preparing {
        w.transition_decision(
            &d.id,
            d.revision,
            DecisionState::Pending,
            &d.binding,
            &Effects::default(),
        )?;
    }
    Ok(())
}

/// What the verifier says about a read-only goal's item (replaces the independent check, which
/// cannot judge a report). `Passed` only for a decision that verifies as approved with
/// `accept_report` (MAC, tamper-checked action) AND every criterion's authentic `human_review`
/// row for this report. A rejection fails the item (it retries within its budget); a decision
/// that expired is renewed with the same brief and action, so the question is never lost.
pub(crate) fn review_verdict(
    w: &mut OutcomeWriter,
    item: &WorkItemRecord,
    attempt: &AttemptRecord,
) -> std::result::Result<Verdict, SchedError> {
    let Some(subject) = attempt.subject_commit.clone() else {
        return Ok(Verdict::Failed("the attempt left no report".into()));
    };
    let mut mine: Vec<_> = w
        .list_decisions(w.id())?
        .into_iter()
        .filter(|d| is_review_of(d, item, attempt))
        .collect();
    for d in mine.iter().rev() {
        let state = w.decision_state(&d.id)?;
        let choice = w.verified_resolution(&d.id)?.map(|r| r.choice);
        match (state, choice.as_deref()) {
            (DecisionState::Approved, Some("accept_report")) => {
                // the stored action must still hash to the binding (tampering is an error, not a pass)
                w.decision_action(&d.id)?;
                let o = w.get_outcome(w.id())?;
                let (contract, ..) = w.get_contract(w.id(), o.contract_version)?;
                let rows = w.authentic_evidence(w.id())?;
                let all = contract.criteria.iter().all(|c| {
                    rows.iter().any(|e| {
                        e.work_item_id == item.id
                            && e.trust_class == TrustClass::HumanReview
                            && e.check_id == format!("report_review:{}", c.id)
                            && e.subject_commit == subject
                            && e.result == "passed"
                            && e.flags.is_empty()
                            && e.fence_current
                    })
                });
                return Ok(if all {
                    Verdict::Passed
                } else {
                    Verdict::Pending
                });
            }
            (DecisionState::Rejected, _) => {
                return Ok(Verdict::Failed("the person rejected the report".into()));
            }
            _ => {}
        }
    }
    // nothing answered yet: an open decision waits; an expired newest one is asked again
    let Some(last) = mine.pop() else {
        return Ok(Verdict::Pending);
    };
    match w.decision_state(&last.id)? {
        DecisionState::Preparing => open(w, &last)?,
        DecisionState::Expired => {
            let action = w.decision_action(&last.id)?;
            let hours = i64::from(w.decision_expiry_hours().min(24));
            let (d, _) = w.new_decision(&NewDecision {
                kind: last.kind.clone(),
                dedupe_key: last.dedupe_key.clone(),
                brief: last.brief.clone(),
                binding: last.binding.clone(),
                expires_at: ts(w.now() + time::Duration::hours(hours)),
                action,
            })?;
            open(w, &d)?;
        }
        _ => {}
    }
    Ok(Verdict::Pending)
}
