//! Messaging between the agents of a session: delivery at safe points, the SendMessage /
//! ListAgents tools, background sub-agents and the front-end entry points (messaging.md Part A).

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};

use futures_util::StreamExt;
use futures_util::stream::FuturesUnordered;
use serde_json::{Value, json};
use synthesis_events::Event;
use synthesis_hooks::HookEvent;
use synthesis_permissions::Mode;
use synthesis_providers::{Block, Content, Message, Role};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::agents::{AgentDef, stricter};
use crate::context::agent_message_block;
use crate::exec::Res;
use crate::harness::{AgentCtx, AgentEnd, AgentState, Harness, TaskRun};
use crate::hub::{
    CANCELLED, EVICTED, IDLE, Job, Kind, MAIN, MAX_BYTES, Msg, Origin, Posted, RUNNING, Retained,
    WAITING,
};
use crate::sink::Scope;
use crate::traits::BoxFut;
use crate::util::{lock, uuid};
use synthesis_events::ToolStatus;

/// What `ListAgents` and the front end's agent list show.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentInfo {
    pub name: String,
    pub kind: String,
    pub status: String,
    pub queued: usize,
}

/// The sub-agent a task is driven by instead of the main agent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TaskAgent {
    /// A retained (idle) instance.
    Existing(String),
    /// A new instance of this definition (`@agent:<definition> text`).
    New(String),
}

/// Result cap of a `notify_when_idle` reply.
const NOTIFY_BYTES: usize = 8 * 1024;

fn cut(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}\n[truncated]", &s[..end])
}

fn summary_of(v: Option<&str>) -> Option<String> {
    v.map(|s| s.trim().chars().take(120).collect::<String>())
        .filter(|s| !s.is_empty())
}

/// `(text, is_error, reason, status)` of a sub-agent's end, as the Task result shows it.
pub(crate) fn end_summary(end: AgentEnd) -> (String, bool, &'static str, ToolStatus) {
    match end {
        AgentEnd::Finished(t) if t.trim().is_empty() => (
            "(subagent finished with no text output)".to_string(),
            false,
            "finished",
            ToolStatus::Ok,
        ),
        AgentEnd::Finished(t) => (t, false, "finished", ToolStatus::Ok),
        AgentEnd::Cancelled => (
            "Interrupted.".into(),
            true,
            "cancelled",
            ToolStatus::Interrupted,
        ),
        AgentEnd::Budget(k) => (
            format!("Task stopped: budget exceeded ({k})."),
            true,
            "budget_exceeded",
            ToolStatus::BudgetExceeded,
        ),
        AgentEnd::NoProgress { .. } => (
            "Task stopped: no progress.".into(),
            true,
            "no_progress",
            ToolStatus::Error,
        ),
        AgentEnd::ModelError { reason, .. } => (
            format!("Task failed: {reason}"),
            true,
            "model_error",
            ToolStatus::Error,
        ),
        AgentEnd::UnsafeWorkspace(w) => (
            format!("Task stopped: unsafe workspace: {w}"),
            true,
            "unsafe_workspace",
            ToolStatus::Error,
        ),
        AgentEnd::Refusal => (
            "Task failed: the model refused.".into(),
            true,
            "refusal",
            ToolStatus::Error,
        ),
        AgentEnd::Internal(w) => (
            format!("Task failed: {w}"),
            true,
            "internal",
            ToolStatus::Error,
        ),
    }
}

/// Mode, permissions and tools of a child: never wider than the parent (stricter mode, copied
/// rules, subset of tools). SendMessage and ListAgents are always granted.
pub(crate) fn child_parts(
    h: &Harness,
    parent: &AgentCtx,
    def: &AgentDef,
) -> (
    Mode,
    Arc<Mutex<synthesis_permissions::Permissions>>,
    BTreeSet<String>,
) {
    let parent_mode = *lock(&parent.mode);
    let mode = stricter(parent_mode, def.mode.unwrap_or(parent_mode));
    let perms = {
        let mut p = lock(&parent.perms).clone();
        p.set_mode(mode);
        Arc::new(Mutex::new(p))
    };
    let mut allowed: BTreeSet<String> = match &def.tools {
        Some(t) => t.iter().cloned().collect(),
        None => h.tools.specs().into_iter().map(|s| s.name).collect(),
    };
    // Never delegable, even when the definition lists them: no nesting, no questions to the
    // user from a child.
    allowed.retain(|n| !matches!(n.as_str(), "Task" | "AskUserQuestion" | "ExitPlanMode"));
    allowed.extend(["SendMessage", "ListAgents", "ListSessions"].map(String::from));
    if let Some(pa) = &parent.allowed {
        allowed.retain(|n| pa.contains(n));
    }
    (mode, perms, allowed)
}

impl Harness {
    fn address(&self, task: &TaskRun, name: &str) -> String {
        let ws = &task.workspace;
        let key = if ws.workspace_key.is_empty() {
            &ws.workspace_id
        } else {
            &ws.workspace_key
        };
        format!("ws:{key}/sess:{}/agent:{name}", task.session_id)
    }

    fn scope(&self, task_id: Option<String>, agent_id: Option<String>) -> Scope {
        Scope {
            task_id,
            turn_id: None,
            agent_id,
        }
    }

    pub(crate) fn status_event(
        &self,
        task_id: Option<String>,
        agent_id: &str,
        name: &str,
        from: &str,
        to: &str,
        reason: Option<&str>,
    ) {
        self.sink.emit(
            &self.scope(task_id, Some(agent_id.into())),
            Event::SubagentStatusChanged {
                agent_id: agent_id.into(),
                name: name.into(),
                from: from.into(),
                to: to.into(),
                reason: reason.map(Into::into),
            },
        );
    }

    /// Moves an agent to `to` and emits the change (an approval wait, for instance).
    pub(crate) fn hub_status(&self, task: &TaskRun, agent: &AgentCtx, to: &'static str) {
        if agent.agent_id.is_some()
            && let Some((id, from)) = self.hub.set_status(&agent.name, to)
        {
            self.status_event(Some(task.task_id.clone()), &id, &agent.name, from, to, None);
        }
    }

    fn dropped(&self, task_id: Option<String>, m: &Msg, reason: &str) {
        self.sink.emit(
            &self.scope(task_id.clone(), None),
            Event::MessageDropped {
                message_id: m.id.clone(),
                reason: reason.into(),
            },
        );
        // The sender of an agent message learns that it did not arrive.
        if m.origin == Origin::Agent && m.from != m.to {
            self.post_system(
                task_id,
                &m.from,
                Kind::Notice,
                format!(
                    "Your message to @{} was not delivered ({reason}). Text: {}",
                    m.to,
                    cut(&m.text, 200)
                ),
            );
        }
    }

    /// A harness-authored message (result, notice). Never refused for a full inbox.
    fn post_system(&self, task_id: Option<String>, to: &str, kind: Kind, text: String) {
        self.post_from(task_id, "system", "system", to, Origin::System, kind, text);
    }

    #[allow(clippy::too_many_arguments)]
    fn post_from(
        &self,
        task_id: Option<String>,
        from: &str,
        from_kind: &str,
        to: &str,
        origin: Origin,
        kind: Kind,
        text: String,
    ) {
        let m = Msg {
            id: uuid(),
            from: from.into(),
            from_kind: from_kind.into(),
            to: to.into(),
            origin,
            kind,
            text,
            summary: None,
            notify_when_idle: false,
        };
        let (id, bytes, body) = (m.id.clone(), m.text.len() as u64, m.text.clone());
        let (f, t, o) = (m.from.clone(), m.to.clone(), m.origin);
        if let Err(e) = self.hub.post(m.clone(), Mode::Plan, true) {
            self.dropped(task_id, &m, &e.to_string());
        } else {
            self.sink.emit(
                &self.scope(task_id, None),
                Event::MessageSent {
                    message_id: id,
                    from: f,
                    to: t,
                    origin: o.as_str().into(),
                    kind: kind.as_str().into(),
                    summary: None,
                    text: cut(&body, 4096),
                    bytes,
                    notify_when_idle: false,
                },
            );
        }
    }

    /// Queues `m` and emits `message.sent` (and the wake-up of an idle agent). The error text
    /// goes back to the sender.
    fn post_msg(
        &self,
        task_id: Option<String>,
        agent_id: Option<String>,
        m: Msg,
        sender_mode: Mode,
    ) -> Result<bool, String> {
        let sent = Event::MessageSent {
            message_id: m.id.clone(),
            from: m.from.clone(),
            to: m.to.clone(),
            origin: m.origin.as_str().into(),
            kind: m.kind.as_str().into(),
            summary: m.summary.clone(),
            text: cut(&m.text, 4096),
            bytes: m.text.len() as u64,
            notify_when_idle: m.notify_when_idle,
        };
        let to = m.to.clone();
        match self.hub.post(m, sender_mode, false) {
            Err(e) => Err(e.to_string()),
            Ok(p) => {
                self.sink.emit(&self.scope(task_id.clone(), agent_id), sent);
                if let Posted::Woken(id, from) = p {
                    self.status_event(task_id, &id, &to, from, RUNNING, Some("message"));
                    return Ok(true);
                }
                Ok(false)
            }
        }
    }

    // ---- delivery --------------------------------------------------------------------------

    /// Drain point: moves the agent's inbox into its history as text blocks next to the tool
    /// results of the round that just ended. Returns whether anything was delivered.
    pub(crate) fn deliver(&self, task: &TaskRun, agent: &AgentCtx, st: &mut AgentState) -> bool {
        self.peer_snapshot(&agent.name, &st.messages);
        let msgs = self.hub.drain(&agent.name);
        let peer = self.peer_take(&agent.name);
        if msgs.is_empty() && peer.is_empty() {
            return false;
        }
        let at = std::mem::replace(&mut st.drain_at, "tool_round");
        let mut blocks = vec![];
        for m in &msgs {
            self.emit(
                task,
                agent,
                None,
                Event::MessageDelivered {
                    message_id: m.id.clone(),
                    at: at.into(),
                },
            );
            if m.origin == Origin::User {
                // The person's own words: a plain user turn.
                self.emit(
                    task,
                    agent,
                    None,
                    Event::UserMessage {
                        text: m.text.clone(),
                        display_text: None,
                        images: vec![],
                    },
                );
                blocks.push(Block::Text {
                    text: m.text.clone(),
                });
            } else {
                blocks.push(Block::Text {
                    text: agent_message_block(m, &self.address(task, &m.from)),
                });
            }
        }
        blocks.extend(peer.into_iter().map(|text| Block::Text { text }));
        match st.messages.last_mut() {
            Some(Message {
                role: Role::User,
                content,
            }) => match content {
                Content::Blocks(b) => b.extend(blocks),
                Content::Text(t) => {
                    let mut v = vec![Block::Text {
                        text: std::mem::take(t),
                    }];
                    v.extend(blocks);
                    *content = Content::Blocks(v);
                }
            },
            _ => st.messages.push(Message {
                role: Role::User,
                content: Content::Blocks(blocks),
            }),
        }
        true
    }

    /// The main agent's loop: a finished turn is not the end while mail is waiting or a
    /// sub-agent still runs (drain point 2). One gate and one budget for the whole task.
    pub(crate) async fn run_with_mail(
        &self,
        task: &TaskRun,
        main: &AgentCtx,
        st: &mut AgentState,
    ) -> AgentEnd {
        loop {
            let text = match self.run_agent(task, main, st).await {
                AgentEnd::Finished(t) => t,
                end => return end,
            };
            let mut changed = self.hub.subscribe();
            let mut peer_changed = self.peer_subscribe();
            loop {
                if self.hub.has_mail(&main.name) || self.peer_has_mail(&main.name) {
                    st.drain_at = "turn_end";
                    break;
                }
                if !self.hub.any_running(&main.name) && self.peer_inflight() == 0 {
                    return AgentEnd::Finished(text);
                }
                let wait = async {
                    match task.deadline {
                        Some(d) => {
                            tokio::time::sleep_until(tokio::time::Instant::from_std(d)).await
                        }
                        None => std::future::pending().await,
                    }
                };
                tokio::select! {
                    biased;
                    () = main.cancel.cancelled() => return AgentEnd::Cancelled,
                    () = wait => {
                        if let Some(k) = self.budget_hit(task, main) {
                            return AgentEnd::Budget(k);
                        }
                    }
                    _ = changed.changed() => {}
                    () = Self::peer_changed(&mut peer_changed) => {}
                }
            }
        }
    }

    /// Runs the task's future next to the background sub-agents it starts (they share the task's
    /// state by reference, so they are polled here and not spawned). When `main` ends, the ones
    /// still running are cancelled and awaited.
    pub(crate) async fn with_background<T>(
        &self,
        task: &TaskRun,
        rx: &mut mpsc::UnboundedReceiver<Job>,
        main: impl std::future::Future<Output = T>,
    ) -> T {
        let mut kids: FuturesUnordered<BoxFut<'_, ()>> = FuturesUnordered::new();
        tokio::pin!(main);
        let out = loop {
            tokio::select! {
                out = &mut main => break out,
                Some(job) = rx.recv() => kids.push(Box::pin(self.run_child(task, job))),
                Some(()) = kids.next(), if !kids.is_empty() => {}
            }
        };
        self.hub.cancel_all();
        while let Ok(job) = rx.try_recv() {
            kids.push(Box::pin(self.run_child(task, job)));
        }
        while kids.next().await.is_some() {}
        out
    }

    /// One background run of a sub-agent (first run or a resume).
    async fn run_child(&self, task: &TaskRun, job: Job) {
        let Job {
            name,
            agent_id,
            kind,
            parent,
            retained,
            cancel,
            first,
        } = job;
        let ctx = AgentCtx {
            agent_id: Some(agent_id),
            name,
            cancel,
            depth: retained.depth,
            mode: Mutex::new(retained.mode),
            perms: retained.perms.clone(),
            allowed: retained.allowed.clone(),
            prompt: retained.prompt.clone(),
        };
        let mut st = AgentState::new(retained.messages);
        if first.is_none() {
            st.drain_at = "wake";
        }
        let end = self.run_agent(task, &ctx, &mut st).await;
        let turn = first.as_ref().map(|f| f.0.as_str());
        let notify_parent = first.is_some().then_some(parent.as_str());
        self.conclude(
            task,
            &ctx,
            &kind,
            first.as_ref(),
            turn,
            true,
            notify_parent,
            st.messages,
            end,
        )
        .await;
    }

    /// A sub-agent's run is over: events, the SubagentStop hook, its conversation kept for a
    /// later message, and `result` messages to whoever waits for it.
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn conclude(
        &self,
        task: &TaskRun,
        ctx: &AgentCtx,
        kind: &str,
        first: Option<&(String, String)>,
        turn: Option<&str>,
        background: bool,
        notify_parent: Option<&str>,
        messages: Vec<Message>,
        end: AgentEnd,
    ) -> (String, bool, &'static str, ToolStatus) {
        let (text, is_error, reason, status) = end_summary(end);
        let agent_id = ctx.agent_id.clone().unwrap_or_default();
        if let Some((_, call_id)) = first {
            self.emit(
                task,
                ctx,
                turn,
                Event::SubagentStopped(synthesis_events::SubagentInfo {
                    agent_id: agent_id.clone(),
                    agent_type: kind.into(),
                    tool_call_id: Some(call_id.clone()),
                    reason: Some(reason.into()),
                    name: Some(ctx.name.clone()),
                    background,
                }),
            );
        }
        self.park(task, ctx, kind, messages, reason, &text, notify_parent);
        self.run_hook(
            task,
            ctx,
            turn,
            HookEvent::SubagentStop,
            json!({"agent_type": kind, "agent_id": agent_id, "stop_reason": reason}),
            Some(kind),
        )
        .await;
        (text, is_error, reason, status)
    }

    /// Keeps the agent's conversation (idle, or cancelled), drops what nobody read and sends the
    /// final text to the senders that asked and to `notify_parent`.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn park(
        &self,
        task: &TaskRun,
        ctx: &AgentCtx,
        kind: &str,
        messages: Vec<Message>,
        reason: &str,
        text: &str,
        notify_parent: Option<&str>,
    ) {
        self.peer_snapshot(&ctx.name, &messages);
        let to = if reason == "cancelled" {
            CANCELLED
        } else {
            IDLE
        };
        let retained = Retained {
            depth: ctx.depth,
            mode: *lock(&ctx.mode),
            perms: ctx.perms.clone(),
            allowed: ctx.allowed.clone(),
            prompt: ctx.prompt.clone(),
            messages,
        };
        let mut notify = self.hub.finish(&ctx.name, retained, to);
        let id = ctx.agent_id.clone().unwrap_or_default();
        self.status_event(
            Some(task.task_id.clone()),
            &id,
            &ctx.name,
            RUNNING,
            to,
            Some(reason),
        );
        if to == CANCELLED {
            self.peer_forget(&ctx.name);
            for m in self.hub.drain(&ctx.name) {
                self.dropped(Some(task.task_id.clone()), &m, "recipient cancelled");
            }
        }
        if let Some(p) = notify_parent
            && !notify.iter().any(|n| n == p)
        {
            notify.push(p.into());
        }
        for n in notify {
            let body = if reason == "finished" {
                cut(text, NOTIFY_BYTES.max(MAX_BYTES / 2))
            } else {
                format!("[{reason}] {}", cut(text, NOTIFY_BYTES))
            };
            self.post_from(
                Some(task.task_id.clone()),
                &ctx.name,
                kind,
                &n,
                Origin::Agent,
                Kind::Result,
                body,
            );
        }
    }

    /// The tail of `run_task`: undelivered mail of the agent the task ran as is dropped.
    pub(crate) fn drop_mail(&self, task: &TaskRun, name: &str) {
        for m in self.hub.drain(name) {
            self.dropped(Some(task.task_id.clone()), &m, "task ended");
        }
    }

    // ---- tools -----------------------------------------------------------------------------

    /// `SendMessage` and `ListAgents`.
    pub(crate) fn run_comm(
        &self,
        task: &TaskRun,
        agent: &AgentCtx,
        name: &str,
        input: &Value,
    ) -> Res {
        let _ = task;
        if name == "ListAgents" {
            let rows: Vec<Value> = self
                .hub
                .rows()
                .into_iter()
                .map(|r| json!({"name": r.name, "address": format!("agent:{}", r.name), "kind": r.kind, "status": r.status, "queued": r.queued, "you": r.name == agent.name}))
                .collect();
            return self.plain(Value::Array(rows).to_string(), false, ToolStatus::Ok);
        }
        let s = |k: &str| input.get(k).and_then(Value::as_str);
        let fail = |t: String| self.plain(t, true, ToolStatus::SoftError);
        let to = match self.hub.resolve(s("to").unwrap_or_default(), &agent.name) {
            Ok(n) => n,
            Err(e) => return fail(e),
        };
        if to == agent.name {
            return fail("You cannot send a message to yourself.".into());
        }
        let m = Msg {
            id: uuid(),
            from: agent.name.clone(),
            from_kind: self.hub.kind_of(&agent.name),
            to: to.clone(),
            origin: Origin::Agent,
            kind: Kind::Message,
            text: s("message").unwrap_or_default().to_string(),
            summary: summary_of(s("summary")),
            notify_when_idle: input
                .get("notify_when_idle")
                .and_then(Value::as_bool)
                .unwrap_or(false),
        };
        let id = m.id.clone();
        let mut mode = *lock(&agent.mode);
        // A sender that cannot mutate must not launder that through a recipient that can: the
        // woken recipient runs in `default` (mutating tools ask) unless already stricter.
        let mutating = ["Bash", "Write", "Edit", "MultiEdit", "NotebookEdit"];
        if agent
            .allowed
            .as_ref()
            .is_some_and(|a| !mutating.iter().any(|t| a.contains(*t)))
        {
            mode = stricter(mode, Mode::Default);
        }
        match self.post_msg(Some(task.task_id.clone()), agent.agent_id.clone(), m, mode) {
            Ok(woken) => self.plain(
                json!({"status": if woken { "resumed" } else { "queued" }, "to": to, "message_id": id})
                    .to_string(),
                false,
                ToolStatus::Ok,
            ),
            Err(e) => fail(e),
        }
    }

    // ---- entry points for front ends -------------------------------------------------------

    /// The session's agents (`main` first) for `/agents` and pickers.
    pub fn agents(&self) -> Vec<AgentInfo> {
        self.hub
            .rows()
            .into_iter()
            .map(|r| AgentInfo {
                name: r.name,
                kind: r.kind,
                status: r.status.into(),
                queued: r.queued,
            })
            .collect()
    }

    /// A line the person typed to `to` (`main` or an instance) while a task runs. It reaches the
    /// agent at its next safe point; an idle sub-agent starts again. Never an approval. It is a
    /// user prompt like any other: the `UserPromptSubmit` hook runs first and can reject it.
    pub async fn send_user_message(&self, to: &str, text: String) -> Result<(), String> {
        let task_id = self
            .hub
            .task_id()
            .ok_or_else(|| "no task is running".to_string())?;
        let to = self.hub.resolve(to, MAIN)?;
        let mode = lock(&self.permissions).mode();
        let mut text = text;
        if let Some(hooks) = &self.hooks {
            let payload =
                json!({"prompt": text, "permission_mode": mode.as_str(), "prompt_id": task_id});
            let out = hooks
                .run(
                    HookEvent::UserPromptSubmit,
                    payload,
                    None,
                    &CancellationToken::new(),
                )
                .await;
            if out.blocked {
                return Err(out
                    .reason
                    .unwrap_or_else(|| "blocked by a UserPromptSubmit hook".into()));
            }
            let extra = out.additional_context.join("\n");
            if !extra.is_empty() {
                text = format!("{text}\n\n{extra}");
            }
        }
        let m = Msg {
            id: uuid(),
            from: "user".into(),
            from_kind: "user".into(),
            to,
            origin: Origin::User,
            kind: Kind::Message,
            text,
            summary: None,
            notify_when_idle: false,
        };
        self.post_msg(Some(task_id), None, m, mode).map(|_| ())
    }

    /// Esc on a sub-agent: cancels that agent only. False when it is not running.
    pub fn cancel_agent(&self, name: &str) -> bool {
        self.hub.cancel(name)
    }

    /// `waiting_approval` for the hub while a dialog is open.
    pub(crate) fn waiting(&self, task: &TaskRun, agent: &AgentCtx, on: bool) {
        self.hub_status(task, agent, if on { WAITING } else { RUNNING });
    }

    /// A registered, running child of `parent` with the delegated prompt as its history.
    pub(crate) fn new_child(
        &self,
        task: &TaskRun,
        parent: &AgentCtx,
        def: &AgentDef,
        want: Option<&str>,
        prompt: String,
    ) -> Result<(AgentCtx, AgentState), String> {
        let (mode, perms, allowed) = child_parts(self, parent, def);
        let agent_id = uuid();
        let cancel: CancellationToken = parent.cancel.child_token();
        let (name, evicted) =
            self.hub
                .register(&def.name, want, &parent.name, &agent_id, cancel.clone())?;
        if let Some(e) = evicted {
            self.peer_forget(&e.name);
            for m in &e.lost {
                self.dropped(Some(task.task_id.clone()), m, "recipient was evicted");
            }
            self.status_event(
                Some(task.task_id.clone()),
                &e.agent_id,
                &e.name,
                IDLE,
                EVICTED,
                Some("too many agents"),
            );
        }
        let ctx = AgentCtx {
            agent_id: Some(agent_id),
            name,
            cancel,
            depth: parent.depth + 1,
            mode: Mutex::new(mode),
            perms,
            allowed: Some(allowed),
            prompt: Some(def.prompt.clone()),
        };
        let st = AgentState::new(vec![Message {
            role: Role::User,
            content: Content::Text(prompt),
        }]);
        Ok((ctx, st))
    }
}
