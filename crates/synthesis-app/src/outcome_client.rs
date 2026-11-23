//! The typed client of the outcome methods (ADR-021 section 1): one library for every surface, over
//! a [`Transport`]. Surfaces never import the orchestrator; they call this and render
//! `synthesis_view::outcomes`.
//!
//! The server is not trusted. A result nobody asked for, a second result, a message that is
//! not a result, or a reply that does not echo what was sent poisons the connection: every later
//! call fails and nothing it said is acted on. Decisions are answered only by a call a person
//! made on purpose: the engine cannot send one, and it cannot fake the answer to one.

use std::io::{BufRead, Write};
use std::time::Duration;

use serde_json::Value;
use synthesis_session::outcome::Surface;
use synthesis_view::outcomes::{CommandReply, DecisionCard, OutcomeRow, Picker};

use crate::proto::{
    Call, CommandArgs, DecisionHint, Incoming, Out, OutcomeCommand, Request, check_hello,
    parse_incoming, read_line,
};

/// Unreadable lines tolerated before the engine is considered broken (as the desktop's link).
const MAX_BAD_LINES: u32 = 100;
/// Events and notices skipped while waiting for one result.
const MAX_NOISE: u32 = 10_000;

#[derive(Debug, PartialEq, Eq)]
pub enum ClientError {
    /// The server said no (outcomes off, a bad id, a store error): the text is the server's, made printable.
    Refused(String),
    /// The server broke the protocol; the connection is dead.
    Hostile(String),
    /// A reply that was well formed JSON but not what the call returns.
    Malformed(String),
    /// The link failed.
    Io(String),
}

impl std::fmt::Display for ClientError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Refused(s) | Self::Malformed(s) | Self::Io(s) => write!(f, "{s}"),
            Self::Hostile(s) => write!(f, "the engine broke the protocol: {s}"),
        }
    }
}
impl std::error::Error for ClientError {}

pub trait Transport {
    fn call(&mut self, call: Call) -> Result<Value, ClientError>;

    /// The next decision hint of a link that opted in to push, waiting up to `wait`. The hint only
    /// says that a decision changed: refetch it, never render from it. A transport without push
    /// waits out `wait` and says nothing (the caller then refetches anyway).
    fn next_hint(&mut self, wait: Duration) -> Result<Option<DecisionHint>, ClientError> {
        std::thread::sleep(wait);
        Ok(None)
    }
}

/// The server in this process: for tests and headless CI only (plan D22), never a surface default.
#[cfg(any(test, feature = "inproc-test"))]
pub struct InProc(pub crate::outcome_service::Service);

#[cfg(any(test, feature = "inproc-test"))]
impl Transport for InProc {
    fn call(&mut self, call: Call) -> Result<Value, ClientError> {
        self.0
            .handle(call)
            .map_err(|e| ClientError::Refused(synthesis_harness::sanitize_for_display(&e)))
    }
}

/// `synt engine --stdio` (the WSL engine link): one request line out, lines in until its result.
/// Waiting has no timeout here: the process that owns the pipes runs the watchdog (the desktop's
/// `remote.rs` kills a silent engine).
pub struct Stdio<R: BufRead, W: Write> {
    input: R,
    output: W,
    next: u64,
    dead: Option<String>,
}

impl<R: BufRead, W: Write> Stdio<R, W> {
    /// Reads `hello` and refuses any engine of another version or protocol (exact match).
    pub fn connect(mut input: R, output: W) -> Result<Self, ClientError> {
        let line = read_line(&mut input)
            .map_err(|e| ClientError::Io(e.to_string()))?
            .ok_or_else(|| ClientError::Io("the engine closed before hello".into()))?;
        check_hello(&line).map_err(ClientError::Refused)?;
        Ok(Self {
            input,
            output,
            next: 0,
            dead: None,
        })
    }

    fn poison(&mut self, why: &str) -> ClientError {
        self.dead = Some(why.to_string());
        ClientError::Hostile(why.to_string())
    }
}

impl<R: BufRead, W: Write> Transport for Stdio<R, W> {
    fn call(&mut self, call: Call) -> Result<Value, ClientError> {
        if let Some(why) = &self.dead {
            return Err(ClientError::Hostile(why.clone()));
        }
        self.next += 1;
        let id = self.next;
        let mut line = serde_json::to_vec(&Request { id, call })
            .map_err(|e| ClientError::Io(e.to_string()))?;
        line.push(b'\n');
        self.output
            .write_all(&line)
            .and_then(|()| self.output.flush())
            .map_err(|e| ClientError::Io(e.to_string()))?;
        let (mut bad, mut noise) = (0, 0);
        loop {
            let raw = match read_line(&mut self.input) {
                Ok(Some(l)) => l,
                Ok(None) => return Err(ClientError::Io("the engine closed the link".into())),
                Err(e) => return Err(self.poison(&e.to_string())),
            };
            match parse_incoming(&raw) {
                Err(_) => {
                    bad += 1;
                    if bad > MAX_BAD_LINES {
                        return Err(self.poison("too many unreadable lines"));
                    }
                }
                Ok(Incoming::Event(_)) => {
                    noise += 1;
                    if noise > MAX_NOISE {
                        return Err(self.poison("events without a result"));
                    }
                }
                Ok(Incoming::Msg(m)) => match *m {
                    Out::Result { id: rid, ok, error } if rid == id => {
                        return match (ok, error) {
                            (Some(v), None) => Ok(v),
                            (None, Some(e)) => Err(ClientError::Refused(
                                synthesis_harness::sanitize_for_display(&e),
                            )),
                            _ => Err(self.poison("a result with neither or both of ok and error")),
                        };
                    }
                    Out::Result { .. } => return Err(self.poison("a result nobody asked for")),
                    Out::Notice { .. } => {
                        noise += 1;
                        if noise > MAX_NOISE {
                            return Err(self.poison("notices without a result"));
                        }
                    }
                    _ => return Err(self.poison("a message nobody asked for")),
                },
            }
        }
    }
}

pub struct OutcomeClient<T: Transport> {
    t: T,
    surface: Surface,
    /// The key of a `create` whose result is not known (the link failed): the next `create` of this
    /// client sends the same key, so the server returns the first draft instead of making a second.
    create_key: Option<String>,
}

fn malformed<T>(what: &str) -> Result<T, ClientError> {
    Err(ClientError::Malformed(format!("unreadable {what}")))
}

fn object(v: Value, what: &str) -> Result<Value, ClientError> {
    if v.is_object() {
        Ok(v)
    } else {
        malformed(what)
    }
}

impl<T: Transport> OutcomeClient<T> {
    pub fn new(t: T, surface: Surface) -> Self {
        Self {
            t,
            surface,
            create_key: None,
        }
    }

    fn args(&self, key: &str) -> CommandArgs {
        CommandArgs {
            idempotency_key: key.into(),
            surface: self.surface,
        }
    }

    /// `{enabled, protocol, schema, ready}`; answers while outcomes are off.
    pub fn status(&mut self) -> Result<Value, ClientError> {
        self.t.call(Call::OrchestratorStatus)
    }

    /// Creates an outcome (the orchestrator process only; the contract waits for `approve_contract`).
    /// Returns `{id, state, contract_version, contract_hash}`.
    pub fn create(
        &mut self,
        workspace: &str,
        base_commit: &str,
        contract: synthesis_core::outcome::Contract,
    ) -> Result<Value, ClientError> {
        let key = self
            .create_key
            .get_or_insert_with(|| format!("create-{}", uuid::Uuid::new_v4()))
            .clone();
        let r = self.create_with_key(workspace, base_commit, contract, &key);
        // an answer (created or refused) ends the key; a failed link or reply leaves it for the retry
        if matches!(r, Ok(_) | Err(ClientError::Refused(_))) {
            self.create_key = None;
        }
        r
    }

    /// [`Self::create`] with the caller's key: after a lost reply, the same call with the same key
    /// returns the draft the first one made instead of a second one.
    pub fn create_with_key(
        &mut self,
        workspace: &str,
        base_commit: &str,
        contract: synthesis_core::outcome::Contract,
        key: &str,
    ) -> Result<Value, ClientError> {
        let v = self.t.call(Call::OutcomeCreate {
            workspace: workspace.into(),
            base_commit: base_commit.into(),
            contract: Box::new(contract),
            surface: self.surface,
            idempotency_key: Some(key.into()),
        })?;
        match v.get("id").and_then(Value::as_str) {
            Some(_) => Ok(v),
            None => malformed("created outcome"),
        }
    }

    /// The explicit migration to schema v5 (a backup first); the setting must already be on. The
    /// reply carries the integrity disclosure (`disclosure: {id, text}`) the surface must show; a
    /// reply without it, or with an id or text other than this build's, is refused (fail closed).
    pub fn enable(&mut self) -> Result<Value, ClientError> {
        let v = self.t.call(Call::OrchestratorEnable {
            surface: self.surface,
        })?;
        // the disclosure must be this build's own, id and text: an owner that words it differently
        // (an older or another build) is refused, not shown
        let is = |k: &str, want: &str| {
            v.pointer(&format!("/disclosure/{k}"))
                .and_then(Value::as_str)
                == Some(want)
        };
        if is("id", crate::outcome_policy::DISCLOSURE_ID)
            && is("text", crate::outcome_policy::INTEGRITY_DISCLOSURE)
        {
            Ok(v)
        } else {
            malformed("enable reply without this build's integrity disclosure (id and text)")
        }
    }

    /// See [`Transport::next_hint`].
    pub fn next_hint(&mut self, wait: Duration) -> Result<Option<DecisionHint>, ClientError> {
        self.t.next_hint(wait)
    }

    pub fn outcomes(&mut self, workspace: Option<&str>) -> Result<Vec<OutcomeRow>, ClientError> {
        let v = self.t.call(Call::OutcomeList {
            workspace: workspace.map(str::to_string),
        })?;
        match v.get("outcomes").and_then(Value::as_array) {
            Some(a) => a
                .iter()
                .map(|o| OutcomeRow::from_json(o).ok_or(()))
                .collect::<Result<_, _>>()
                .or_else(|()| malformed("outcome list")),
            None => malformed("outcome list"),
        }
    }

    pub fn outcome(&mut self, id: &str) -> Result<OutcomeRow, ClientError> {
        let v = self.t.call(Call::OutcomeGet { id: id.into() })?;
        OutcomeRow::from_json(&v).map_or_else(|| malformed("outcome"), Ok)
    }

    /// `{version, hash, approved, contract}` of a stored contract (default: the current one).
    pub fn contract(&mut self, id: &str, version: Option<u32>) -> Result<Value, ClientError> {
        let v = self.t.call(Call::OutcomeContractGet {
            id: id.into(),
            version,
        })?;
        object(v, "contract")
    }

    /// `{plan_version, items}` of the current plan, as stored.
    pub fn plan(&mut self, id: &str) -> Result<Value, ClientError> {
        let v = self.t.call(Call::PlanGet { id: id.into() })?;
        object(v, "plan")
    }

    /// The evidence rows of the outcome, as stored.
    pub fn evidence(&mut self, outcome: &str) -> Result<Vec<Value>, ClientError> {
        let v = self.t.call(Call::EvidenceGet {
            outcome: outcome.into(),
        })?;
        match v.get("evidence").and_then(Value::as_array) {
            Some(a) if a.iter().all(Value::is_object) => Ok(a.clone()),
            _ => malformed("evidence list"),
        }
    }

    /// `{cost_usd{spent,limit}, wall_secs{spent}, reserved_usd, planner_spent_usd}`; no `remaining`, no tokens.
    pub fn budget(&mut self, outcome: &str) -> Result<Value, ClientError> {
        let v = self.t.call(Call::OutcomeBudget { id: outcome.into() })?;
        object(v, "budget")
    }

    /// The attempt's candidate commit against the outcome's base: capped files and patches.
    pub fn diff(&mut self, outcome: &str, attempt: &str) -> Result<Value, ClientError> {
        let v = self.t.call(Call::AttemptDiff {
            outcome: outcome.into(),
            id: attempt.into(),
        })?;
        match v.get("files") {
            Some(f) if f.is_array() => Ok(v),
            _ => malformed("diff"),
        }
    }

    /// One work item with its `attempts`, as stored.
    pub fn work_item(&mut self, outcome: &str, id: &str) -> Result<Value, ClientError> {
        let v = self.t.call(Call::WorkItemGet {
            outcome: outcome.into(),
            id: id.into(),
        })?;
        object(v, "work item")
    }

    /// `{work_item_id, attempt}` of one attempt.
    pub fn attempt(&mut self, outcome: &str, id: &str) -> Result<Value, ClientError> {
        let v = self.t.call(Call::AttemptInspect {
            outcome: outcome.into(),
            id: id.into(),
        })?;
        object(v, "attempt")
    }

    /// Every decision of the outcome as the server states it (a brief that names a preferred
    /// option makes the whole list an error: it is not shown).
    pub fn decisions(&mut self, outcome: &str) -> Result<Vec<DecisionCard>, ClientError> {
        let v = self.t.call(Call::DecisionList {
            outcome: outcome.into(),
        })?;
        let Some(a) = v.get("decisions").and_then(Value::as_array) else {
            return malformed("decision list");
        };
        a.iter()
            .map(|d| DecisionCard::from_json(d).map_err(ClientError::Malformed))
            .collect()
    }

    /// Answers `card` with `choice`: the revision and brief hash sent are the ones the card carries,
    /// never fetched again. Refused here, before anything is sent, when the card is not pending or
    /// `choice` is not one of its options (there is no default).
    pub fn resolve(
        &mut self,
        outcome: &str,
        card: &DecisionCard,
        choice: &str,
        key: &str,
        note: Option<&str>,
    ) -> Result<CommandReply, ClientError> {
        let offered = Picker::new(card).is_some_and(|p| p.options.iter().any(|o| o.id == choice));
        if !offered {
            return Err(ClientError::Refused(format!(
                "decision {} is {} and does not offer {choice:?}",
                card.id, card.state
            )));
        }
        let v = self.t.call(Call::DecisionResolve {
            outcome: outcome.into(),
            id: card.id.clone(),
            expected_revision: card.revision,
            choice: choice.into(),
            brief_sha256: card.brief_sha256.clone(),
            note: note.map(str::to_string),
            cmd: self.args(key),
        })?;
        self.reply(v, key, "decision.resolve")
    }

    pub fn approve_contract(
        &mut self,
        id: &str,
        version: u32,
        hash: &str,
        key: &str,
    ) -> Result<CommandReply, ClientError> {
        let v = self.t.call(Call::OutcomeContractApprove {
            id: id.into(),
            version,
            hash: hash.into(),
            cmd: self.args(key),
        })?;
        self.reply(v, key, "outcome.contract.approve")
    }

    /// Queues a revised contract (the version inside is ignored: the owner numbers it). The settled
    /// reply carries `{version, hash}`; approve it with [`Self::approve_contract`] after reading
    /// [`Self::contract_diff`].
    pub fn revise_contract(
        &mut self,
        id: &str,
        expected_revision: i64,
        contract: synthesis_core::outcome::Contract,
        key: &str,
    ) -> Result<CommandReply, ClientError> {
        let v = self.t.call(Call::OutcomeContractRevise {
            id: id.into(),
            expected_revision,
            contract: Box::new(contract),
            cmd: self.args(key),
        })?;
        self.reply(v, key, "outcome.contract.revise")
    }

    /// What `version` changes against the outcome's approved contract, as the server computed it:
    /// `{base, version, hash, approved, diff{changes,widens,weakens}, widens_scope}`.
    pub fn contract_diff(&mut self, id: &str, version: u32) -> Result<Value, ClientError> {
        let v = self.t.call(Call::OutcomeContractDiff {
            id: id.into(),
            version,
        })?;
        match (v.get("diff"), v.get("widens_scope")) {
            (Some(d), Some(Value::Bool(_))) if d.is_object() => Ok(v),
            _ => malformed("contract diff"),
        }
    }

    /// The person's line to the worker of `work_item` (S1 4.6; the server must list `attempt_message`
    /// in its `features`). It is checked here before anything is sent (native Windows, the text), and
    /// again by the server and the owner. Poll [`Self::command_status`]: the settled reply's result is
    /// `{delivery: "live"|"queued", seq, attempt?, rebriefed}`, or the command is `denied` with a reason.
    pub fn send_worker_message(
        &mut self,
        outcome: &str,
        work_item: &str,
        text: &str,
        brief_change: bool,
        expected_revision: Option<i64>,
        key: &str,
    ) -> Result<CommandReply, ClientError> {
        if cfg!(windows) {
            return Err(ClientError::Refused(
                "worker messages are not available on native Windows; open the folder in WSL2"
                    .into(),
            ));
        }
        synthesis_session::outcome::check_message_text(text)
            .map_err(|e| ClientError::Refused(e.into()))?;
        let v = self.t.call(Call::AttemptMessage {
            outcome: outcome.into(),
            work_item: work_item.into(),
            text: text.into(),
            brief_change,
            expected_revision,
            proposal_sha256: None,
            cmd: self.args(key),
        })?;
        self.reply(v, key, "attempt.message")
    }

    /// `kind`: `start`, `pause`, `resume`, `cancel` or `ack`.
    pub fn command(
        &mut self,
        kind: &str,
        id: &str,
        expected_revision: Option<i64>,
        key: &str,
    ) -> Result<CommandReply, ClientError> {
        let c = OutcomeCommand {
            id: id.into(),
            expected_revision,
            cmd: self.args(key),
        };
        let call = match kind {
            "start" => Call::OutcomeStart(c),
            "pause" => Call::OutcomePause(c),
            "resume" => Call::OutcomeResume(c),
            "cancel" => Call::OutcomeCancel(c),
            "ack" => Call::OutcomeAck(c),
            _ => return Err(ClientError::Refused(format!("no outcome command {kind:?}"))),
        };
        let v = self.t.call(call)?;
        self.reply(v, key, &format!("outcome.{kind}"))
    }

    pub fn command_status(
        &mut self,
        outcome: &str,
        key: &str,
    ) -> Result<CommandReply, ClientError> {
        let v = self.t.call(Call::CommandStatus {
            outcome: outcome.into(),
            idempotency_key: key.into(),
        })?;
        let r = CommandReply::from_json(&v);
        match r {
            Some(r) if r.idempotency_key == key => Ok(r),
            _ => Err(ClientError::Hostile("a status for another command".into())),
        }
    }

    /// One page of events after `after`; resume from the returned cursor. Sequence numbers must
    /// rise from `after`: a server that repeats or reorders is not believed.
    pub fn events(&mut self, outcome: &str, after: u64) -> Result<(Vec<Value>, u64), ClientError> {
        let v = self.t.call(Call::EventsSubscribe {
            outcome: outcome.into(),
            after_event_id: after,
        })?;
        let (Some(events), Some(next)) = (
            v.get("events").and_then(Value::as_array),
            v.get("next_event_id").and_then(Value::as_u64),
        ) else {
            return malformed("event page");
        };
        let mut last = after;
        for e in events {
            match e.get("seq").and_then(Value::as_u64) {
                Some(s) if s > last => last = s,
                _ => return Err(ClientError::Hostile("events out of order".into())),
            }
        }
        if next != last {
            return Err(ClientError::Hostile(
                "a cursor that is not the last event".into(),
            ));
        }
        Ok((events.clone(), next))
    }

    /// The reply of a mutating call must be about the command that was sent.
    fn reply(&self, v: Value, key: &str, kind: &str) -> Result<CommandReply, ClientError> {
        match CommandReply::from_json(&v) {
            Some(r) if r.idempotency_key == key && r.kind == kind => Ok(r),
            Some(_) => Err(ClientError::Hostile("a reply to another command".into())),
            None => malformed("command reply"),
        }
    }
}
