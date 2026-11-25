//! The report of a read-only goal's attempt (S1 section 2.3): the worker's final message, which is
//! untrusted data. `report:sha256:<hex>` is the attempt's subject. When the attempt settles,
//! [`open_review`] checks its citations against the goal folder (no model) and stores a
//! `report_review` decision; only the person's `accept_report` answer writes the `human_review`
//! evidence (in the store, in the transaction of the answer), and [`review_verdict`] reads that
//! back. Nothing here marks anything verified.

use sha2::{Digest, Sha256};
use synthesis_core::outcome::{ContractKind, DecisionState, TrustClass, hidden_char};
use synthesis_core::secrets::is_secret_shaped_in;
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

/// The deliverable: `{report, citations}` as JSON (a code fence around it is tolerated). Anything
/// else, including JSON with any other key (it would be hashed but never shown), is the whole
/// message as the report with no citations, so what the person reads is all that was hashed. The
/// flag is set when more than `CITES_MAX` citations were given (the rest are not checked).
pub fn parse_report(text: &str) -> (String, Vec<Citation>, bool) {
    let whole = || (text.to_owned(), vec![], false);
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
        return whole();
    };
    let Some(report) = o.get("report").and_then(|r| r.as_str()) else {
        return whole();
    };
    if o.keys().any(|k| k != "report" && k != "citations") {
        return whole();
    }
    let list = o.get("citations").and_then(|c| c.as_array());
    let cites = list
        .map(|a| {
            a.iter()
                .take(CITES_MAX)
                // a malformed entry (or one with other keys) stays as an empty one, which fails its check
                .map(|c| {
                    let known = c.as_object().is_some_and(|m| {
                        m.keys().all(|k| {
                            matches!(k.as_str(), "path" | "start_line" | "end_line" | "quote")
                        })
                    });
                    let s = |k: &str| {
                        c.get(k)
                            .and_then(|v| v.as_str())
                            .filter(|_| known)
                            .unwrap_or("")
                            .to_owned()
                    };
                    let n = |k: &str| c.get(k).and_then(|v| v.as_u64()).filter(|_| known);
                    Citation {
                        path: s("path"),
                        start_line: n("start_line").unwrap_or(0),
                        end_line: n("end_line").unwrap_or(0),
                        quote: s("quote"),
                    }
                })
                .collect()
        })
        .unwrap_or_default();
    (
        report.to_owned(),
        cites,
        list.is_some_and(|a| a.len() > CITES_MAX),
    )
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
pub fn check_citation(
    root: &std::path::Path,
    c: &Citation,
    gate: &synthesis_events::Redactor,
) -> Checked {
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
                ".git" | ".synt" | ".synthesis" // the legacy state folder may still be on disk
            )
        })
    };
    if rel.components().any(state) || is_secret_shaped_in(rel, std::path::Path::new("")) {
        return no("a secret or state path is never cited");
    }
    // Windows reads `name.` and `name ` as `name`, and `PROGRA~1` as another name
    if cfg!(windows)
        && rel.components().any(|x| {
            let n = x.as_os_str().to_string_lossy();
            n.ends_with(['.', ' ']) || n.contains('~')
        })
    {
        return no("the path has a Windows alias form");
    }
    let quote = norm(&c.quote);
    if quote.is_empty() || c.quote.chars().count() > QUOTE_MAX {
        return no("the quote is empty or longer than 400 characters");
    }
    if c.start_line == 0 || c.end_line < c.start_line || c.end_line - c.start_line >= SPAN_MAX {
        return no("the line range is not valid");
    }
    // shortcut: each component is checked with lstat and the last is opened O_NOFOLLOW, not
    // opened with openat per component, so a link swapped in between the check and the open of a
    // directory on the way is not caught; the checked file is only read and quoted, never written
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
    let cited = window.join("\n");
    if gate.redact_str(&cited) != cited || gate.redact_str(&c.quote) != c.quote {
        return no("the cited lines or the quote hold a secret");
    }
    if norm(&cited).contains(&quote) {
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
fn show(text: &str, redactor: &synthesis_events::Redactor) -> (String, usize, bool) {
    let mut hidden = 0;
    let mut out = String::new();
    // a secret in the report (pasted or read from a file) is masked before anyone, or any later
    // model request, sees it
    let text = redactor.redact_str(text);
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

/// The report as a person is shown it, and the citations to check: what [`open_review`] makes
/// from the message and what a re-check takes from the stored action and brief.
struct Prepared {
    sha: String,
    shown: String,
    hidden: u64,
    cut: bool,
    cites: Vec<Citation>,
    too_many: bool,
}

fn cite_json(c: &Citation) -> serde_json::Value {
    serde_json::json!({"path": c.path, "start_line": c.start_line, "end_line": c.end_line, "quote": c.quote})
}

/// Checks the citations NOW and stores the citation-check evidence row and a new `report_review`
/// decision (current binding) in ONE transaction, then opens it. `rebrief` counts how often the
/// person asked for evidence (carried so the store's limit holds).
fn issue(
    w: &mut OutcomeWriter,
    item: &WorkItemRecord,
    a: &AttemptRecord,
    p: &Prepared,
    rebrief: u64,
    redactor: &synthesis_events::Redactor,
) -> Result<()> {
    let now = w.now();
    let o = w.get_outcome(w.id())?;
    let (contract, ..) = w.get_contract(w.id(), o.contract_version)?;
    let checks: Vec<(&Citation, Checked)> = p
        .cites
        .iter()
        .map(|c| {
            (
                c,
                check_citation(std::path::Path::new(&o.repo_root), c, redactor),
            )
        })
        .collect();
    let all_ok = checks.iter().all(|(_, r)| r.ok);
    // accepting needs the whole report on the card, every citation checked and verified
    let acceptable = all_ok && !p.cut && !p.too_many;
    let ids: Vec<&str> = {
        let mut v: Vec<&str> = contract.criteria.iter().map(|c| c.id.as_str()).collect();
        v.sort_unstable();
        v
    };
    let action = serde_json::json!({
        "attempt": a.id, "report_sha256": p.sha, "criteria": ids,
        "citations": p.cites.iter().map(cite_json).collect::<Vec<_>>(),
        "too_many": p.too_many, "cut": p.cut,
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
    let why_not = if p.too_many {
        "the report gave more than 200 citations: accepting is not offered"
    } else if p.cut {
        "the report is longer than the card can show: accepting is not offered"
    } else {
        "a citation did not check out: accepting is not offered; reject it, or ask for evidence"
    };
    let mut brief = serde_json::json!({
        "why": "a read-only worker finished. Its report is the worker's own, untrusted text: nothing is accepted until you say so",
        "delta_from_contract": "",
        "alternatives": if acceptable { vec![serde_json::json!({"id": "accept_report",
            "label": "accept this report: it answers every rubric line above"})] } else { vec![] },
        "report": p.shown,
        "rubric": contract.criteria.iter().map(|c| format!("{}: {}", c.id, clean(&c.text))).collect::<Vec<_>>(),
        "citations": checks.iter().map(|(c, r)| serde_json::json!({
            "path": clean(&c.path), "lines": format!("{}-{}", c.start_line, c.end_line),
            "quote": clean(&c.quote), "result": r.why, "file_sha256": r.sha256,
        })).collect::<Vec<_>>(),
        "evidence": {"work_item": item.key, "attempt_ordinal": a.ordinal,
                     "attempt_session": a.session_id, "report_sha256": p.sha,
                     "checked_at": ts(now),
                     "citations_verified": if checks.is_empty() { "no citations".to_owned() }
                         else { format!("{verified} of {}{}", checks.len(), if p.too_many { " (more were given and not checked)" } else { "" }) },
                     "hidden_characters_replaced": p.hidden, "report_shortened": p.cut},
        "unknowns": [
            "the folder was not frozen: citations were read as it is now, at checked_at",
            if acceptable { "no independent check can judge a report: accepting it is your review" } else { why_not },
        ],
        "safe_default": "HOLD",
    });
    if rebrief > 0 {
        brief["rebrief"] = serde_json::json!(rebrief);
    }
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
    // the first check of an attempt is `citation_check`, each re-check `citation_check:N`
    let reruns = w
        .authentic_evidence(w.id())?
        .iter()
        .filter(|e| e.work_item_id == item.id && e.check_id.starts_with("citation_check"))
        .count();
    let ev = NewEvidence {
        work_item_id: item.id.clone(),
        attempt_id: Some(a.id.clone()),
        trust_class: TrustClass::IndependentCheck,
        producer: "independent_runner".into(),
        subject_commit: format!("report:sha256:{}", p.sha),
        check_id: if reruns == 0 {
            "citation_check".into()
        } else {
            format!("citation_check:{}", reruns + 1)
        },
        command_hash: None,
        criteria_ids: vec![],
        result: if all_ok && !p.too_many {
            "passed"
        } else {
            "failed"
        }
        .into(),
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

/// The attempt settled with `report` as its subject: checks the citations, then stores the
/// citation-check evidence row and the `report_review` decision in ONE transaction and opens the
/// decision. Replays find the decision and do nothing (a crash leaves either nothing or all of
/// it; a decision still `preparing` is opened). Never asks a model, never marks anything verified.
pub fn open_review(
    w: &mut OutcomeWriter,
    item: &WorkItemRecord,
    attempt: &AttemptRecord,
    report: &AttemptReport,
    _now: OffsetDateTime,
    redactor: &synthesis_events::Redactor,
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
    let (text, cites, too_many) = parse_report(&report.text);
    let (shown, hidden, cut) = show(&text, redactor);
    let p = Prepared {
        sha: report.sha256.clone(),
        shown,
        hidden: hidden as u64,
        cut,
        cites,
        too_many,
    };
    issue(w, item, &a, &p, 0, redactor)
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

/// Asks the question again from what the old decision stored (its verified action and brief):
/// the citations are checked afresh, against the current contract and binding.
fn reissue(
    w: &mut OutcomeWriter,
    item: &WorkItemRecord,
    a: &AttemptRecord,
    old: &synthesis_session::outcome::DecisionRecord,
    rebrief: u64,
    redactor: &synthesis_events::Redactor,
) -> Result<()> {
    let action = w
        .decision_action(&old.id)?
        .ok_or_else(|| synthesis_session::Error::Invalid("the review has no action".into()))?;
    let cites = action
        .get("citations")
        .and_then(|c| c.as_array())
        .map(|c| {
            c.iter()
                .map(|c| Citation {
                    path: c["path"].as_str().unwrap_or("").to_owned(),
                    start_line: c["start_line"].as_u64().unwrap_or(0),
                    end_line: c["end_line"].as_u64().unwrap_or(0),
                    quote: c["quote"].as_str().unwrap_or("").to_owned(),
                })
                .collect()
        })
        .unwrap_or_default();
    let p = Prepared {
        sha: action["report_sha256"].as_str().unwrap_or("").to_owned(),
        shown: old.brief["report"].as_str().unwrap_or("").to_owned(),
        hidden: old.brief["evidence"]["hidden_characters_replaced"]
            .as_u64()
            .unwrap_or(0),
        cut: action["cut"].as_bool().unwrap_or(true),
        cites,
        too_many: action["too_many"].as_bool().unwrap_or(true),
    };
    issue(w, item, a, &p, rebrief, redactor)
}

/// What the verifier says about a read-only goal's item (replaces the independent check, which
/// cannot judge a report). `Passed` only for a decision that verifies as approved with
/// `accept_report` (MAC, tamper-checked action) AND every criterion's authentic `human_review`
/// row for this report. A rejection fails the item (it retries within its budget). When no
/// decision is open (it expired, its binding moved, or the person asked for evidence) the
/// question is asked again with the citations checked afresh, so it is never lost.
pub(crate) fn review_verdict(
    w: &mut OutcomeWriter,
    item: &WorkItemRecord,
    attempt: &AttemptRecord,
    redactor: &synthesis_events::Redactor,
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
    // nothing answered: an open decision waits, otherwise the newest one is asked again
    let Some(last) = mine.pop() else {
        // no review exists for a finished attempt (the settle opens it before finishing, so this is
        // a store from elsewhere): fail it so a new attempt makes one, never wait for ever
        return Ok(Verdict::Failed("the report has no review".into()));
    };
    let rebriefs = last.brief["rebrief"].as_u64().unwrap_or(0);
    match w.decision_state(&last.id)? {
        DecisionState::Preparing => open(w, &last)?,
        DecisionState::Expired | DecisionState::Invalidated => {
            reissue(w, item, attempt, &last, rebriefs, redactor)?
        }
        DecisionState::NeedsMoreInfo
            if w.verified_resolution(&last.id)?
                .map(|r| r.choice)
                .as_deref()
                == Some("request_evidence") =>
        {
            // the driver leaves it to us: close the card, check again, ask again
            w.transition_decision(
                &last.id,
                last.revision,
                DecisionState::Invalidated,
                &serde_json::json!({"rebriefed": last.id}),
                &Effects::default(),
            )?;
            reissue(w, item, attempt, &last, rebriefs + 1, redactor)?;
        }
        _ => {}
    }
    Ok(Verdict::Pending)
}
