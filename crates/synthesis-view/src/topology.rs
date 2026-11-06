//! State of the Topology panel: node states (dispatch / working / returning / failed / done), the
//! stage pipeline Plan -> Guard -> Orch -> Exec -> Verify -> Learn, and per-node counters, derived
//! from a [`Session`] (itself built from `Event`s). The TUI draws it as a character canvas
//! (`synthesis-tui-render::flow`); a GUI can draw the same [`Topology`] its own way.

use std::collections::BTreeSet;

use serde::Serialize;

use crate::model::{AgentRec, Component, Phase, Session, Tool, session_phase};
use crate::text::{basename, one_line};

const DISPATCH_MS: i64 = 1800;
const RETURN_HOLD_MS: i64 = 6000;
const COMP_DISPATCH_MS: i64 = 600;
const COMP_HOLD_MS: i64 = 2500;
/// A guard hook that blocked a call this recently raises the alert.
const BLOCK_ALERT_MS: i64 = 10_000;
/// The prompt node flashes this long after a prompt.
const PROMPT_FLASH_MS: i64 = 5000;
/// Guard hooks count as the Guard stage this long after they ended.
const GUARD_STAGE_MS: i64 = 900;
/// Without a todo list, or this soon after a prompt, the core is still reading the request.
const INTENT_MS: i64 = 15_000;

#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize)]
pub enum St {
    Dispatch,
    Working,
    Returning,
    Failed,
    Done,
}
pub fn is_active(s: St) -> bool {
    matches!(s, St::Dispatch | St::Working | St::Returning)
}
pub fn is_busy(s: St) -> bool {
    matches!(s, St::Dispatch | St::Working)
}

pub fn agent_state(a: &AgentRec, now: i64) -> St {
    if a.status == "running" {
        return if now - a.start.unwrap_or(0) < DISPATCH_MS {
            St::Dispatch
        } else {
            St::Working
        };
    }
    if let Some(e) = a.end.filter(|e| *e != 0)
        && now - e < RETURN_HOLD_MS
    {
        return St::Returning;
    }
    if a.status == "failed" {
        St::Failed
    } else {
        St::Done
    }
}

pub fn component_state(c: &Component, now: i64) -> St {
    if c.open > 0 {
        return if now - c.last_start.unwrap_or(0) < COMP_DISPATCH_MS {
            St::Dispatch
        } else {
            St::Working
        };
    }
    if let Some(e) = c.last_end.filter(|e| *e != 0)
        && now - e < COMP_HOLD_MS
    {
        return St::Returning;
    }
    if c.error { St::Failed } else { St::Done }
}

pub fn is_guard(c: &Component) -> bool {
    let s = c
        .event
        .as_deref()
        .filter(|e| !e.is_empty())
        .unwrap_or(&c.name);
    ["PreToolUse", "UserPromptSubmit", "PermissionRequest"]
        .iter()
        .any(|p| {
            s.strip_prefix(p).is_some_and(|rest| {
                !rest.starts_with(|ch: char| ch.is_ascii_alphanumeric() || ch == '_')
            })
        })
}

pub struct PhaseState<'a> {
    /// Key of the stage running now (`plan`, `guard`, `orchestrate`, a tool category).
    pub current: Option<String>,
    /// Stages already passed since the last prompt.
    pub seen: BTreeSet<String>,
    pub current_tool: Option<&'a Tool>,
}

pub fn phase_state(s: &Session, now: i64) -> PhaseState<'_> {
    let since = s.last_prompt_at.unwrap_or(0);
    let mut seen = BTreeSet::new();
    let mut current_tool: Option<&Tool> = None;
    for t in s.tools.values() {
        if t.start.unwrap_or(0) >= since {
            seen.insert(t.cat.to_string());
        }
        if t.end.is_none()
            && current_tool.is_none_or(|c| t.start.unwrap_or(0) > c.start.unwrap_or(0))
        {
            current_tool = Some(t);
        }
    }
    let mut last_guard = 0;
    for c in s.components.values() {
        let Some(le) = c.last_end.filter(|e| *e != 0) else {
            continue;
        };
        if c.kind != "hook" || !is_guard(c) {
            continue;
        }
        if le >= since {
            seen.insert("guard".into());
        }
        last_guard = last_guard.max(le);
    }
    let agents_busy = s.agents.values().any(|a| a.status == "running");
    let current = if last_guard != 0 && now - last_guard < GUARD_STAGE_MS {
        Some("guard".to_string())
    } else if let Some(t) = current_tool {
        Some(t.cat.to_string())
    } else if agents_busy {
        Some("orchestrate".into())
    } else if session_phase(s, now).phase == Phase::Thinking {
        Some("plan".into())
    } else {
        None
    };
    if let Some(c) = &current {
        seen.remove(c);
    }
    PhaseState {
        current,
        seen,
        current_tool,
    }
}

/// A stage of the pipeline.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize)]
pub enum Stage {
    Plan,
    Guard,
    Orchestrate,
    Execute,
    Verify,
    Learn,
}

impl Stage {
    pub const ALL: [Stage; 6] = [
        Stage::Plan,
        Stage::Guard,
        Stage::Orchestrate,
        Stage::Execute,
        Stage::Verify,
        Stage::Learn,
    ];

    /// The key `phase_state` uses (and tool categories use).
    pub fn key(self) -> &'static str {
        match self {
            Stage::Plan => "plan",
            Stage::Guard => "guard",
            Stage::Orchestrate => "orchestrate",
            Stage::Execute => "execute",
            Stage::Verify => "verify",
            Stage::Learn => "learn",
        }
    }

    pub fn from_key(k: &str) -> Option<Stage> {
        Self::ALL.into_iter().find(|s| s.key() == k)
    }

    /// Short label of the pipeline bar.
    pub fn label(self) -> &'static str {
        match self {
            Stage::Plan => "Plan",
            Stage::Guard => "Guard",
            Stage::Orchestrate => "Orch",
            Stage::Execute => "Exec",
            Stage::Verify => "Verify",
            Stage::Learn => "Learn",
        }
    }
}

/// One agent, skill, hook, MCP server or tool in a group.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct TopoItem {
    pub id: String,
    pub name: String,
    pub kind: String,
    pub state: St,
    /// Calls (components) or tools used (agents).
    pub count: u32,
    pub error: bool,
}

/// The nodes of one kind (guards, skills, sub-agents, hooks, MCP, execution), busiest first.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct Group {
    pub items: Vec<TopoItem>,
    pub busy: usize,
    /// Calls blocked by a guard / tool calls made.
    pub blocked: u32,
    pub calls: u32,
    /// A guard blocked a call within the last 10 seconds.
    pub alert: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct UserNode {
    pub prompts: u32,
    pub last_prompt: String,
    pub last_prompt_at: Option<i64>,
    pub project: String,
    pub git_branch: String,
    /// A prompt arrived a moment ago.
    pub busy: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct CoreNode {
    pub model: String,
    /// Intent / Plan / Orchestrator rows: which one is working.
    pub intent_on: bool,
    pub plan_on: bool,
    pub orchestrator_on: bool,
    pub agents_busy: usize,
    pub skills_busy: usize,
    pub tasks_done: usize,
    pub tasks_total: usize,
    /// The todo being worked on.
    pub doing: Option<String>,
    pub tokens: u64,
    /// `None` when no window is known (never guessed).
    pub context_pct: Option<f64>,
    pub busy: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct VerifyInfo {
    pub ok: bool,
    pub summary: String,
    pub ts: Option<i64>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct MemoryNode {
    pub verify: Option<VerifyInfo>,
    pub memory_writes: u32,
    /// Verify / learn tool running.
    pub busy: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct Topology {
    /// The stage running now; `None` when idle.
    pub stage: Option<Stage>,
    /// Stages passed since the last prompt.
    pub stages_seen: Vec<Stage>,
    pub user: UserNode,
    pub core: CoreNode,
    pub guards: Group,
    pub skills: Group,
    pub agents: Group,
    pub hooks: Group,
    pub mcp: Group,
    pub exec: Group,
    pub memory: MemoryNode,
    pub errors: u32,
}

fn item_of_agent(a: &AgentRec, now: i64) -> (TopoItem, i64) {
    let last = a
        .start
        .unwrap_or(0)
        .max(a.end.unwrap_or(0))
        .max(a.last_activity.unwrap_or(0));
    (
        TopoItem {
            id: format!("agent:{}", a.tool_use_id),
            name: a.kind.clone(),
            kind: "agent".into(),
            state: agent_state(a, now),
            count: a.tool_count,
            error: a.status == "failed",
        },
        last,
    )
}

fn item_of_comp(c: &Component, now: i64) -> (TopoItem, i64) {
    (
        TopoItem {
            id: c.key.clone(),
            name: c.name.clone(),
            kind: c.kind.clone(),
            state: component_state(c, now),
            count: c.count,
            error: c.error,
        },
        c.last_start.unwrap_or(0).max(c.last_end.unwrap_or(0)),
    )
}

/// Busy first, then active, then the most recent, then by id.
fn group(mut v: Vec<(TopoItem, i64)>) -> Group {
    let w = |i: &TopoItem| match i.state {
        s if is_busy(s) => 0,
        s if is_active(s) => 1,
        _ => 2,
    };
    v.sort_by(|(a, la), (b, lb)| w(a).cmp(&w(b)).then(lb.cmp(la)).then(a.id.cmp(&b.id)));
    let busy = v.iter().filter(|(i, _)| is_busy(i.state)).count();
    Group {
        items: v.into_iter().map(|(i, _)| i).collect(),
        busy,
        ..Group::default()
    }
}

impl Topology {
    /// `context_pct`: the context usage in percent when the host knows the window.
    pub fn from_session(s: &Session, now: i64, context_pct: Option<f64>) -> Self {
        let ps = phase_state(s, now);
        let stage = ps.current.as_deref().and_then(Stage::from_key);
        let on = |k: Stage| stage == Some(k);
        let comps = |pred: &dyn Fn(&Component) -> bool| -> Vec<(TopoItem, i64)> {
            s.components
                .values()
                .filter(|c| pred(c))
                .map(|c| item_of_comp(c, now))
                .collect()
        };
        let hook = |c: &Component| c.kind == "hook";

        let guard_comps: Vec<&Component> = s
            .components
            .values()
            .filter(|c| hook(c) && is_guard(c))
            .collect();
        let mut guards = group(comps(&|c| hook(c) && is_guard(c)));
        guards.blocked = guard_comps.iter().map(|c| c.blocked).sum();
        guards.alert = guard_comps
            .iter()
            .filter_map(|c| c.last_blocked_at.filter(|b| *b != 0))
            .max()
            .is_some_and(|b| now - b < BLOCK_ALERT_MS);
        let skills = group(comps(&|c| c.kind == "skill" || c.kind == "cmd"));
        let hooks = group(comps(&|c| hook(c) && !is_guard(c)));
        let mcp = group(comps(&|c| c.kind == "mcp"));
        let mut exec = group(comps(&|c| c.kind == "tool"));
        exec.calls = exec.items.iter().map(|i| i.count).sum();
        let agents = group(s.agents.values().map(|a| item_of_agent(a, now)).collect());

        let done = s.todos.iter().filter(|t| t.status == "completed").count();
        let doing = s.todos.iter().find(|t| t.status == "in_progress").map(|d| {
            one_line(if d.active_form.is_empty() {
                &d.content
            } else {
                &d.active_form
            })
        });
        let intent_on = on(Stage::Plan)
            && (s.todos.is_empty() || now - s.last_prompt_at.unwrap_or(0) < INTENT_MS);
        let plan_on = on(Stage::Plan) && !intent_on;
        let (agents_busy, skills_busy) = (agents.busy, skills.busy);
        let project = [basename(&s.cwd), s.project.clone()]
            .into_iter()
            .find(|p| !p.is_empty())
            .unwrap_or_default();

        Topology {
            stage,
            stages_seen: ps.seen.iter().filter_map(|k| Stage::from_key(k)).collect(),
            user: UserNode {
                prompts: s.prompts,
                last_prompt: one_line(&s.last_prompt),
                last_prompt_at: s.last_prompt_at.filter(|p| *p != 0),
                project,
                git_branch: s.git_branch.clone(),
                busy: s
                    .last_prompt_at
                    .is_some_and(|p| p != 0 && now - p < PROMPT_FLASH_MS),
            },
            core: CoreNode {
                model: s
                    .model
                    .strip_prefix("claude-")
                    .unwrap_or(&s.model)
                    .to_string(),
                intent_on,
                plan_on,
                orchestrator_on: on(Stage::Orchestrate) || agents_busy > 0,
                agents_busy,
                skills_busy,
                tasks_done: done,
                tasks_total: s.todos.len(),
                doing,
                tokens: s.tokens.total(),
                context_pct: context_pct.map(|p| p.clamp(0.0, 100.0)),
                busy: stage.is_some(),
            },
            guards,
            skills,
            agents,
            hooks,
            mcp,
            exec,
            memory: MemoryNode {
                verify: s.last_verify.as_ref().map(|v| VerifyInfo {
                    ok: v.ok,
                    summary: one_line(if v.summary.is_empty() {
                        &v.name
                    } else {
                        &v.summary
                    }),
                    ts: v.ts,
                }),
                memory_writes: s.memory_writes,
                busy: ps
                    .current_tool
                    .is_some_and(|t| t.cat == "verify" || t.cat == "learn"),
            },
            errors: s.errors,
        }
    }

    /// Nodes in total and how many are busy (the panel title).
    pub fn counts(&self) -> (usize, usize) {
        let gs = [
            &self.guards,
            &self.skills,
            &self.agents,
            &self.hooks,
            &self.mcp,
            &self.exec,
        ];
        (
            gs.iter().map(|g| g.busy).sum(),
            gs.iter().map(|g| g.items.len()).sum(),
        )
    }
}
