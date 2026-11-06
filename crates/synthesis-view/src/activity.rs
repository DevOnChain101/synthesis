//! The Activity feed and the Tools & Skills panels, as typed rows over a [`Session`].

use serde::Serialize;

use crate::model::{FeedItem, Session};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub enum ActivityKind {
    /// What the person typed.
    Prompt,
    /// What the assistant answered (one line).
    Text,
    /// tool / hook / skill / agent / cmd rows of the full log.
    Tool,
    Hook,
    Skill,
    Agent,
    Command,
    Other,
}

impl ActivityKind {
    fn of(kind: &str) -> Self {
        match kind {
            "prompt" => Self::Prompt,
            "text" => Self::Text,
            "tool" => Self::Tool,
            "hook" => Self::Hook,
            "skill" => Self::Skill,
            "agent" => Self::Agent,
            "cmd" => Self::Command,
            _ => Self::Other,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ActivityRow {
    pub ts: Option<i64>,
    pub kind: ActivityKind,
    pub text: String,
    /// A prompt row: index of the conversation item that holds it (set by the host).
    pub anchor: Option<usize>,
}

fn rows(v: &[FeedItem]) -> Vec<ActivityRow> {
    v.iter()
        .map(|f| ActivityRow {
            ts: f.ts,
            kind: ActivityKind::of(f.kind),
            text: f.text.clone(),
            anchor: f.anchor,
        })
        .collect()
}

/// `rows`: the Activity panel (prompts and answers, oldest first); `log`: everything that
/// happened, tools and hooks included.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct ActivityModel {
    pub rows: Vec<ActivityRow>,
    pub log: Vec<ActivityRow>,
}

impl ActivityModel {
    pub fn from_session(s: &Session) -> Self {
        Self {
            rows: rows(&s.activity),
            log: rows(&s.feed),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct RunningTool {
    pub id: String,
    pub name: String,
    pub summary: String,
    pub start: Option<i64>,
}

/// Tool calls by name (most used first), skills by use, and the tools running now.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct ToolsSkillsModel {
    pub tools: Vec<(String, u32)>,
    pub skills: Vec<(String, u32)>,
    pub running: Vec<RunningTool>,
    pub errors: u32,
}

impl ToolsSkillsModel {
    pub fn from_session(s: &Session) -> Self {
        let mut tools = s.tool_counts.clone();
        tools.sort_by_key(|e| std::cmp::Reverse(e.1)); // stable: ties keep first-use order
        Self {
            tools,
            skills: s.skills.clone(),
            running: s
                .tools
                .values()
                .filter(|t| t.end.is_none())
                .map(|t| RunningTool {
                    id: t.id.clone(),
                    name: t.name.clone(),
                    summary: t.summary.clone(),
                    start: t.start,
                })
                .collect(),
            errors: s.errors,
        }
    }
}
