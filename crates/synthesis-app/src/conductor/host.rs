//! The conductor host: the one policy point behind the conductor's tools (conductor.md 2.2). The model's calls
//! only read host-built digests or store a proposal; nothing here starts, sends, stops or writes. Turns start
//! only from the person's line ([`ConductorHost::person_turn`]); news from sessions becomes host notes.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use synthesis_harness::fence;
use synthesis_tools::{BoxFuture, ToolSpec};

use super::MESSAGE_WORKER_ENABLED;
use super::config::Confirm;
use super::digest::{Digests, TITLE_MAX, hidden, valid_id};
use super::limits::*;
use super::proposal::{Alt, Payload, Proposal, State};
use super::reads::{ConductorReads, SessionRow};
use super::tools::*;
use crate::host_tools::HostTools;

pub type Clock = Arc<dyn Fn() -> u64 + Send + Sync>;
/// Where audit-only events go (the conductor session's event log).
pub type Audit = Arc<dyn Fn(&Value) + Send + Sync>;

pub struct HostConfig {
    pub max_sessions: usize,
    pub confirm: Confirm,
    /// The user's home directory and `$SYNT_HOME`: a new session or goal may not use them or what holds them.
    pub home: PathBuf,
    pub synt_home: PathBuf,
    /// `provider/model` ids a proposed session may name.
    pub models: Vec<String>,
    /// Exact secret values (API keys) the digests mask on top of the built-in patterns.
    pub secrets: Vec<String>,
}

#[derive(Default)]
pub(super) struct St {
    turn: Turn,
    /// Sticky: once the conductor read session content, its context holds it until the session ends (4.2).
    tainted: bool,
    /// The person's latest line: what `basis: explicit` is checked against.
    line: String,
    sources: Vec<String>,
    proposals: Vec<Proposal>,
    next: u64,
    sends: SendLog,
    notes: Vec<String>,
    notes_tainted: bool,
    ended: bool,
}

pub struct ConductorHost {
    cfg: HostConfig,
    reads: Arc<dyn ConductorReads>,
    digests: Digests,
    clock: Clock,
    audit: Audit,
    st: Mutex<St>,
}

fn system_clock() -> Clock {
    Arc::new(|| {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs())
    })
}

enum Resolved {
    One(SessionRow),
    NotFound,
    Ambiguous(Vec<SessionRow>),
}

/// `s<N>`, `@s<N>` or a session id prefix of at least 8 hex digits, among live own sessions. Exactly one match,
/// or it fails: no recency or best-match tie-break (conductor.md 3.3). An outcome attempt's id is not in `rows`.
fn resolve(rows: &[SessionRow], token: &str) -> Resolved {
    let t = token.trim_start_matches('@');
    let is_handle = t
        .strip_prefix('s')
        .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()));
    let is_sid = t.len() >= 8 && t.bytes().all(|b| b.is_ascii_hexdigit() || b == b'-');
    let hits: Vec<&SessionRow> = rows
        .iter()
        .filter(|r| r.live)
        .filter(|r| (is_handle && r.handle == t) || (is_sid && r.sid.starts_with(t)))
        .collect();
    match hits.as_slice() {
        [] => Resolved::NotFound,
        [one] => Resolved::One((*one).clone()),
        many => Resolved::Ambiguous(many.iter().map(|r| (*r).clone()).collect()),
    }
}

/// Does the person's line name this session by handle (`s3`, `@s3`) or by a session id prefix of 8+ hex digits?
fn names(line: &str, row: &SessionRow) -> bool {
    line.to_lowercase()
        .split(|c: char| !(c.is_ascii_alphanumeric() || c == '-'))
        .any(|t| t == row.handle || (t.len() >= 8 && row.sid.starts_with(t)))
}

fn parse<T: DeserializeOwned>(input: Value) -> Result<T, String> {
    serde_json::from_value(input).map_err(|e| format!("invalid input: {e}"))
}

const WORKER_OFF: &str = "message_worker is not available yet: it needs the live worker inbox (P2c). Ask the person to message the worker in its session.";

/// A person-visible text field: not empty, within `max` bytes, no hidden or control characters (T11).
fn check_text(field: &str, s: &str, max: usize) -> Result<(), String> {
    if s.trim().is_empty() || s.len() > max {
        return Err(format!("{field} must be 1..={max} bytes"));
    }
    if s.chars().any(hidden) {
        return Err(format!(
            "{field} holds hidden or control characters: refused"
        ));
    }
    Ok(())
}

impl ConductorHost {
    pub fn new(cfg: HostConfig, reads: Arc<dyn ConductorReads>) -> Self {
        let digests = Digests::new(cfg.secrets.clone());
        Self {
            cfg,
            reads,
            digests,
            clock: system_clock(),
            audit: Arc::new(|_| {}),
            st: Mutex::new(St::default()),
        }
    }

    pub fn with_clock(mut self, c: Clock) -> Self {
        self.clock = c;
        self
    }

    pub fn with_audit(mut self, a: Audit) -> Self {
        self.audit = a;
        self
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, St> {
        self.st.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The person typed a line: a new conductor turn. Resets the turn's limits (not the taint: the model's context keeps what it read), and returns the text
    /// the model reads: the pending host notes, fenced, then the person's line. This is the only way a turn starts.
    pub fn person_turn(&self, line: &str) -> String {
        let mut st = self.lock();
        self.sweep(&mut st);
        st.turn = Turn::default();
        st.line = line.to_string();
        if std::mem::take(&mut st.notes_tainted) {
            st.tainted = true;
            if !st.sources.iter().any(|s| s == "host_note") && st.sources.len() < 32 {
                st.sources.push("host_note".into());
            }
        }
        let mut out = String::new();
        for n in st.notes.drain(..) {
            out += &format!("<host_note>\n{}\n</host_note>\n", fence("host_note", &n));
        }
        out + line
    }

    /// News for the conductor's next turn (a session finished, needs the person, ...). It never starts a turn.
    /// `session_text`: the news carries text from a session, which taints the next turn.
    pub fn push_news(&self, text: &str, session_text: bool) {
        let mut st = self.lock();
        st.notes.push(self.digests.clean(text, 512));
        st.notes_tainted |= session_text;
    }

    /// The conductor session ended: every shown proposal expires (4.3).
    pub fn end(&self) {
        let mut st = self.lock();
        st.ended = true;
        for i in 0..st.proposals.len() {
            if st.proposals[i].state == State::Shown {
                self.set_state(&mut st, i, State::Expired, None);
            }
        }
    }

    pub fn proposals(&self) -> Vec<Proposal> {
        let mut st = self.lock();
        self.sweep(&mut st);
        st.proposals.clone()
    }

    pub fn proposal(&self, id: &str) -> Option<Proposal> {
        self.proposals().into_iter().find(|p| p.id == id)
    }

    /// The person pressed Cancel (or dismissed the card). Not an effect, so it needs no click token.
    pub fn cancel(&self, id: &str) -> Result<(), String> {
        let mut st = self.lock();
        self.sweep(&mut st);
        match st.proposals.iter().position(|p| p.id == id) {
            Some(i) if st.proposals[i].state == State::Shown => {
                self.set_state(&mut st, i, State::Cancelled, None);
                Ok(())
            }
            Some(_) => Err("that proposal is no longer open".into()),
            None => Err("unknown proposal".into()),
        }
    }

    // ---- state helpers (the lock is held) ----

    fn sweep(&self, st: &mut St) {
        let now = (self.clock)();
        for i in 0..st.proposals.len() {
            if st.proposals[i].state == State::Shown
                && now.saturating_sub(st.proposals[i].created) >= PROPOSAL_TTL_SECS
            {
                self.set_state(st, i, State::Expired, None);
            }
        }
    }

    fn set_state(&self, st: &mut St, i: usize, state: State, detail: Option<String>) {
        let p = &mut st.proposals[i];
        p.state = state;
        p.detail = detail.map(|d| self.digests.clean(&d, 200));
        let note = format!(
            "proposal {} is {}",
            p.id,
            serde_json::to_value(state)
                .unwrap_or_default()
                .as_str()
                .unwrap_or("")
        );
        let ev = self.audit_event(&st.proposals[i]);
        // the person's own click or the clock decided: the note names no session text, so it taints nothing
        st.notes.push(note);
        (self.audit)(&ev);
    }

    fn audit_event(&self, p: &Proposal) -> Value {
        let mut payload = serde_json::to_value(&p.payload).unwrap_or_default();
        self.digests.redactor().redact_value(&mut payload);
        json!({"event": "conductor.proposal", "id": p.id, "state": p.state, "payload_sha256": p.payload_sha256,
               "tainted": p.tainted, "taint_sources": p.taint_sources, "payload": payload})
    }

    fn taint(&self, st: &mut St, source: String) {
        st.tainted = true;
        if !st.sources.contains(&source) && st.sources.len() < 32 {
            st.sources.push(source);
        }
    }

    // ---- validation shared by propose and the click ----

    /// Absolute, existing, not a filesystem root, not the home directory or a directory that holds it, not
    /// under `$SYNT_HOME`. Returns the canonical path.
    pub(super) fn check_folder(&self, f: &str) -> Result<String, String> {
        check_text("folder", f, 1024)?;
        let p = Path::new(f);
        if !p.is_absolute() {
            return Err("folder must be an absolute path".into());
        }
        let c = dunce::canonicalize(p).map_err(|_| "folder does not exist".to_string())?;
        let home = dunce::canonicalize(&self.cfg.home)
            .map_err(|_| "the home directory cannot be checked: folder refused".to_string())?;
        if !c.is_dir() || c.parent().is_none() || c == home || home.starts_with(&c) {
            return Err("folder is not a usable workspace (a filesystem root, the home directory or a directory that holds it)".into());
        }
        if dunce::canonicalize(&self.cfg.synt_home).is_ok_and(|s| c.starts_with(s)) {
            return Err("folder is inside the Synthesis home: refused".into());
        }
        c.to_str()
            .map(str::to_string)
            .ok_or_else(|| "folder is not valid text".into())
    }

    pub(super) fn check_session_room(&self) -> Result<(), String> {
        let live = self.reads.sessions().iter().filter(|r| r.live).count();
        if live >= self.cfg.max_sessions {
            return Err(format!(
                "limit: {} live sessions already (the most is {})",
                live, self.cfg.max_sessions
            ));
        }
        Ok(())
    }

    /// Does this payload still hold? Called when proposed and again at the click.
    pub(super) fn revalidate(&self, p: &Payload) -> Result<(), String> {
        match p {
            Payload::CreateSession {
                first_message,
                folder,
                model,
                title,
            } => {
                check_text("first_message", first_message, FIRST_MESSAGE_MAX)?;
                if let Some(f) = folder {
                    self.check_folder(f)?;
                }
                if let Some(m) = model
                    && !self.cfg.models.contains(m)
                {
                    return Err("model is not one of the listed models".into());
                }
                if let Some(t) = title {
                    check_text("title", t, TITLE_MAX)?;
                }
                self.check_session_room()
            }
            Payload::CreateGoal {
                goal_kind,
                contract,
                folder,
                base_commit,
            } => {
                check_text("contract", contract, CONTRACT_MAX)?;
                self.check_folder(folder)?;
                use synthesis_view::outcomes::is_hex;
                match (goal_kind.as_str(), base_commit.as_deref()) {
                    ("read_only", Some(_)) => Err("a read-only goal has no base commit".into()),
                    ("write" | "read_only", None) => Ok(()),
                    ("write", Some(c)) if is_hex(c, 40) || is_hex(c, 64) => Ok(()),
                    ("write", Some(_)) => {
                        Err("base_commit must be a lowercase hex commit id".into())
                    }
                    _ => Err("kind must be write or read_only".into()),
                }
            }
            Payload::SendMessage {
                session, sid, text, ..
            } => {
                check_text("text", text, TEXT_MAX)?;
                self.live_session(session, sid)
            }
            Payload::Interrupt { session, sid } | Payload::Close { session, sid } => {
                self.live_session(session, sid)
            }
            Payload::MessageWorker {
                outcome,
                work_item,
                text,
            } => {
                if !MESSAGE_WORKER_ENABLED {
                    return Err(WORKER_OFF.into());
                }
                self.check_worker(outcome, work_item, text)
            }
            Payload::PauseOutcome { outcome } => self
                .reads
                .outcome(outcome)
                .map(|_| ())
                .ok_or_else(|| "not_found: no such outcome".into()),
        }
    }

    /// Text and ownership of a worker message; the dark flag is checked by the callers, not here.
    fn check_worker(&self, outcome: &str, work_item: &str, text: &str) -> Result<(), String> {
        check_text("text", text, TEXT_MAX)?;
        // refused, never masked: the person's card must show the bytes the worker would read
        if self.digests.redactor().redact_str(text) != text {
            return Err("text looks like a secret: refused".into());
        }
        // the item must be one the host can read, and sit in the outcome the card names
        match self.reads.item(work_item) {
            Some(i) if i.outcome_id == outcome && self.reads.outcome(outcome).is_some() => Ok(()),
            _ => Err("not_found: no such work item in an outcome of yours".into()),
        }
    }

    fn live_session(&self, handle: &str, sid: &str) -> Result<(), String> {
        match resolve(&self.reads.sessions(), handle) {
            Resolved::One(r) if r.sid == sid => Ok(()),
            _ => Err("the session ended or changed".into()),
        }
    }

    // ---- the click side (used by the executor only) ----

    /// Claims a shown proposal for execution (so a second click finds it taken), re-validates it, and
    /// charges the rate limit. `edit`: the person changed the card's text; the new bytes are validated and
    /// become the proposal's payload. Returns the payload to run.
    pub(super) fn claim(
        &self,
        id: &str,
        shown_sha256: &str,
        edit: Option<String>,
    ) -> Result<Proposal, String> {
        let mut st = self.lock();
        self.sweep(&mut st);
        let i = st
            .proposals
            .iter()
            .position(|p| p.id == id)
            .ok_or("unknown proposal")?;
        if st.proposals[i].state != State::Shown {
            return Err("that proposal is no longer open".into());
        }
        let mut payload = st.proposals[i].payload.clone();
        // the click is bound to the bytes the card displayed, not to whatever is stored now
        if st.proposals[i].payload_sha256 != shown_sha256
            || st.proposals[i].payload_sha256 != payload.sha256()
        {
            self.set_state(&mut st, i, State::Stale, Some("the card changed".into()));
            return Err("stale: the proposal changed".into());
        }
        let edited = edit.is_some();
        if let Some(t) = edit {
            match &mut payload {
                Payload::CreateSession {
                    first_message: x, ..
                }
                | Payload::SendMessage { text: x, .. }
                | Payload::MessageWorker { text: x, .. }
                | Payload::CreateGoal { contract: x, .. } => *x = t,
                _ => return Err("this card has no text to edit".into()),
            }
        }
        if let Err(e) = self.revalidate(&payload) {
            // an edit that does not validate leaves the card open; anything else means it went stale
            if edited {
                return Err(e);
            }
            self.set_state(&mut st, i, State::Stale, Some(e.clone()));
            return Err(format!("stale: {e}"));
        }
        let target = match &payload {
            Payload::SendMessage { sid, .. } => Some(sid.clone()),
            // prefixed so a work item id never shares a budget with a session id
            Payload::MessageWorker { work_item, .. } => Some(format!("w:{work_item}")),
            _ => None,
        };
        if let Some(sid) = target {
            let now = (self.clock)();
            if !st.sends.allows(&sid, now) {
                return Err(format!(
                    "limit: {MAX_SENDS_PER_TARGET} sends to one session in 10 minutes"
                ));
            }
            st.sends.record(&sid, now);
        }
        let p = &mut st.proposals[i];
        p.payload = payload;
        p.payload_sha256 = p.payload.sha256();
        self.set_state(
            &mut st,
            i,
            if edited {
                State::Edited
            } else {
                State::Accepted
            },
            None,
        );
        Ok(st.proposals[i].clone())
    }

    /// The effect failed after the claim.
    pub(super) fn failed(&self, id: &str, why: &str) {
        let mut st = self.lock();
        if let Some(i) = st.proposals.iter().position(|p| p.id == id) {
            self.set_state(&mut st, i, State::Failed, Some(why.into()));
        }
    }

    // ---- the tools ----

    fn dispatch(&self, name: &str, input: Value) -> Result<Value, String> {
        if !TOOLS.contains(&name) {
            return Err(format!("unknown tool \"{}\"", self.digests.clean(name, 64)));
        }
        {
            let mut st = self.lock();
            if st.ended {
                return Err("the conductor session has ended".into());
            }
            if st.turn.calls >= MAX_CALLS_PER_TURN {
                return Err(format!(
                    "limit: at most {MAX_CALLS_PER_TURN} tool calls per turn; ask the person to continue"
                ));
            }
            st.turn.calls += 1;
        }
        match name {
            "conductor_list_sessions" => {
                parse::<serde_json::Map<String, Value>>(input)
                    .ok()
                    .filter(|m| m.is_empty())
                    .ok_or("invalid input: this tool takes no fields")?;
                let rows = self.reads.sessions();
                self.taint(&mut self.lock(), "sessions".into());
                Ok(self.digests.session_rows(&rows))
            }
            "conductor_list_outcomes" => {
                let a: ListOutcomes = parse(input)?;
                if a.state.as_deref().is_some_and(|s| !valid_id(s)) {
                    return Err("invalid input: state".into());
                }
                let rows = self.reads.outcomes(a.state.as_deref());
                self.taint(&mut self.lock(), "outcomes".into());
                Ok(self.digests.outcome_rows(&rows))
            }
            "conductor_read_digest" => self.read_digest(parse::<ReadDigest>(input)?.target),
            "conductor_propose_session" => self.propose_session(parse(input)?),
            "conductor_propose_goal" => self.propose_goal(parse(input)?),
            "conductor_propose_assignment" => self.propose_assignment(parse(input)?),
            _ => self.propose_stop(parse(input)?),
        }
    }

    fn read_digest(&self, t: DigestTarget) -> Result<Value, String> {
        let nf = |what: &str| format!("not_found: no such {what}");
        let id_ok = |s: &str| {
            if valid_id(s) {
                Ok(())
            } else {
                Err("invalid input: id".to_string())
            }
        };
        let (v, source) = match t {
            DigestTarget::Session(SessionT { session }) => {
                id_ok(&session)?;
                let Resolved::One(row) = resolve(&self.reads.sessions(), &session) else {
                    return Err(nf("live session"));
                };
                let rep = self
                    .reads
                    .session_report(&row.handle)
                    .ok_or_else(|| nf("session"))?;
                (
                    self.digests.session_digest(&row, &rep),
                    format!("session:{}", row.handle),
                )
            }
            DigestTarget::Outcome(OutcomeT { outcome }) => {
                id_ok(&outcome)?;
                let r = self.reads.outcome(&outcome).ok_or_else(|| nf("outcome"))?;
                (
                    self.digests.outcome_digest(&r),
                    format!("outcome:{outcome}"),
                )
            }
            DigestTarget::Item(ItemT { work_item }) => {
                id_ok(&work_item)?;
                let r = self.reads.item(&work_item).ok_or_else(|| nf("work item"))?;
                (self.digests.item_digest(&r), format!("item:{work_item}"))
            }
            DigestTarget::Report(ReportT { report }) => {
                id_ok(&report)?;
                let text = self.reads.report(&report).ok_or_else(|| nf("report"))?;
                (
                    self.digests.report_digest(&report, &text),
                    format!("report:{report}"),
                )
            }
        };
        self.taint(&mut self.lock(), source);
        Ok(v)
    }

    /// Stores the proposal and answers at once. Taint and the forced-card reasons are recorded here.
    fn store(&self, payload: Payload, extra_forced: Option<&str>) -> Result<Value, String> {
        let mut st = self.lock();
        self.sweep(&mut st);
        let is_goal = matches!(payload, Payload::CreateGoal { .. });
        if st.turn.proposals >= MAX_PROPOSALS_PER_TURN
            || (is_goal && st.turn.goals >= MAX_GOALS_PER_TURN)
        {
            return Err(format!(
                "limit: at most {MAX_PROPOSALS_PER_TURN} proposals ({MAX_GOALS_PER_TURN} goal) per turn"
            ));
        }
        if st
            .proposals
            .iter()
            .filter(|p| p.state == State::Shown)
            .count()
            >= MAX_OPEN_PROPOSALS
        {
            return Err(format!(
                "limit: {MAX_OPEN_PROPOSALS} proposals are waiting for the person"
            ));
        }
        let mut forced = vec![];
        if st.tainted {
            forced.push("the conductor read session content in this session".to_string());
        }
        forced.extend(extra_forced.map(str::to_string));
        let auto_eligible = forced.is_empty()
            && payload.action().is_some_and(|a| match &self.cfg.confirm {
                Confirm::All => false,
                Confirm::Auto(set) => set.contains(&a),
            });
        st.next += 1;
        let id = format!("p{}", st.next);
        let p = Proposal {
            id: id.clone(),
            payload_sha256: payload.sha256(),
            payload,
            tainted: st.tainted,
            taint_sources: st.sources.clone(),
            card_forced: forced,
            auto_eligible,
            state: State::Shown,
            created: (self.clock)(),
            detail: None,
        };
        st.turn.proposals += 1;
        st.turn.goals += u32::from(is_goal);
        (self.audit)(&self.audit_event(&p));
        st.proposals.push(p);
        Ok(json!({"proposal_id": id, "state": "shown"}))
    }

    fn propose_session(&self, a: ProposeSession) -> Result<Value, String> {
        let payload = Payload::CreateSession {
            folder: a
                .folder
                .as_deref()
                .map(|f| self.check_folder(f))
                .transpose()?,
            first_message: a.first_message,
            model: a.model,
            title: a.title,
        };
        self.revalidate(&payload)?;
        let forced = matches!(
            &payload,
            Payload::CreateSession {
                folder: Some(_),
                ..
            }
        )
        .then_some("the workspace was chosen by the conductor");
        self.store(payload, forced)
    }

    fn propose_goal(&self, a: ProposeGoal) -> Result<Value, String> {
        let payload = Payload::CreateGoal {
            goal_kind: a.kind,
            folder: self.check_folder(&a.folder)?,
            contract: a.contract,
            base_commit: a.base_commit,
        };
        self.revalidate(&payload)?;
        self.store(payload, None)
    }

    fn propose_assignment(&self, a: ProposeAssignment) -> Result<Value, String> {
        check_text("text", &a.text, TEXT_MAX)?;
        if a.alternatives.len() > MAX_ALTERNATIVES || !a.alternatives.iter().all(|x| valid_id(x)) {
            return Err("invalid input: alternatives".into());
        }
        let described = match a.basis.as_str() {
            "explicit" => false,
            "described" => true,
            _ => return Err("invalid input: basis must be explicit or described".into()),
        };
        let target = match a.target {
            AssignTarget::Item(ItemT { work_item }) => {
                return self.propose_worker(work_item, a.text, a.brief_change, &a.alternatives);
            }
            AssignTarget::Session(SessionT { session }) => session,
        };
        if a.brief_change {
            return Err("invalid input: brief_change is for a work item".into());
        }
        if !valid_id(target.trim_start_matches('@')) {
            return Err("invalid input: session".into());
        }
        let rows = self.reads.sessions();
        let row = match resolve(&rows, &target) {
            Resolved::One(r) => r,
            Resolved::NotFound => {
                return Err("not_found: no live session with that handle or id".into());
            }
            Resolved::Ambiguous(c) => return Err(self.ambiguous(&c)),
        };
        // `alternatives` names the other sessions the model could not rule out: ask the person, no card (3.3)
        let mut named = vec![row.clone()];
        for x in &a.alternatives {
            match resolve(&rows, x) {
                Resolved::One(r) => named.push(r),
                _ => return Err("not_found: an alternative is not a live session".into()),
            }
        }
        let mut handles: Vec<&str> = named.iter().map(|r| r.handle.as_str()).collect();
        handles.sort_unstable();
        handles.dedup();
        if handles.len() > 1 {
            return Err(self.ambiguous(&named));
        }
        if !self.lock().sends.allows(&row.sid, (self.clock)()) {
            return Err(format!(
                "limit: {MAX_SENDS_PER_TARGET} sends to one session in 10 minutes"
            ));
        }
        let alternatives = rows
            .iter()
            .filter(|r| r.live && r.handle != row.handle)
            .map(|r| Alt {
                handle: r.handle.clone(),
                title: self.digests.clean(&r.title, TITLE_MAX),
            })
            .collect();
        // `explicit` is the model's claim: it holds only if the person's latest line names the target
        let described = described || !names(&self.lock().line, &row);
        let payload = Payload::SendMessage {
            session: row.handle,
            sid: row.sid,
            text: a.text,
            basis: a.basis,
            alternatives,
        };
        self.store(
            payload,
            described.then_some("the target was not named by you"),
        )
    }

    fn propose_worker(
        &self,
        work_item: String,
        text: String,
        brief_change: bool,
        alternatives: &[String],
    ) -> Result<Value, String> {
        if !MESSAGE_WORKER_ENABLED {
            return Err(WORKER_OFF.into());
        }
        if brief_change || !alternatives.is_empty() {
            return Err("invalid input: a worker card has no brief_change or alternatives".into());
        }
        if !valid_id(&work_item) {
            return Err("invalid input: work_item".into());
        }
        let item = self
            .reads
            .item(&work_item)
            .ok_or("not_found: no such work item in an outcome of yours")?;
        let payload = Payload::MessageWorker {
            outcome: item.outcome_id,
            work_item,
            text,
        };
        self.revalidate(&payload)?;
        self.store(payload, Some("a worker message always needs your card"))
    }

    fn ambiguous(&self, rows: &[SessionRow]) -> String {
        let c: Vec<String> = rows
            .iter()
            .map(|r| format!("{} \"{}\"", r.handle, self.digests.clean(&r.title, 60)))
            .collect();
        format!(
            "ambiguous: more than one session could fit ({}); ask the person which one",
            c.join(", ")
        )
    }

    fn propose_stop(&self, a: ProposeStop) -> Result<Value, String> {
        let payload = match (a.session, a.outcome, a.how.as_str()) {
            (Some(s), None, how @ ("interrupt" | "close")) => {
                if !valid_id(s.trim_start_matches('@')) {
                    return Err("invalid input: session".into());
                }
                let Resolved::One(r) = resolve(&self.reads.sessions(), &s) else {
                    return Err("not_found: no live session with that handle or id".into());
                };
                if how == "close" {
                    Payload::Close {
                        session: r.handle,
                        sid: r.sid,
                    }
                } else {
                    Payload::Interrupt {
                        session: r.handle,
                        sid: r.sid,
                    }
                }
            }
            (None, Some(o), "pause") => {
                if !valid_id(&o) {
                    return Err("invalid input: outcome".into());
                }
                Payload::PauseOutcome { outcome: o }
            }
            _ => return Err(
                "invalid input: a session with how interrupt|close, or an outcome with how pause"
                    .into(),
            ),
        };
        self.revalidate(&payload)?;
        self.store(payload, None)
    }
}

impl HostTools for ConductorHost {
    fn specs(&self) -> Vec<ToolSpec> {
        specs()
    }

    fn call<'a>(&'a self, name: &'a str, input: Value) -> BoxFuture<'a, Result<Value, String>> {
        let r = self.dispatch(name, input);
        Box::pin(async move { r })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::conductor::reads::{ItemRow, OutcomeRow, SessionReport};

    struct R;
    impl ConductorReads for R {
        fn sessions(&self) -> Vec<SessionRow> {
            vec![]
        }
        fn session_report(&self, _: &str) -> Option<SessionReport> {
            None
        }
        fn outcomes(&self, _: Option<&str>) -> Vec<OutcomeRow> {
            vec![]
        }
        fn outcome(&self, id: &str) -> Option<OutcomeRow> {
            (id == "o1").then(|| OutcomeRow {
                id: "o1".into(),
                title: "t".into(),
                kind: "write".into(),
                state: "running".into(),
                needs_you: 0,
                spent_usd: 0.0,
                cap_usd: 1.0,
            })
        }
        fn item(&self, id: &str) -> Option<ItemRow> {
            let outcome_id = match id {
                "w1" => "o1",
                "w2" => "o2",
                _ => return None,
            };
            Some(ItemRow {
                id: id.into(),
                outcome_id: outcome_id.into(),
                state: "running".into(),
                attempts: 1,
                needs_you: false,
                spent_usd: 0.0,
            })
        }
        fn report(&self, _: &str) -> Option<String> {
            None
        }
    }

    fn host(now: Arc<std::sync::atomic::AtomicU64>) -> ConductorHost {
        let cfg = HostConfig {
            max_sessions: 3,
            confirm: Confirm::All,
            home: std::env::temp_dir(),
            synt_home: std::env::temp_dir(),
            models: vec![],
            secrets: vec!["hunter2-secret".into()],
        };
        ConductorHost::new(cfg, Arc::new(R)).with_clock(Arc::new(move || {
            now.load(std::sync::atomic::Ordering::SeqCst)
        }))
    }

    #[test]
    fn worker_message_validation_and_dark_flag() {
        let h = host(Arc::default());
        assert!(h.check_worker("o1", "w1", "please rebase").is_ok());
        // an item of an outcome that is not readable, or named under another outcome
        assert!(
            h.check_worker("o2", "w2", "hi")
                .unwrap_err()
                .contains("not_found")
        );
        assert!(h.check_worker("o1", "w2", "hi").is_err());
        assert!(h.check_worker("o1", "nope", "hi").is_err());
        assert!(
            h.check_worker("o1", "w1", &"x".repeat(TEXT_MAX + 1))
                .is_err()
        );
        assert!(h.check_worker("o1", "w1", "bell\u{7}").is_err());
        assert!(
            h.check_worker("o1", "w1", "key hunter2-secret")
                .unwrap_err()
                .contains("secret")
        );
        assert!(
            h.check_worker(
                "o1",
                "w1",
                "sk-ant-api03-abcdefghijklmnopqrstuvwxyz0123456789"
            )
            .is_err()
        );
        // dark: every path refuses with the one message
        let p = Payload::MessageWorker {
            outcome: "o1".into(),
            work_item: "w1".into(),
            text: "hi".into(),
        };
        assert_eq!(h.revalidate(&p).unwrap_err(), WORKER_OFF);
        assert_eq!(
            h.propose_worker("w1".into(), "hi".into(), false, &[])
                .unwrap_err(),
            WORKER_OFF
        );
        assert!(h.proposals().is_empty());
    }

    #[test]
    fn worker_and_session_send_budgets_are_separate() {
        let mut log = SendLog::default();
        for _ in 0..MAX_SENDS_PER_TARGET {
            log.record("w:abc", 1);
        }
        assert!(!log.allows("w:abc", 1) && log.allows("abc", 1));
    }

    #[test]
    fn worker_proposal_expires_after_the_ttl() {
        let now = Arc::new(std::sync::atomic::AtomicU64::new(100));
        let h = host(now.clone());
        // stored directly: the flag is off, so no tool call can make one
        h.store(
            Payload::MessageWorker {
                outcome: "o1".into(),
                work_item: "w1".into(),
                text: "hi".into(),
            },
            None,
        )
        .unwrap();
        assert_eq!(h.proposals()[0].state, State::Shown);
        now.store(100 + PROPOSAL_TTL_SECS, std::sync::atomic::Ordering::SeqCst);
        let id = h.proposals()[0].id.clone();
        assert_eq!(h.proposal(&id).unwrap().state, State::Expired);
        assert!(
            h.claim(&id, &h.proposal(&id).unwrap().payload_sha256, None)
                .is_err()
        );
    }
}
