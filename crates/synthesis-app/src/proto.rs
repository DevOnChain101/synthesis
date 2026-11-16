//! The wire format of `synt engine --stdio` (INTERNAL, docs/specs/desktop.md "Engine over stdio"):
//! one JSON object per line in each direction. The engine speaks first (`hello`); the client sends
//! `{id, method, params}` requests; the engine answers with `{msg:"result", id, ok|error}` and streams
//! the ordinary event [`Envelope`] lines (no `msg` key) plus the asks. Both sides treat every line as
//! untrusted: bounded length, strict parse, a bad line never panics.

use std::io::{self, BufRead, Read};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use synthesis_events::Envelope;
use synthesis_harness::UserReply;
use synthesis_permissions::{Decision, Mode, RuleSpec, Suggestion};
use synthesis_session::outcome::Surface;
use synthesis_tools::NeedsUser;

use crate::asks::{Ask, AskApproval, Choice};

/// Bumped when a message changes shape. The version handshake is exact: both numbers must match.
pub const PROTOCOL: u32 = 4;
/// The engine's own version (one workspace version for the app and the engine).
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
/// Longest accepted line (a tool result can carry a big file).
pub const MAX_LINE: usize = 8 << 20;

/// Starts the error of a restart request that was refused BEFORE anything was ended: the session goes on.
/// Any other error of a restart request means the engine cannot be trusted to have a session: the client drops it.
pub const REFUSED: &str = "refused: ";
/// Starts the error of a restart that ended the session and could not start any: the engine exits.
pub const FATAL: &str = "fatal: ";

#[derive(Debug, Serialize, Deserialize)]
pub struct Request {
    pub id: u64,
    #[serde(flatten)]
    pub call: Call,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "method", content = "params", rename_all = "snake_case")]
pub enum Call {
    /// Prepares the session (once). `cwd` is a folder path on the engine's own OS.
    Start {
        cwd: String,
        /// Continue this stored session of the same folder (an id from `workspaces.list`).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        resume: Option<String>,
        /// Start on this `provider/model`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        model: Option<String>,
    },
    /// Starts a task; `{msg:"task_done"}` follows when it ended.
    Send {
        text: String,
        /// Ids `image.put` issued (the engine holds the bytes).
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        images: Vec<String>,
    },
    Answer {
        id: String,
        choice: Choice,
    },
    AnswerInput {
        id: String,
        reply: Option<Reply>,
    },
    /// Changes the permission mode (never to bypassPermissions); the result carries `{mode}`.
    SetMode {
        mode: String,
    },
    /// The workspaces of the store (and this session's folder) with their sessions; bounded.
    #[serde(rename = "workspaces.list")]
    WorkspacesList,
    #[serde(rename = "sessions.list")]
    SessionsList {
        workspace: String,
    },
    /// The stored conversation of a session, read-only (newest events within the bounds).
    #[serde(rename = "session.preview")]
    SessionPreview {
        id: String,
    },
    /// Ends this session and continues the stored one of the same folder (a new `started` report).
    #[serde(rename = "session.resume")]
    SessionResume {
        id: String,
    },
    /// Ends this session and starts a fresh one in the same folder.
    #[serde(rename = "session.new")]
    SessionNew,
    #[serde(rename = "session.rename")]
    SessionRename {
        id: String,
        title: String,
    },
    /// Hides this session (only the engine's own) and starts a fresh one.
    #[serde(rename = "session.close")]
    SessionClose {
        id: String,
    },
    /// Hides every session of the workspace key that no other process holds; the own workspace also
    /// continues in a fresh session.
    #[serde(rename = "workspace.close")]
    WorkspaceClose {
        workspace: String,
    },
    #[serde(rename = "usage.summary")]
    UsageSummary {
        tz_min: i32,
    },
    #[serde(rename = "trust.state")]
    TrustState,
    /// The person's choice; the engine decides, stores and restarts the session trusted.
    #[serde(rename = "trust.set")]
    TrustSet {
        mode: crate::engine_methods::Mode,
    },
    #[serde(rename = "trust.revoke")]
    TrustRevoke {
        root: String,
    },
    /// Ends this session and continues it on `model`.
    #[serde(rename = "model.set")]
    ModelSet {
        model: String,
    },
    /// The tree of the session folder under `dir` ("" is all of it), `offset` rows in; paged.
    #[serde(rename = "fs.list")]
    FsList {
        #[serde(default)]
        dir: String,
        #[serde(default)]
        offset: u32,
    },
    /// A text preview of a file of the session folder (relative path).
    #[serde(rename = "fs.read")]
    FsRead {
        path: String,
    },
    /// Changed files and the diff, only in a trusted folder and only through the engine's hardened git.
    #[serde(rename = "fs.git_status")]
    FsGitStatus,
    #[serde(rename = "fs.git_diff")]
    FsGitDiff,
    /// One chunk of an image (base64); the first names the type and size, the last finishes it.
    #[serde(rename = "image.put")]
    ImagePut {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        upload: Option<String>,
        seq: u32,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        mime: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        size: Option<u64>,
        data: String,
        last: bool,
    },
    #[serde(rename = "image.drop")]
    ImageDrop {
        id: String,
    },
    #[serde(rename = "rewind.list")]
    RewindList,
    #[serde(rename = "rewind.dry_run")]
    RewindDryRun {
        checkpoint: Option<String>,
    },
    /// Applies the dry run the engine issued `token` for (single use).
    #[serde(rename = "rewind.apply")]
    RewindApply {
        token: String,
    },
    #[serde(rename = "clean.plan")]
    CleanPlan,
    #[serde(rename = "clean.apply")]
    CleanApply {
        token: String,
        backup: bool,
    },
    #[serde(rename = "memory.list")]
    MemoryList,
    #[serde(rename = "memory.retrieve")]
    MemoryRetrieve {
        query: String,
    },
    #[serde(rename = "memory.show")]
    MemoryShow {
        id: String,
    },
    #[serde(rename = "memory.add")]
    MemoryAdd {
        text: String,
        user: bool,
    },
    #[serde(rename = "memory.forget")]
    MemoryForget {
        id: String,
    },
    /// A `#` note: appended to an instruction file of the engine's machine.
    #[serde(rename = "memory.note")]
    MemoryNote {
        text: String,
        user: bool,
    },
    #[serde(rename = "config.effort")]
    ConfigEffort {
        effort: Option<String>,
    },
    /// `/context`: a token count (`1m`, `200000`) or `unset`, for the model in use.
    #[serde(rename = "context.set")]
    ContextSet {
        value: String,
    },
    #[serde(rename = "doctor.summary")]
    DoctorSummary,
    #[serde(rename = "mcp.list")]
    McpList,
    /// A URL server into the user file; `command`, `env` and `header` exist only to be refused.
    #[serde(rename = "mcp.add")]
    McpAdd {
        name: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        url: Option<String>,
        transport: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        env: Vec<String>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        header: Vec<String>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        command: Vec<String>,
    },
    #[serde(rename = "mcp.remove")]
    McpRemove {
        name: String,
    },
    /// `{enabled, protocol, schema}`: the one outcome method that answers while the feature is off.
    #[serde(rename = "orchestrator.status")]
    OrchestratorStatus,
    #[serde(rename = "outcome.list")]
    OutcomeList {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        workspace: Option<String>,
    },
    #[serde(rename = "outcome.get")]
    OutcomeGet {
        id: String,
    },
    /// The stored contract (`version` default: the outcome's current one) and whether its approval verifies.
    #[serde(rename = "outcome.contract.get")]
    OutcomeContractGet {
        id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        version: Option<u32>,
    },
    /// Creates an outcome (draft, contract v1, waiting for its approval). Only the orchestrator
    /// process serves it: creating takes the control-session lock, which is the owner's (ADR-017).
    #[serde(rename = "outcome.create")]
    OutcomeCreate {
        /// The repository root (an absolute path on the orchestrator's machine).
        workspace: String,
        /// The commit the attempts start from (full lowercase hex id).
        base_commit: String,
        contract: Box<synthesis_core::outcome::Contract>,
        surface: Surface,
        /// A retry with the same key (after a lost reply) returns the draft the first call made;
        /// the same key with another request is refused. Absent: no retry protection.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        idempotency_key: Option<String>,
    },
    /// The explicit migration (backup, schema v5) once `synt.orchestrator.enabled` is true. Only the
    /// orchestrator process serves it.
    #[serde(rename = "orchestrator.enable")]
    OrchestratorEnable {
        surface: Surface,
    },
    #[serde(rename = "outcome.contract.approve")]
    OutcomeContractApprove {
        id: String,
        version: u32,
        hash: String,
        #[serde(flatten)]
        cmd: CommandArgs,
    },
    #[serde(rename = "outcome.start")]
    OutcomeStart(OutcomeCommand),
    #[serde(rename = "outcome.pause")]
    OutcomePause(OutcomeCommand),
    #[serde(rename = "outcome.resume")]
    OutcomeResume(OutcomeCommand),
    #[serde(rename = "outcome.cancel")]
    OutcomeCancel(OutcomeCommand),
    #[serde(rename = "outcome.ack")]
    OutcomeAck(OutcomeCommand),
    #[serde(rename = "plan.get")]
    PlanGet {
        id: String,
    },
    #[serde(rename = "work_item.get")]
    WorkItemGet {
        outcome: String,
        id: String,
    },
    #[serde(rename = "attempt.inspect")]
    AttemptInspect {
        outcome: String,
        id: String,
    },
    /// What the outcome's attempts used against its contract (read-only; ADR-021 notes).
    #[serde(rename = "outcome.budget")]
    OutcomeBudget {
        id: String,
    },
    /// The candidate commit of an attempt against the outcome's base commit (read-only, capped).
    #[serde(rename = "attempt.diff")]
    AttemptDiff {
        outcome: String,
        id: String,
    },
    #[serde(rename = "decision.list")]
    DecisionList {
        outcome: String,
    },
    #[serde(rename = "decision.get")]
    DecisionGet {
        outcome: String,
        id: String,
    },
    /// A person's answer. `expected_revision` and `brief_sha256` are what the person was shown; the
    /// server never fills either in, and there is no default `choice`.
    #[serde(rename = "decision.resolve")]
    DecisionResolve {
        outcome: String,
        id: String,
        expected_revision: i64,
        choice: String,
        brief_sha256: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        note: Option<String>,
        #[serde(flatten)]
        cmd: CommandArgs,
    },
    #[serde(rename = "evidence.get")]
    EvidenceGet {
        outcome: String,
    },
    #[serde(rename = "command.status")]
    CommandStatus {
        outcome: String,
        idempotency_key: String,
    },
    /// Events of the outcome's control session after a cursor (a page; reconnect resumes from the last `seq`).
    #[serde(rename = "events.subscribe")]
    EventsSubscribe {
        outcome: String,
        #[serde(default)]
        after_event_id: u64,
    },
    /// `{started}`: false when it ran between tasks (`{msg:"compacted"}` follows).
    Compact {
        instructions: Option<String>,
    },
    Clear,
    Cancel,
    #[serde(alias = "shutdown")]
    Close,
}

/// What every mutating outcome call carries: the retry key (same key, same stored result, one effect)
/// and the surface it comes from (self-asserted until the orchestrator endpoint authenticates clients).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommandArgs {
    pub idempotency_key: String,
    pub surface: Surface,
}

/// A mutating call on a whole outcome; `expected_revision` is the outcome revision the person saw.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OutcomeCommand {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_revision: Option<i64>,
    #[serde(flatten)]
    pub cmd: CommandArgs,
}

impl Call {
    /// The outcome methods (protocol v4), served by [`crate::outcome_service`] and never by a session.
    pub fn is_outcome(&self) -> bool {
        matches!(
            self,
            Call::OrchestratorStatus
                | Call::OutcomeCreate { .. }
                | Call::OrchestratorEnable { .. }
                | Call::OutcomeList { .. }
                | Call::OutcomeGet { .. }
                | Call::OutcomeContractGet { .. }
                | Call::OutcomeContractApprove { .. }
                | Call::OutcomeStart(_)
                | Call::OutcomePause(_)
                | Call::OutcomeResume(_)
                | Call::OutcomeCancel(_)
                | Call::OutcomeAck(_)
                | Call::PlanGet { .. }
                | Call::WorkItemGet { .. }
                | Call::AttemptInspect { .. }
                | Call::OutcomeBudget { .. }
                | Call::AttemptDiff { .. }
                | Call::DecisionList { .. }
                | Call::DecisionGet { .. }
                | Call::DecisionResolve { .. }
                | Call::EvidenceGet { .. }
                | Call::CommandStatus { .. }
                | Call::EventsSubscribe { .. }
        )
    }

    /// The surface a mutating outcome call says it comes from (`None`: a read).
    pub fn surface(&self) -> Option<Surface> {
        match self {
            Call::OutcomeCreate { surface, .. } | Call::OrchestratorEnable { surface } => {
                Some(*surface)
            }
            Call::OutcomeContractApprove { cmd, .. } | Call::DecisionResolve { cmd, .. } => {
                Some(cmd.surface)
            }
            Call::OutcomeStart(c)
            | Call::OutcomePause(c)
            | Call::OutcomeResume(c)
            | Call::OutcomeCancel(c)
            | Call::OutcomeAck(c) => Some(c.cmd.surface),
            _ => None,
        }
    }

    /// The calls that end the session and start another in this process.
    pub fn is_restart(&self) -> bool {
        matches!(
            self,
            Call::TrustSet { .. }
                | Call::ModelSet { .. }
                | Call::SessionNew
                | Call::SessionResume { .. }
                | Call::SessionClose { .. }
                | Call::WorkspaceClose { .. }
        )
    }
}

/// The answer to a plan or question dialog.
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Reply {
    Answers(Value),
    Plan {
        approved: bool,
        message: Option<String>,
        mode: Option<String>,
    },
}

impl Reply {
    /// A plan reply continues in default or acceptEdits only: never bypassPermissions, and no other
    /// mode a reply could use to widen what runs unasked.
    pub fn into_user_reply(self) -> UserReply {
        match self {
            Reply::Answers(v) => UserReply::Answers(v),
            Reply::Plan {
                approved,
                message,
                mode,
            } => UserReply::Plan {
                approved,
                message,
                mode: mode
                    .as_deref()
                    .and_then(Mode::parse)
                    .filter(|m| matches!(m, Mode::Default | Mode::AcceptEdits)),
            },
        }
    }
}

/// The opt-in push of an orchestrator link (`attach {push: ["decision"]}`, ADR-021 section 3): a
/// hint that decision `id` of `outcome` changed. Ids and codes only, never brief text: the client
/// refetches with `decision.get` and renders what the server answers, never the line. It is not an
/// [`Out`] variant on purpose: the stdio engine never sends it, so there it stays a protocol error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecisionHint {
    pub outcome: String,
    pub id: String,
    pub revision: i64,
    pub state: String,
}

/// The `DecisionState` words a hint may carry.
const HINT_STATES: [&str; 7] = [
    "preparing",
    "pending",
    "needs_more_info",
    "approved",
    "rejected",
    "expired",
    "invalidated",
];

impl DecisionHint {
    pub fn to_line(&self) -> Value {
        json!({"msg": "decision", "outcome": self.outcome, "id": self.id,
               "revision": self.revision, "state": self.state})
    }

    /// Strict: exactly these five keys, id shapes and a known state; anything else is not a hint.
    pub fn parse(v: &Value) -> Option<Self> {
        let o = v.as_object().filter(|o| o.len() == 5)?;
        let word = |k: &str| o.get(k)?.as_str().map(str::to_string);
        let (outcome, id, state) = (word("outcome")?, word("id")?, word("state")?);
        let ids = [&outcome, &id]
            .iter()
            .all(|i| synthesis_view::outcomes::valid_alt_id(i));
        (o.get("msg")? == "decision" && ids && HINT_STATES.contains(&state.as_str())).then_some(
            Self {
                outcome,
                id,
                revision: o.get("revision")?.as_i64().filter(|r| *r >= 0)?,
                state,
            },
        )
    }
}

/// What the engine writes besides event envelopes.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "msg")]
pub enum Out {
    #[serde(rename = "hello")]
    Hello { version: String, protocol: u32 },
    #[serde(rename = "result")]
    Result {
        id: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        ok: Option<Value>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    },
    #[serde(rename = "ask.approval")]
    Approval {
        id: String,
        agent_id: Option<String>,
        tool: String,
        input: Value,
        dialog: String,
        prompt: String,
        reason: String,
        origin: Option<String>,
        suggestions: Vec<Value>,
    },
    #[serde(rename = "ask.input")]
    Input { id: String, req: NeedsUser },
    /// The ask with this id is gone.
    #[serde(rename = "withdraw")]
    Withdraw { id: String },
    #[serde(rename = "task_done")]
    TaskDone { error: Option<String> },
    /// The session was ended for a restart (`trust.set`, `model.set`, `session.*`): what follows is the new one.
    #[serde(rename = "restarted")]
    Restarted,
    /// `compact` between tasks ended.
    #[serde(rename = "compacted")]
    Compacted { error: Option<String> },
    /// Something the engine wants shown (a dropped oversized event, ...).
    #[serde(rename = "notice")]
    Notice { text: String },
}

impl Out {
    pub fn hello() -> Self {
        Out::Hello {
            version: VERSION.into(),
            protocol: PROTOCOL,
        }
    }

    pub fn from_ask(a: Ask) -> Self {
        match a {
            Ask::Approval(a) => Out::Approval {
                id: a.id,
                agent_id: a.agent_id,
                tool: a.tool,
                input: a.input,
                dialog: a.dialog.into(),
                prompt: a.prompt,
                reason: a.reason,
                origin: a.origin,
                suggestions: a.suggestions.iter().map(suggestion_json).collect(),
            },
            Ask::Input(a) => Out::Input {
                id: a.id,
                req: a.req,
            },
            Ask::Withdraw(id) => Out::Withdraw { id },
        }
    }

    /// The client's view of an approval ask. Text is made printable: the engine is not trusted.
    pub fn into_approval(self) -> Option<AskApproval> {
        let Out::Approval {
            id,
            agent_id,
            tool,
            input,
            dialog,
            prompt,
            reason,
            origin,
            suggestions,
        } = self
        else {
            return None;
        };
        let clean = synthesis_harness::sanitize_for_display;
        Some(AskApproval {
            id,
            agent_id,
            tool: clean(&tool),
            input,
            dialog: if dialog == "outside_read" {
                "outside_read"
            } else {
                "permission"
            },
            prompt: clean(&prompt),
            reason: clean(&reason),
            origin: origin.map(|o| clean(&o)),
            // an offer the client cannot read is not shown (and so never accepted)
            suggestions: suggestions
                .iter()
                .filter_map(suggestion_from_json)
                .collect(),
        })
    }
}

fn decision_str(d: Decision) -> &'static str {
    match d {
        Decision::Ask => "ask",
        Decision::Deny => "deny",
        Decision::Allow | Decision::Classify => "allow",
    }
}

fn suggestion_json(s: &Suggestion) -> Value {
    match s {
        Suggestion::AddRules { rules, behavior } => json!({
            "kind": "rules",
            "behavior": decision_str(*behavior),
            "rules": rules.iter().map(|r| json!({"tool": r.tool_name, "content": r.rule_content})).collect::<Vec<_>>(),
        }),
        Suggestion::SetMode(m) => json!({"kind": "mode", "mode": m.as_str()}),
    }
}

fn suggestion_from_json(v: &Value) -> Option<Suggestion> {
    match v["kind"].as_str()? {
        "mode" => Mode::parse(v["mode"].as_str()?)
            .filter(|m| *m != Mode::BypassPermissions)
            .map(Suggestion::SetMode),
        "rules" => Some(Suggestion::AddRules {
            behavior: match v["behavior"].as_str()? {
                "ask" => Decision::Ask,
                "deny" => Decision::Deny,
                _ => Decision::Allow,
            },
            rules: v["rules"]
                .as_array()?
                .iter()
                .map(|r| {
                    Some(RuleSpec {
                        tool_name: r["tool"].as_str()?.to_string(),
                        rule_content: r["content"].as_str().map(str::to_string),
                    })
                })
                .collect::<Option<_>>()?,
        }),
        _ => None,
    }
}

/// One line the engine wrote.
#[derive(Debug)]
pub enum Incoming {
    Msg(Box<Out>),
    Event(Box<Envelope>),
}

pub fn parse_incoming(line: &[u8]) -> Result<Incoming, String> {
    let v: Value = serde_json::from_slice(line).map_err(|e| format!("not JSON: {e}"))?;
    if v.get("msg").is_some() {
        serde_json::from_value(v).map(|m| Incoming::Msg(Box::new(m)))
    } else {
        serde_json::from_value(v).map(|e| Incoming::Event(Box::new(e)))
    }
    .map_err(|e| format!("unreadable message: {e}"))
}

/// The client refuses an engine of another version or protocol (exact match, no negotiation).
pub fn check_hello(line: &[u8]) -> Result<(), String> {
    let Incoming::Msg(m) = parse_incoming(line)? else {
        return Err("the engine did not start with hello".into());
    };
    match *m {
        Out::Hello { version, protocol } if version == VERSION && protocol == PROTOCOL => Ok(()),
        Out::Hello { version, protocol } => Err(format!(
            "engine version mismatch: the engine is {} (protocol {protocol}), this app is {VERSION} (protocol {PROTOCOL}); install the matching version",
            synthesis_harness::sanitize_for_display(&version)
        )),
        _ => Err("the engine did not start with hello".into()),
    }
}

/// Reads one line of at most [`MAX_LINE`] bytes (without the line end); `None` at end of input. A longer
/// line is an error: the stream cannot be trusted after it.
pub fn read_line(r: &mut impl BufRead) -> io::Result<Option<Vec<u8>>> {
    let mut buf = vec![];
    let n = r
        .by_ref()
        .take(MAX_LINE as u64 + 1)
        .read_until(b'\n', &mut buf)?;
    if n == 0 {
        return Ok(None);
    }
    if buf.last() == Some(&b'\n') {
        buf.pop();
        if buf.last() == Some(&b'\r') {
            buf.pop();
        }
    } else if buf.len() > MAX_LINE {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "line too long"));
    }
    Ok(Some(buf))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_round_trip() {
        let r: Request =
            serde_json::from_str(r#"{"id":3,"method":"send","params":{"text":"hi"}}"#).unwrap();
        assert!(
            r.id == 3
                && matches!(r.call, Call::Send { ref text, ref images } if text == "hi" && images.is_empty())
        );
        let r: Request = serde_json::from_str(r#"{"id":4,"method":"cancel"}"#).unwrap();
        assert!(matches!(r.call, Call::Cancel));
        let r: Request = serde_json::from_str(r#"{"id":5,"method":"shutdown"}"#).unwrap();
        assert!(matches!(r.call, Call::Close));
        let r: Request = serde_json::from_str(
            r#"{"id":6,"method":"answer","params":{"id":"a","choice":"always"}}"#,
        )
        .unwrap();
        assert!(matches!(
            r.call,
            Call::Answer {
                choice: Choice::Always,
                ..
            }
        ));
        let s = serde_json::to_string(&Request {
            id: 1,
            call: Call::Start {
                cwd: "/w".into(),
                resume: None,
                model: None,
            },
        })
        .unwrap();
        assert_eq!(s, r#"{"id":1,"method":"start","params":{"cwd":"/w"}}"#);
    }

    /// Golden lines of the v4 outcome methods: what a surface writes is what the engine reads.
    #[test]
    fn outcome_requests_are_golden_lines() {
        let resolve = r#"{"id":9,"method":"decision.resolve","params":{"outcome":"o","id":"d","expected_revision":2,"choice":"REST","brief_sha256":"h","idempotency_key":"k","surface":"cli"}}"#;
        let r: Request = serde_json::from_str(resolve).unwrap();
        assert!(r.call.is_outcome() && !r.call.is_restart());
        assert!(matches!(
            r.call,
            Call::DecisionResolve { expected_revision: 2, ref choice, note: None, .. } if choice == "REST"
        ));
        assert_eq!(serde_json::to_string(&r).unwrap(), resolve);
        let start = r#"{"id":1,"method":"outcome.start","params":{"id":"o","expected_revision":3,"idempotency_key":"k","surface":"tui"}}"#;
        let r: Request = serde_json::from_str(start).unwrap();
        assert!(matches!(r.call, Call::OutcomeStart(ref c) if c.expected_revision == Some(3)));
        assert_eq!(serde_json::to_string(&r).unwrap(), start);
        for line in [
            r#"{"id":1,"method":"orchestrator.status"}"#,
            r#"{"id":1,"method":"outcome.list","params":{}}"#,
            r#"{"id":1,"method":"outcome.get","params":{"id":"o"}}"#,
            r#"{"id":1,"method":"outcome.contract.get","params":{"id":"o"}}"#,
            r#"{"id":1,"method":"outcome.contract.approve","params":{"id":"o","version":1,"hash":"h","idempotency_key":"k","surface":"superagent"}}"#,
            r#"{"id":1,"method":"plan.get","params":{"id":"o"}}"#,
            r#"{"id":1,"method":"work_item.get","params":{"outcome":"o","id":"w"}}"#,
            r#"{"id":1,"method":"attempt.inspect","params":{"outcome":"o","id":"a"}}"#,
            r#"{"id":1,"method":"decision.list","params":{"outcome":"o"}}"#,
            r#"{"id":1,"method":"decision.get","params":{"outcome":"o","id":"d"}}"#,
            r#"{"id":1,"method":"evidence.get","params":{"outcome":"o"}}"#,
            r#"{"id":1,"method":"outcome.budget","params":{"id":"o"}}"#,
            r#"{"id":1,"method":"attempt.diff","params":{"outcome":"o","id":"a"}}"#,
            r#"{"id":1,"method":"command.status","params":{"outcome":"o","idempotency_key":"k"}}"#,
            r#"{"id":1,"method":"events.subscribe","params":{"outcome":"o","after_event_id":7}}"#,
            r#"{"id":1,"method":"outcome.pause","params":{"id":"o","idempotency_key":"k","surface":"cli"}}"#,
            r#"{"id":1,"method":"outcome.resume","params":{"id":"o","idempotency_key":"k","surface":"cli"}}"#,
            r#"{"id":1,"method":"outcome.cancel","params":{"id":"o","idempotency_key":"k","surface":"cli"}}"#,
            r#"{"id":1,"method":"outcome.ack","params":{"id":"o","idempotency_key":"k","surface":"cli"}}"#,
        ] {
            let r: Request = serde_json::from_str(line).unwrap_or_else(|e| panic!("{line}: {e}"));
            assert!(r.call.is_outcome(), "{line}");
        }
        assert!(!Call::Cancel.is_outcome());
    }

    #[test]
    fn a_resolve_without_what_the_person_was_shown_does_not_parse() {
        let ok = |p: &str| format!(r#"{{"id":1,"method":"decision.resolve","params":{{{p}}}}}"#);
        let all = r#""outcome":"o","id":"d","expected_revision":2,"choice":"A","brief_sha256":"h","idempotency_key":"k","surface":"cli""#;
        assert!(serde_json::from_str::<Request>(&ok(all)).is_ok());
        // no default choice, no revision, no brief hash, no key, an unknown surface
        for drop in [
            "\"choice\":\"A\",",
            "\"expected_revision\":2,",
            "\"brief_sha256\":\"h\",",
            ",\"idempotency_key\":\"k\"",
        ] {
            assert!(
                serde_json::from_str::<Request>(&ok(&all.replace(drop, ""))).is_err(),
                "{drop}"
            );
        }
        assert!(serde_json::from_str::<Request>(&ok(&all.replace("cli", "root"))).is_err());
    }

    #[test]
    fn malformed_requests_are_errors() {
        for bad in [
            "",
            "nope",
            r#"{"id":1}"#,
            r#"{"id":1,"method":"rm_rf"}"#,
            r#"{"id":"x","method":"cancel"}"#,
            r#"{"id":1,"method":"answer","params":{"id":"a","choice":"maybe"}}"#,
        ] {
            assert!(serde_json::from_str::<Request>(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn the_handshake_is_exact() {
        let ok = serde_json::to_vec(&Out::hello()).unwrap();
        assert!(check_hello(&ok).is_ok());
        let old = format!(r#"{{"msg":"hello","version":"0.0.0-old","protocol":{PROTOCOL}}}"#);
        assert!(
            check_hello(old.as_bytes())
                .unwrap_err()
                .contains("mismatch")
        );
        let next = format!(
            r#"{{"msg":"hello","version":"{VERSION}","protocol":{}}}"#,
            PROTOCOL + 1
        );
        assert!(
            check_hello(next.as_bytes())
                .unwrap_err()
                .contains("mismatch")
        );
        // a protocol 2 engine is refused cleanly by this client (and the other way round)
        let v2 = format!(r#"{{"msg":"hello","version":"{VERSION}","protocol":2}}"#);
        assert!(
            check_hello(v2.as_bytes())
                .unwrap_err()
                .contains("protocol 2")
        );
        // a protocol 3 engine (before outcomes) is refused too: the lock-step rule of ADR-016
        let v3 = format!(r#"{{"msg":"hello","version":"{VERSION}","protocol":3}}"#);
        assert!(
            check_hello(v3.as_bytes())
                .unwrap_err()
                .contains("protocol 3")
        );
        assert_eq!(PROTOCOL, 4);
        assert!(check_hello(br#"{"msg":"task_done","error":null}"#).is_err());
        assert!(check_hello(b"garbage").is_err());
    }

    #[test]
    fn incoming_lines_split_into_messages_and_events() {
        let ev = r#"{"v":1,"seq":1,"ts":"2026-01-01T00:00:00Z","session_id":"s","task_id":null,"turn_id":null,"agent_id":null,"type":"notice","payload":{"level":"info","text":"hi"}}"#;
        assert!(matches!(parse_incoming(ev.as_bytes()), Ok(Incoming::Event(e)) if e.seq == 1));
        let r = br#"{"msg":"result","id":2,"error":"no"}"#;
        assert!(matches!(
            parse_incoming(r),
            Ok(Incoming::Msg(m)) if matches!(*m, Out::Result { id: 2, .. })
        ));
        assert!(parse_incoming(br#"{"msg":"mystery"}"#).is_err());
        assert!(parse_incoming(b"{").is_err());
    }

    #[test]
    fn an_approval_ask_survives_the_wire_and_never_offers_bypass() {
        let rule = Suggestion::AddRules {
            rules: vec![RuleSpec {
                tool_name: "Bash".into(),
                rule_content: Some("ls".into()),
            }],
            behavior: Decision::Allow,
        };
        let ask = AskApproval {
            id: "a".into(),
            agent_id: None,
            tool: "Bash".into(),
            input: json!({"command": "ls"}),
            dialog: "permission",
            prompt: "p".into(),
            reason: "r".into(),
            origin: None,
            suggestions: vec![rule.clone(), Suggestion::SetMode(Mode::AcceptEdits)],
        };
        let line = serde_json::to_vec(&Out::from_ask(Ask::Approval(Box::new(ask)))).unwrap();
        let Ok(Incoming::Msg(m)) = parse_incoming(&line) else {
            panic!()
        };
        let back = (*m).into_approval().unwrap();
        assert_eq!(
            back.suggestions,
            vec![rule, Suggestion::SetMode(Mode::AcceptEdits)]
        );
        assert!(suggestion_from_json(&json!({"kind":"mode","mode":"bypassPermissions"})).is_none());
        for m in ["bypassPermissions", "auto", "dontAsk", "plan", "nonsense"] {
            let plan = Reply::Plan {
                approved: true,
                message: None,
                mode: Some(m.into()),
            };
            assert!(matches!(
                plan.into_user_reply(),
                UserReply::Plan { mode: None, .. }
            ));
        }
        let ok = Reply::Plan {
            approved: true,
            message: None,
            mode: Some("acceptEdits".into()),
        };
        assert!(matches!(
            ok.into_user_reply(),
            UserReply::Plan {
                mode: Some(Mode::AcceptEdits),
                ..
            }
        ));
    }

    #[test]
    fn lines_are_bounded() {
        let mut r = io::Cursor::new(b"a\r\nb\nlast".to_vec());
        assert_eq!(read_line(&mut r).unwrap().unwrap(), b"a");
        assert_eq!(read_line(&mut r).unwrap().unwrap(), b"b");
        assert_eq!(read_line(&mut r).unwrap().unwrap(), b"last");
        assert!(read_line(&mut r).unwrap().is_none());
        let mut long = io::Cursor::new(vec![b'x'; MAX_LINE + 1]);
        assert!(read_line(&mut long).is_err());
        let mut edge = vec![b'x'; MAX_LINE];
        edge.push(b'\n');
        assert_eq!(
            read_line(&mut io::Cursor::new(edge))
                .unwrap()
                .unwrap()
                .len(),
            MAX_LINE
        );
    }
}

#[cfg(test)]
mod hint_tests {
    use super::*;

    #[test]
    fn a_hint_round_trips_and_anything_looser_is_not_one() {
        let h = DecisionHint {
            outcome: "o1".into(),
            id: "d-1".into(),
            revision: 3,
            state: "pending".into(),
        };
        assert_eq!(DecisionHint::parse(&h.to_line()), Some(h.clone()));
        let mut extra = h.to_line();
        extra["brief"] = json!("approve me");
        let mut bad_state = h.to_line();
        bad_state["state"] = json!("approved; rm -rf");
        let mut bad_id = h.to_line();
        bad_id["id"] = json!("a b");
        let mut neg = h.to_line();
        neg["revision"] = json!(-1);
        for bad in [extra, bad_state, bad_id, neg, json!({"msg": "decision"})] {
            assert_eq!(DecisionHint::parse(&bad), None, "{bad}");
        }
    }
}
