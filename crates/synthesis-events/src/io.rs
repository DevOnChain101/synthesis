use std::io::{self, BufRead, Write};

use regex::Regex;
use serde_json::Value;
use time::OffsetDateTime;

use crate::{Envelope, Event, RawEnvelope, SCHEMA_VERSION};

/// Masks secrets in string values. Built-in patterns follow spec §5.2; `secrets` are exact
/// values (e.g. env values of API keys) masked wherever they appear.
pub struct Redactor {
    secrets: Vec<String>,
    patterns: Vec<(&'static str, Regex)>,
}

impl Redactor {
    pub fn new(secrets: impl IntoIterator<Item = String>) -> Self {
        // Order matters: `sk-ant-` must run before the generic `sk-`.
        let defs: [(&str, &str); 6] = [
            ("sk-ant", r"sk-ant-[A-Za-z0-9_-]+"),
            ("sk", r"sk-[A-Za-z0-9_-]{20,}"),
            (
                "github",
                r"(?:gh[po]_[A-Za-z0-9]+|github_pat_[A-Za-z0-9_]+)",
            ),
            ("aws", r"AKIA[0-9A-Z]{16}"),
            (
                "private-key",
                r"(?s)-----BEGIN [A-Z ]*PRIVATE KEY-----.*?(?:-----END [A-Z ]*PRIVATE KEY-----|$)",
            ),
            ("bearer", r"Bearer [A-Za-z0-9._~+/=-]+"),
        ];
        Self {
            // Empty secrets would match everywhere.
            secrets: secrets.into_iter().filter(|s| !s.is_empty()).collect(),
            patterns: defs
                .iter()
                .map(|(k, p)| (*k, Regex::new(p).expect("static regex")))
                .collect(),
        }
    }

    pub fn redact_str(&self, s: &str) -> String {
        let mut out = s.to_string();
        for secret in &self.secrets {
            out = out.replace(secret.as_str(), "[REDACTED:secret]");
        }
        for (kind, re) in &self.patterns {
            out = re
                .replace_all(&out, format!("[REDACTED:{kind}]").as_str())
                .into_owned();
        }
        out
    }

    pub fn redact_value(&self, v: &mut Value) {
        match v {
            Value::String(s) => *s = self.redact_str(s),
            Value::Array(a) => a.iter_mut().for_each(|x| self.redact_value(x)),
            Value::Object(o) => o.values_mut().for_each(|x| self.redact_value(x)),
            _ => {}
        }
    }
}

/// Appends one JSON object per line with a per-session monotonic `seq` starting at 1.
pub struct EventWriter<W: Write> {
    out: W,
    session_id: String,
    seq: u64,
    redactor: Redactor,
    pub task_id: Option<String>,
    pub turn_id: Option<String>,
    pub agent_id: Option<String>,
}

impl<W: Write> EventWriter<W> {
    pub fn redactor(&self) -> &Redactor {
        &self.redactor
    }

    pub fn new(out: W, session_id: impl Into<String>, redactor: Redactor) -> Self {
        Self {
            out,
            session_id: session_id.into(),
            seq: 0,
            redactor,
            task_id: None,
            turn_id: None,
            agent_id: None,
        }
    }

    /// Stamps, redacts and writes `event`; returns the envelope as persisted (redacted).
    /// `seq` is consumed only when the write succeeds, so there are no gaps.
    pub fn emit(&mut self, event: Event) -> io::Result<Envelope> {
        let env = Envelope {
            v: SCHEMA_VERSION,
            seq: self.seq + 1,
            ts: now_rfc3339_ms(),
            session_id: self.session_id.clone(),
            task_id: self.task_id.clone(),
            turn_id: self.turn_id.clone(),
            agent_id: self.agent_id.clone(),
            event,
        };
        let mut value = serde_json::to_value(&env).map_err(io::Error::other)?;
        if let Some(p) = value.get_mut("payload") {
            self.redactor.redact_value(p);
        }
        // serde_json escapes newlines inside strings, so this is exactly one line.
        let mut line = serde_json::to_string(&value).map_err(io::Error::other)?;
        line.push('\n');
        self.out.write_all(line.as_bytes())?;
        self.out.flush()?;
        self.seq += 1;
        serde_json::from_value(value).map_err(io::Error::other)
    }

    pub fn into_inner(self) -> W {
        self.out
    }
}

fn now_rfc3339_ms() -> String {
    let fmt = time::macros::format_description!(
        "[year]-[month]-[day]T[hour]:[minute]:[second].[subsecond digits:3]Z"
    );
    OffsetDateTime::now_utc()
        .format(&fmt)
        .expect("format utc timestamp")
}

/// One line read back from a stream.
// An `Envelope` is read, applied and dropped; boxing it would only add an allocation per event.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq)]
pub enum Record {
    Known(Envelope),
    /// Valid envelope whose `type`/`payload` this build does not understand (spec §7).
    Unknown(RawEnvelope),
}

pub struct EventReader<R: BufRead> {
    lines: io::Lines<R>,
}

impl<R: BufRead> EventReader<R> {
    pub fn new(r: R) -> Self {
        Self { lines: r.lines() }
    }
}

impl<R: BufRead> Iterator for EventReader<R> {
    type Item = io::Result<Record>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            let line = match self.lines.next()? {
                Ok(l) => l,
                Err(e) => return Some(Err(e)),
            };
            if line.trim().is_empty() {
                continue;
            }
            return Some(parse_line(&line));
        }
    }
}

fn parse_line(line: &str) -> io::Result<Record> {
    let bad = |e: serde_json::Error| io::Error::new(io::ErrorKind::InvalidData, e);
    let value: Value = serde_json::from_str(line).map_err(bad)?;
    match serde_json::from_value::<Envelope>(value.clone()) {
        Ok(env) => Ok(Record::Known(env)),
        Err(_) => serde_json::from_value(value)
            .map(Record::Unknown)
            .map_err(bad),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::*;
    use serde_json::json;

    fn aref() -> ArtifactRef {
        ArtifactRef {
            artifact_id: "a".into(),
            path: "artifacts/a.txt".into(),
            size: 3,
            sha256: "00".into(),
            mime: "text/plain".into(),
        }
    }
    fn usage() -> Usage {
        Usage {
            input_tokens: 1,
            output_tokens: 2,
            cache_read_input_tokens: Some(3),
            cache_creation_input_tokens: None,
            cost_usd: Some(0.5),
        }
    }
    fn proc() -> ProcessInfo {
        ProcessInfo {
            process_id: "p".into(),
            name: Some("n".into()),
            pid: Some(1),
            exit_code: None,
            signal: None,
            tool_call_id: Some("t".into()),
            command: Some("ls".into()),
            duration_ms: Some(3),
        }
    }
    fn sub() -> SubagentInfo {
        SubagentInfo {
            agent_id: "a".into(),
            agent_type: "t".into(),
            tool_call_id: None,
            reason: Some("r".into()),
            name: Some("n".into()),
            background: true,
        }
    }
    fn peer() -> PeerAddr {
        PeerAddr {
            workspace_key: "k".into(),
            workspace_label: "api".into(),
            workspace_root: Some("/r".into()),
            session_id: "s".into(),
            session_name: None,
            agent_id: None,
            instance: "main".into(),
            kind: "main".into(),
        }
    }
    fn end() -> SessionEnd {
        SessionEnd {
            reason: "r".into(),
            error: None,
        }
    }
    fn s(x: &str) -> String {
        x.into()
    }

    fn all_events() -> Vec<Event> {
        vec![
            Event::SessionStarted {
                workspace: WorkspaceInfo {
                    workspace_id: s("w"),
                    workspace_key: Some(s("k0123456789a")),
                    root: s("/r"),
                    cwd: s("/r"),
                    additional_dirs: vec![s("/x")],
                    git_worktree: None,
                },
                model: s("m"),
                provider: Some(s("p")),
                permission_mode: s("default"),
                source: s("startup"),
                resumed_from_seq: Some(4),
            },
            Event::BrowserLoginRequired {
                origin: s("https://app.example.com"),
                profile: s("app-example-com"),
            },
            Event::SessionStatusChanged {
                from: s("idle"),
                to: s("running"),
                reason: None,
            },
            Event::SessionCompleted(end()),
            Event::SessionFailed(SessionEnd {
                reason: s("r"),
                error: Some(s("e")),
            }),
            Event::SessionCancelled(end()),
            Event::TaskStarted {
                request: s("do"),
                acceptance_criteria: vec![Criterion {
                    id: s("1"),
                    text: s("t"),
                    check: Some(s("c")),
                }],
                budget: Budget::default(),
            },
            Event::TaskEnded {
                status: TaskStatus::Unverified,
                reason: s("r"),
                verification_report_id: Some(s("v")),
                usage: usage(),
                changed_files: vec![s("a.rs")],
                artifacts_root: s("art"),
                needed_permissions: vec![s("Bash(cargo test:*)")],
            },
            Event::UserMessage {
                text: s("hi"),
                display_text: None,
                images: vec![aref()],
            },
            Event::TurnStarted {
                model: s("m"),
                provider: s("p"),
                iteration: 1,
            },
            Event::ModelDelta {
                block_index: 0,
                kind: s("text"),
                text: s("t"),
            },
            Event::AssistantMessage {
                message_id: Some(s("m")),
                content: vec![json!({"type":"text","text":"x"})],
                stop_reason: s("end_turn"),
                usage: Some(usage()),
            },
            Event::TurnCompleted {
                stop_reason: s("tool_use"),
                usage: None,
                duration_ms: 5,
            },
            Event::ModelRetry {
                attempt: 1,
                max: 3,
                error: s("e"),
                delay_ms: 10,
            },
            Event::ModelError {
                status: Some(429),
                message: s("m"),
                retryable: true,
                context_overflow: false,
            },
            Event::ContextCompacted {
                trigger: s("auto"),
                summary_ref: aref(),
                tokens_before: Some(9),
                reason: s("overflow"),
                before_tokens: 9,
                after_tokens: 4,
                window_tokens: 100,
                messages_before: 7,
                messages_after: 2,
                summary_chars: 30,
                duration_ms: 12,
            },
            Event::ContextUsage {
                used_tokens: 42,
                window_tokens: 100,
                percent: 42.0,
                source: s("provider"),
                stale: false,
            },
            Event::ContextCompacting {
                reason: s("auto"),
                used_tokens: 90,
                window_tokens: 100,
            },
            Event::ContextCompactionFailed {
                reason: s("manual"),
                error: s("summarizer failed: x"),
            },
            Event::ToolRequested {
                tool_call_id: s("t"),
                name: s("Bash"),
                input: json!({"cmd":"ls"}),
                input_truncated: false,
                origin: Some(s("user")),
            },
            Event::ToolApprovalRequired {
                approval_id: s("ap"),
                tool_call_id: s("t"),
                dialog: s("permission"),
                prompt: s("ok?"),
                options: vec![s("yes")],
                suggestions: vec![json!({})],
                deadline: None,
            },
            Event::ToolApprovalResolved {
                approval_id: s("ap"),
                decision: s("allow"),
                reviewer: s("user"),
                input_changed: false,
                message: None,
            },
            Event::ToolStarted {
                tool_call_id: s("t"),
                name: s("Bash"),
                side_effect: s("execute"),
                cwd: s("/r"),
            },
            Event::ToolCompleted {
                tool_call_id: s("t"),
                name: s("Bash"),
                status: ToolStatus::SoftError,
                is_error: true,
                not_run: false,
                duration_ms: 1,
                preview: s("p"),
                bytes: 1,
                truncated: true,
                result_ref: Some(aref()),
                origin: Some(s("user")),
                output: Some(s("full text")),
                output_truncated: true,
                output_bytes: 9,
                diff: None,
            },
            Event::ToolOutput {
                tool_call_id: s("t"),
                stream: s("merged"),
                text: s("line\n"),
                seq: 1,
                origin: None,
            },
            Event::HookRan {
                event: s("e"),
                hook_name: s("h"),
                exit_code: Some(0),
                outcome: s("success"),
                duration_ms: 1,
            },
            Event::ProcessSpawned(proc()),
            Event::ProcessReady(proc()),
            Event::ProcessExited(proc()),
            Event::ProcessOutput {
                process_id: s("p"),
                tool_call_id: s("t"),
                text: s("hi\n"),
                seq: 1,
            },
            Event::BrowserFrame {
                path: s("/a/screenshot-1.png"),
                url: s("https://example.com"),
                w: 1280,
                h: 720,
                bytes: 1234,
            },
            Event::BrowserAction {
                action: s("click"),
                x: Some(640),
                y: Some(360),
                w: Some(1280),
                h: Some(720),
                target: Some(s("button \"Send\"")),
                frame: None,
            },
            Event::BrowserAction {
                action: s("navigate"),
                x: None,
                y: None,
                w: None,
                h: None,
                target: None,
                frame: Some(s("/a/screenshot-1.png")),
            },
            Event::ArtifactCreated {
                artifact: aref(),
                kind: s("log"),
                retention: s("session"),
            },
            Event::VerificationCompleted {
                report_id: s("r"),
                status: s("passed"),
                attempt: 1,
                summary: s("ok"),
            },
            Event::TodoUpdated {
                todos: vec![Todo {
                    id: None,
                    content: s("c"),
                    status: s("pending"),
                    active_form: s("a"),
                }],
            },
            Event::SubagentStarted(sub()),
            Event::SubagentStopped(sub()),
            Event::SubagentStatusChanged {
                agent_id: s("a"),
                name: s("n"),
                from: s("running"),
                to: s("idle"),
                reason: None,
            },
            Event::MessageSent {
                message_id: s("m"),
                from: s("main"),
                to: s("w"),
                origin: s("agent"),
                kind: s("message"),
                summary: Some(s("hi")),
                text: s("hello"),
                bytes: 5,
                notify_when_idle: true,
            },
            Event::MessageDelivered {
                message_id: s("m"),
                at: s("tool_round"),
            },
            Event::MessageDropped {
                message_id: s("m"),
                reason: s("cancelled"),
            },
            Event::PeerListening {
                endpoint_kind: s("unix"),
            },
            Event::PeerStopped {
                endpoint_kind: s("pipe"),
            },
            Event::PeerRequestSent {
                request_id: s("r"),
                from: peer(),
                to: peer(),
                summary: s("q"),
                bytes: 3,
                chain_len: 1,
            },
            Event::PeerRequestReceived {
                request_id: s("r"),
                from: peer(),
                to: peer(),
                summary: s("q"),
                bytes: 3,
                decision: s("rejected"),
                reason: Some(s("denied")),
            },
            Event::PeerReplyPartial {
                request_id: s("r"),
                seq: 1,
                text: s("t"),
            },
            Event::PeerReply {
                request_id: s("r"),
                status: s("answered"),
                text: s("t"),
                bytes: 1,
                duration_ms: 5,
            },
            Event::PeerThreadClosed {
                thread_id: s("t"),
                reason: s("cap"),
            },
            Event::PeerDelegationAccepted {
                delegation_id: s("d"),
                request_id: s("r"),
                thread_id: Some(s("t")),
                from: PeerAddr {
                    workspace_key: s("k"),
                    workspace_label: s("l"),
                    workspace_root: None,
                    session_id: s("s"),
                    session_name: None,
                    agent_id: None,
                    instance: s("main"),
                    kind: s("main"),
                },
                mode: s("default"),
                cap: s("plan"),
            },
            Event::PeerDelegationStarted {
                delegation_id: s("d"),
                agent_id: s("a"),
            },
            Event::PeerDelegationTool {
                delegation_id: s("d"),
                tool_call_id: s("c"),
                name: s("Bash"),
                origin: s("peer"),
                from: s("ws:k/sess:s/agent:main"),
            },
            Event::PeerDelegationCompleted {
                delegation_id: s("d"),
                status: s("answered"),
                tool_calls: 2,
                duration_ms: 5,
            },
            Event::PeerDelegationDenied {
                request_id: s("r"),
                from: PeerAddr {
                    workspace_key: s("k"),
                    workspace_label: s("l"),
                    workspace_root: None,
                    session_id: s("s"),
                    session_name: None,
                    agent_id: None,
                    instance: s("main"),
                    kind: s("main"),
                },
                reason: s("delegation_denied"),
            },
            Event::SessionModeChanged {
                from: s("default"),
                to: s("plan"),
                reason: Some(s("user")),
            },
            Event::AutoVerdict {
                tool_call_id: s("t"),
                name: s("Bash"),
                verdict: s("allow"),
                reason: s("r"),
                source: s("model"),
            },
            Event::ModelChanged {
                model: s("m"),
                effort: Some(s("high")),
            },
            Event::BudgetExceeded {
                budget: s("turns"),
                limit: 50.0,
                used: 51.0,
            },
            Event::MemoryRetrieved {
                ids: vec![s("m-1")],
                tokens: 12,
                candidates: 3,
            },
            Event::MemoryWritten {
                id: s("m-1"),
                op: s("added"),
                kind: s("fact"),
                scope: s("workspace"),
                trust: s("agent"),
                status: s("active"),
            },
            Event::MemoryQuarantined {
                id: s("m-1"),
                rule: s("fence_tag"),
            },
            Event::MemoryReflectionStarted { job_id: s("j") },
            Event::MemoryReflectionCompleted {
                job_id: s("j"),
                ops: 2,
                tokens: Some(10),
                cost_usd: Some(0.01),
                estimated: true,
            },
            Event::MemoryReflectionFailed {
                job_id: s("j"),
                error: s("timed out"),
            },
            Event::MemoryProposalCreated { id: s("p-1") },
            Event::Notice {
                level: s("info"),
                text: s("t"),
            },
        ]
    }

    fn writer() -> EventWriter<Vec<u8>> {
        EventWriter::new(Vec::new(), "sess", Redactor::new([]))
    }
    fn read(buf: &[u8]) -> Vec<Record> {
        EventReader::new(buf).map(|r| r.unwrap()).collect()
    }

    #[test]
    fn round_trip_every_variant_with_monotonic_seq() {
        let events = all_events();
        let mut w = writer();
        w.task_id = Some("task".into());
        let written: Vec<Envelope> = events.iter().cloned().map(|e| w.emit(e).unwrap()).collect();
        let buf = w.into_inner();
        let back = read(&buf);
        assert_eq!(back.len(), events.len());
        for (i, (rec, ev)) in back.iter().zip(&events).enumerate() {
            let Record::Known(env) = rec else {
                panic!("variant {i} read as unknown")
            };
            assert_eq!(&env.event, ev);
            assert_eq!(env.seq, i as u64 + 1);
            assert_eq!(env.v, 1);
            assert_eq!(env.task_id.as_deref(), Some("task"));
            assert_eq!(env, &written[i]);
        }
        assert_eq!(buf.iter().filter(|&&b| b == b'\n').count(), events.len());
    }

    #[test]
    fn envelope_wire_shape() {
        let mut w = writer();
        w.emit(Event::Notice {
            level: s("info"),
            text: s("t"),
        })
        .unwrap();
        let v: Value = serde_json::from_slice(&w.into_inner()).unwrap();
        assert_eq!(v["type"], "notice");
        assert_eq!(v["payload"], json!({"level":"info","text":"t"}));
        assert!(v["task_id"].is_null() && v["agent_id"].is_null());
        let ts = v["ts"].as_str().unwrap();
        assert!(ts.ends_with('Z') && ts.len() == 24, "{ts}");
    }

    #[test]
    fn browser_login_required_wire_shape() {
        let mut w = writer();
        w.emit(Event::BrowserLoginRequired {
            origin: s("https://app.example.com"),
            profile: s("acme"),
        })
        .unwrap();
        let v: Value = serde_json::from_slice(&w.into_inner()).unwrap();
        assert_eq!(v["type"], "browser.login_required");
        assert_eq!(
            v["payload"],
            json!({"origin":"https://app.example.com","profile":"acme"})
        );
    }

    #[test]
    fn unknown_type_and_fields_tolerated() {
        let base = r#""v":1,"seq":1,"ts":"t","session_id":"s","task_id":null,"turn_id":null"#;
        let input = format!(
            "{{{base},\"type\":\"future.thing\",\"payload\":{{\"a\":1}}}}\n\
             {{{base},\"extra\":true,\"type\":\"notice\",\"payload\":{{\"level\":\"info\",\"text\":\"t\",\"new\":1}}}}\n"
        );
        let recs = read(input.as_bytes());
        assert!(matches!(&recs[0], Record::Unknown(r) if r.kind == "future.thing"));
        assert!(matches!(&recs[1], Record::Known(e) if matches!(e.event, Event::Notice { .. })));
    }

    #[test]
    fn context_events_wire_shape_and_old_compacted_still_reads() {
        let base = r#""v":1,"seq":1,"ts":"t","session_id":"s","task_id":null,"turn_id":null"#;
        let mut w = writer();
        w.emit(Event::ContextUsage {
            used_tokens: 5,
            window_tokens: 10,
            percent: 50.0,
            source: s("estimate"),
            stale: true,
        })
        .unwrap();
        let v: Value = serde_json::from_slice(&w.into_inner()).unwrap();
        assert_eq!(v["type"], "context.usage");
        assert_eq!(
            v["payload"],
            json!({"used_tokens":5,"window_tokens":10,"percent":50.0,"source":"estimate","stale":true})
        );
        // Written before the new fields existed, and with a field from the future.
        let old = format!(
            "{{{base},\"type\":\"context.compacted\",\"payload\":{{\"trigger\":\"auto\",\"summary_ref\":{{\"artifact_id\":\"a\",\"path\":\"p\",\"size\":1,\"sha256\":\"0\",\"mime\":\"text/plain\"}},\"future\":1}}}}\n"
        );
        let Record::Known(env) = &read(old.as_bytes())[0] else {
            panic!("old context.compacted must stay readable")
        };
        assert!(matches!(
            &env.event,
            Event::ContextCompacted { reason, before_tokens: 0, duration_ms: 0, .. } if reason.is_empty()
        ));
    }

    #[test]
    fn tool_output_fields_are_additive_and_redacted() {
        let mut w = writer();
        w.emit(Event::ToolOutput {
            tool_call_id: s("t"),
            stream: s("merged"),
            text: s("token ghp_abcdef123456\n"),
            seq: 2,
            origin: Some(s("user")),
        })
        .unwrap();
        let v: Value = serde_json::from_slice(&w.into_inner()).unwrap();
        assert_eq!(v["type"], "tool.output");
        assert_eq!(v["payload"]["text"], "token [REDACTED:github]\n");
        assert_eq!(v["payload"]["origin"], "user");
        // a v1 tool.completed written before `output` existed still reads, with the defaults
        let old = r#"{"v":1,"seq":1,"ts":"t","session_id":"s","task_id":null,"turn_id":null,"type":"tool.completed","payload":{"tool_call_id":"t","name":"n","status":"ok","is_error":false,"not_run":false,"duration_ms":0,"preview":"p","bytes":1,"truncated":false}}"#;
        assert!(matches!(
            &read(old.as_bytes())[0],
            Record::Known(Envelope {
                event: Event::ToolCompleted {
                    origin: None,
                    output: None,
                    output_truncated: false,
                    output_bytes: 0,
                    ..
                },
                ..
            })
        ));
    }

    #[test]
    fn unknown_enum_value_is_other() {
        let line = r#"{"v":1,"seq":1,"ts":"t","session_id":"s","task_id":null,"turn_id":null,"type":"tool.completed","payload":{"tool_call_id":"t","name":"n","status":"brand_new","is_error":false,"not_run":false,"duration_ms":0,"preview":"","bytes":0,"truncated":false}}"#;
        let recs = read(line.as_bytes());
        assert!(matches!(
            &recs[0],
            Record::Known(Envelope {
                event: Event::ToolCompleted {
                    status: ToolStatus::Other,
                    ..
                },
                ..
            })
        ));
    }

    #[test]
    fn newline_in_payload_is_escaped() {
        let mut w = writer();
        w.emit(Event::Notice {
            level: s("info"),
            text: s("a\nb\r\nc"),
        })
        .unwrap();
        let buf = w.into_inner();
        assert_eq!(buf.iter().filter(|&&b| b == b'\n').count(), 1);
        let Record::Known(env) = &read(&buf)[0] else {
            panic!()
        };
        assert!(matches!(&env.event, Event::Notice { text, .. } if text == "a\nb\r\nc"));
    }

    #[test]
    fn redaction_masks_configured_and_builtin_secrets() {
        let mut w = EventWriter::new(Vec::new(), "s", Redactor::new([s("hunter2-secret"), s("")]));
        w.emit(Event::ToolRequested {
            tool_call_id: s("t"),
            name: s("Bash"),
            input: json!({"cmd": "login hunter2-secret now", "h": ["Bearer abc.def", "sk-ant-api03-XYZ"], "n": 7}),
            input_truncated: false,
            origin: None,
        })
        .unwrap();
        let text = String::from_utf8(w.into_inner()).unwrap();
        assert!(
            !text.contains("hunter2") && !text.contains("abc.def") && !text.contains("XYZ"),
            "{text}"
        );
        assert!(text.contains("login [REDACTED:secret] now"));
        assert!(text.contains("[REDACTED:bearer]") && text.contains("[REDACTED:sk-ant]"));
        assert!(text.contains("\"n\":7"));
    }

    #[test]
    fn redactor_builtin_kinds() {
        let r = Redactor::new([]);
        for (input, kind) in [
            ("ghp_abcdef123456", "github"),
            ("AKIAABCDEFGHIJKLMNOP", "aws"),
            ("sk-abcdefghijklmnopqrstuv", "sk"),
            (
                "-----BEGIN RSA PRIVATE KEY-----\nxx\n-----END RSA PRIVATE KEY-----",
                "private-key",
            ),
        ] {
            assert_eq!(
                r.redact_str(&format!("a {input} b")),
                format!("a [REDACTED:{kind}] b")
            );
        }
        assert_eq!(r.redact_str("sk-short"), "sk-short");
    }

    #[test]
    fn tool_completed_diff_is_additive_round_trips_and_is_redacted() {
        let done = |diff: Option<Value>| Event::ToolCompleted {
            tool_call_id: s("t"),
            name: s("Edit"),
            status: ToolStatus::Ok,
            is_error: false,
            not_run: false,
            duration_ms: 1,
            preview: s("p"),
            bytes: 1,
            truncated: false,
            result_ref: None,
            origin: None,
            output: None,
            output_truncated: false,
            output_bytes: 0,
            diff,
        };
        let mut w = writer();
        w.emit(done(None)).unwrap();
        let v: Value = serde_json::from_slice(&w.into_inner()).unwrap();
        assert!(v["payload"].get("diff").is_none(), "absent, not null");

        let d = json!({"path": "a.rs", "kind": "update", "added": 1, "removed": 0, "truncated": false,
            "hunks": [{"old_start": 1, "old_lines": 0, "new_start": 1, "new_lines": 1,
                "lines": [{"tag": "+", "new_no": 1, "text": "token ghp_abcdef123456"}]}]});
        let mut w = writer();
        w.emit(done(Some(d.clone()))).unwrap();
        let bytes = w.into_inner();
        let v: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(
            v["payload"]["diff"]["hunks"][0]["lines"][0]["text"],
            "token [REDACTED:github]"
        );
        assert_eq!(v["payload"]["diff"]["added"], 1);
        let Record::Known(env) = &read(&bytes)[0] else {
            panic!("diff event must read back")
        };
        assert!(
            matches!(&env.event, Event::ToolCompleted { diff: Some(x), .. } if x["path"] == "a.rs")
        );
    }
}
