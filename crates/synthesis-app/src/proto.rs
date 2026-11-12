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
use synthesis_tools::NeedsUser;

use crate::asks::{Ask, AskApproval, Choice};

/// Bumped when a message changes shape. The version handshake is exact: both numbers must match.
pub const PROTOCOL: u32 = 2;
/// The engine's own version (one workspace version for the app and the engine).
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
/// Longest accepted line (a tool result can carry a big file).
pub const MAX_LINE: usize = 8 << 20;

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
    /// `{started}`: false when it ran between tasks (`{msg:"compacted"}` follows).
    Compact {
        instructions: Option<String>,
    },
    Clear,
    Cancel,
    #[serde(alias = "shutdown")]
    Close,
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
        assert!(r.id == 3 && matches!(r.call, Call::Send { ref text } if text == "hi"));
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
