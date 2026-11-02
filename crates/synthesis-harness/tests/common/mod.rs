#![allow(dead_code)]

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};
use synthesis_core::{
    AgentSession, Budget, BudgetUsage, PermissionMode, SessionStatus, ToolContext, WorkspaceContext,
};
use synthesis_events::{Envelope, Event};
use synthesis_harness::testing::{FakeProvider, SharedBuf, Turn};
use synthesis_harness::{
    Approval, ApprovalRequest, Approver, BoxFut, EventSink, Harness, HarnessConfig, TaskOutcome,
    TaskRequest, ToolBox,
};
use synthesis_permissions::{Env, Mode, PermissionRules, Permissions};
use synthesis_providers::{Block, Content, Message, Request, Role};
use synthesis_terminal::TerminalRuntime;
use synthesis_tools::{BoxFuture, SideEffectClass, Tool, ToolOutput, ToolRuntime, ToolSpec};
use tokio_util::sync::CancellationToken;

pub const SECRET: &str = "sk-test-TOPSECRETVALUE0123456789";

/// A scripted tool: sleeps, records concurrency, and returns fixed text.
pub struct Probe {
    spec: ToolSpec,
    pub delay_ms: u64,
    pub out: String,
    pub runs: AtomicUsize,
    active: AtomicUsize,
    pub max_active: AtomicUsize,
    pub modes: Mutex<Vec<PermissionMode>>,
}

impl Probe {
    pub fn new(name: &str, effect: SideEffectClass, delay_ms: u64, out: &str) -> Arc<Self> {
        Arc::new(Self {
            spec: ToolSpec {
                name: name.into(),
                description: "test tool".into(),
                input_schema: json!({"type": "object", "properties": {"x": {"type": "string"}}}),
                side_effect: effect,
                runtime: ToolRuntime::Builtin,
            },
            delay_ms,
            out: out.into(),
            runs: AtomicUsize::new(0),
            active: AtomicUsize::new(0),
            max_active: AtomicUsize::new(0),
            modes: Mutex::default(),
        })
    }

    pub fn runs(&self) -> usize {
        self.runs.load(Ordering::SeqCst)
    }
}

impl Tool for Probe {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    fn execute<'a>(&'a self, _input: Value, ctx: &'a ToolContext) -> BoxFuture<'a, ToolOutput> {
        Box::pin(async move {
            self.runs.fetch_add(1, Ordering::SeqCst);
            self.modes.lock().unwrap().push(ctx.permission_mode);
            let now = self.active.fetch_add(1, Ordering::SeqCst) + 1;
            self.max_active.fetch_max(now, Ordering::SeqCst);
            tokio::select! {
                () = tokio::time::sleep(Duration::from_millis(self.delay_ms)) => {}
                () = ctx.cancel.cancelled() => {}
            }
            self.active.fetch_sub(1, Ordering::SeqCst);
            ToolOutput::text(self.out.clone())
        })
    }
}

/// Answers approvals from a queue and records what it was asked.
pub struct ScriptedApprover {
    answers: Mutex<Vec<Approval>>,
    pub asked: Mutex<Vec<ApprovalRequest>>,
}

impl ScriptedApprover {
    pub fn new(answers: Vec<Approval>) -> Arc<Self> {
        Arc::new(Self {
            answers: Mutex::new(answers),
            asked: Mutex::default(),
        })
    }
}

impl Approver for ScriptedApprover {
    fn approve<'a>(
        &'a self,
        req: ApprovalRequest,
        _: &'a CancellationToken,
    ) -> BoxFut<'a, Approval> {
        self.asked.lock().unwrap().push(req);
        let a = {
            let mut q = self.answers.lock().unwrap();
            if q.is_empty() {
                Approval::deny("no more answers")
            } else {
                q.remove(0)
            }
        };
        Box::pin(async move { a })
    }
}

pub struct Opts {
    pub mode: PermissionMode,
    pub interactive: bool,
    pub allow: Vec<String>,
    pub deny: Vec<String>,
    pub budget: Budget,
    pub tools: ToolBox,
    pub config: fn(&mut HarnessConfig),
    pub secrets: Vec<String>,
    pub parallel_tools: bool,
    /// The event output starts failing after this many writes.
    pub fail_sink_after: Option<usize>,
    /// Model context window in tokens (`None` = unknown).
    pub context_window: Option<u64>,
    /// Picks the provider's answer from the request before the script is consulted.
    pub responder: Option<synthesis_harness::testing::Responder>,
    /// The model does not take images.
    pub no_vision: bool,
}

/// Writes into `buf` until `left` writes are used up, then fails every write.
pub struct FlakyWriter {
    pub buf: SharedBuf,
    pub left: usize,
}

impl std::io::Write for FlakyWriter {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        if self.left == 0 {
            return Err(std::io::Error::other("disk full"));
        }
        self.left -= 1;
        self.buf.write(b)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Default for Opts {
    fn default() -> Self {
        Self {
            mode: PermissionMode::Default,
            interactive: false,
            allow: vec![],
            deny: vec![],
            budget: Budget::default(),
            tools: ToolBox::empty(),
            config: |_| {},
            secrets: vec![SECRET.into()],
            parallel_tools: true,
            fail_sink_after: None,
            context_window: None,
            responder: None,
            no_vision: false,
        }
    }
}

pub struct Rig {
    pub dir: tempfile::TempDir,
    pub root: PathBuf,
    pub buf: SharedBuf,
    pub provider: Arc<FakeProvider>,
    pub harness: Harness,
    pub session: AgentSession,
    pub rt: Arc<TerminalRuntime>,
    pub ws: Arc<WorkspaceContext>,
}

impl Rig {
    pub fn new(o: Opts, turns: Vec<Turn>) -> Self {
        Self::build(o, turns, |h, _| h)
    }

    pub fn build(o: Opts, turns: Vec<Turn>, f: impl FnOnce(Harness, &Rig0) -> Harness) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = synthesis_core::canonicalize(dir.path()).unwrap();
        std::fs::create_dir_all(root.join("w")).unwrap();
        let ws = WorkspaceContext::open(
            "w",
            &root.join("w"),
            &root.join("w"),
            &[],
            root.join("art"),
            "pg",
        )
        .unwrap();
        let ws = Arc::new(ws);
        let session = AgentSession {
            id: "s1".into(),
            task_id: None,
            status: SessionStatus::Created,
            workspace: (*ws).clone(),
            model_config_id: "m".into(),
            permission_policy_id: "p".into(),
            permission_mode: o.mode,
            event_cursor: 0,
            budget: o.budget,
            usage: BudgetUsage::default(),
            created_at: String::new(),
            updated_at: String::new(),
            parent: None,
        };
        let rt = Arc::new(TerminalRuntime::new(synthesis_terminal::SandboxMode::Off));
        let env = BTreeMap::from([(
            "PATH".to_string(),
            std::env::var("PATH").unwrap_or_default(),
        )]);
        rt.open_session("s1", ws.clone(), env);

        let caps = {
            let mut c = synthesis_providers::ProviderCapabilities::defaults(
                synthesis_config::Protocol::Anthropic,
            );
            c.parallel_tools = o.parallel_tools;
            c.context_window = o.context_window;
            c.images = !o.no_vision;
            c
        };
        let mut fake = FakeProvider::new(turns).with_caps(caps);
        if let Some(r) = o.responder.clone() {
            fake = fake.with_responder(r);
        }
        let provider = Arc::new(fake);
        let buf = SharedBuf::default();
        let sink = match o.fail_sink_after {
            Some(left) => EventSink::new(
                FlakyWriter {
                    buf: buf.clone(),
                    left,
                },
                "s1",
                o.secrets.clone(),
            ),
            None => EventSink::new(buf.clone(), "s1", o.secrets.clone()),
        };
        let mut cfg = HarnessConfig::new("fake-model");
        cfg.recovery_delay = Duration::from_millis(1);
        (o.config)(&mut cfg);
        let rules = PermissionRules {
            allow: o.allow,
            deny: o.deny,
            ..Default::default()
        };
        let env =
            Env::new(root.join("w"), root.join("home"), Mode::Default).interactive(o.interactive);
        let perms = Permissions::new(rules, env);
        let harness = Harness::new(provider.clone(), o.tools, perms, sink, cfg);
        let h0 = Rig0 {
            rt: rt.clone(),
            ws: ws.clone(),
            root: root.clone(),
        };
        let harness = f(harness, &h0);
        Self {
            dir,
            root,
            buf,
            provider,
            harness,
            session,
            rt,
            ws,
        }
    }

    pub fn work(&self) -> PathBuf {
        self.root.join("w")
    }

    pub async fn run(&mut self, request: &str) -> TaskOutcome {
        self.run_with(TaskRequest::new(request), CancellationToken::new())
            .await
    }

    pub async fn run_with(&mut self, req: TaskRequest, cancel: CancellationToken) -> TaskOutcome {
        self.harness
            .start_session(&mut self.session, "startup")
            .unwrap();
        let out = self
            .harness
            .run_task(&mut self.session, req, cancel)
            .await
            .unwrap();
        self.harness.end_session(&out);
        out
    }

    pub fn events(&self) -> Vec<Envelope> {
        self.buf.envelopes()
    }

    pub fn types(&self) -> Vec<String> {
        self.events()
            .iter()
            .map(|e| {
                serde_json::to_value(e).unwrap()["type"]
                    .as_str()
                    .unwrap()
                    .to_string()
            })
            .collect()
    }

    pub fn find<T>(&self, f: impl Fn(&Event) -> Option<T>) -> Vec<T> {
        self.events().iter().filter_map(|e| f(&e.event)).collect()
    }

    /// `(from, to)` of every status change, in order.
    pub fn transitions(&self) -> Vec<(String, String)> {
        self.find(|e| match e {
            Event::SessionStatusChanged { from, to, .. } => Some((from.clone(), to.clone())),
            _ => None,
        })
    }

    pub fn requests(&self) -> Vec<Request> {
        self.provider.requests()
    }
}

/// What `Rig::build` hands to the customizer (the rig is not finished yet).
pub struct Rig0 {
    pub rt: Arc<TerminalRuntime>,
    pub ws: Arc<WorkspaceContext>,
    pub root: PathBuf,
}

/// Text of the `tool_result` block for `id` in a request's messages.
pub fn tool_result(req: &Request, id: &str) -> Option<(String, bool)> {
    for m in &req.messages {
        if let (Role::User, Content::Blocks(bs)) = (m.role, &m.content) {
            for b in bs {
                if let Block::ToolResult {
                    tool_use_id,
                    content,
                    is_error,
                } = b
                    && tool_use_id == id
                {
                    let text = match content {
                        Some(Value::String(s)) => s.clone(),
                        Some(v) => v.to_string(),
                        None => String::new(),
                    };
                    return Some((text, is_error.unwrap_or(false)));
                }
            }
        }
    }
    None
}

/// Structural checks every stream must satisfy (events.md §2, §3 ordering invariants).
pub fn check_stream(envs: &[Envelope]) {
    for (i, e) in envs.iter().enumerate() {
        assert_eq!(e.seq, i as u64 + 1, "seq must start at 1 with no gaps");
        assert_eq!(e.session_id, "s1");
        // JSONL round trip: re-serializing the parsed envelope equals what was parsed from.
        let v = serde_json::to_value(e).unwrap();
        let back: Envelope = serde_json::from_value(v.clone()).unwrap();
        assert_eq!(&back, e);
    }
    let pos = |pred: &dyn Fn(&Event) -> bool| envs.iter().position(|e| pred(&e.event));
    let ended = pos(&|e| matches!(e, Event::TaskEnded { .. })).expect("task.ended");
    let started = pos(&|e| matches!(e, Event::TaskStarted { .. })).expect("task.started");
    assert!(started < ended);
    if let Some(s) = pos(&|e| {
        matches!(
            e,
            Event::SessionCompleted(_) | Event::SessionFailed(_) | Event::SessionCancelled(_)
        )
    }) {
        assert!(ended < s, "task.ended precedes session.*");
        assert_eq!(s, envs.len() - 1, "session.* is the last event");
    }
    // every tool.requested has exactly one tool.completed after it
    for (i, e) in envs.iter().enumerate() {
        if let Event::ToolRequested { tool_call_id, .. } = &e.event {
            let done: Vec<usize> = envs
                .iter()
                .enumerate()
                .filter(|(_, d)| matches!(&d.event, Event::ToolCompleted { tool_call_id: t, .. } if t == tool_call_id))
                .map(|(j, _)| j)
                .collect();
            assert_eq!(done.len(), 1, "one tool.completed for {tool_call_id}");
            assert!(done[0] > i);
            // started (if any) sits between
            if let Some(s) = pos(
                &|x| matches!(x, Event::ToolStarted { tool_call_id: t, .. } if t == tool_call_id),
            ) {
                assert!(i < s && s < done[0]);
            }
        }
    }
}

pub fn user_text(m: &Message) -> String {
    match &m.content {
        Content::Text(t) => t.clone(),
        Content::Blocks(bs) => bs
            .iter()
            .filter_map(|b| {
                if let Block::Text { text } = b {
                    Some(text.as_str())
                } else {
                    None
                }
            })
            .collect(),
    }
}
