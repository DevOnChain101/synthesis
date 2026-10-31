//! The Activity page: one row per notable event, filtered All / Agents / Tools / Events.

use serde_json::Value;
use synthesis_events::{Envelope, Event};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Filter {
    All,
    Agents,
    Tools,
    Events,
}

impl Filter {
    pub const ALL: [Filter; 4] = [Filter::All, Filter::Agents, Filter::Tools, Filter::Events];

    pub fn label(self) -> &'static str {
        match self {
            Self::All => "All",
            Self::Agents => "Agents",
            Self::Tools => "Tools",
            Self::Events => "Events",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    pub time: String,
    pub who: String,
    pub text: String,
    pub kind: Filter,
}

/// `HH:MM:SS` of an RFC 3339 timestamp (empty when it is not one).
fn clock(ts: &str) -> String {
    ts.split('T')
        .nth(1)
        .map(|t| t.chars().take(8).collect())
        .unwrap_or_default()
}

fn kind_of(e: &Envelope, name: &str) -> Filter {
    if e.agent_id.is_some() || name.starts_with("subagent.") || name.starts_with("peer.") {
        Filter::Agents
    } else if name.starts_with("tool.") || name.starts_with("process.") {
        Filter::Tools
    } else {
        Filter::Events
    }
}

/// `None` for chatter that would drown the page (stream deltas, usage ticks).
pub fn row(e: &Envelope) -> Option<Row> {
    let name = serde_json::to_value(&e.event)
        .ok()?
        .get("type")?
        .as_str()?
        .to_string();
    let text = match &e.event {
        Event::ModelDelta { .. }
        | Event::ContextUsage { .. }
        | Event::ToolStarted { .. }
        | Event::AssistantMessage { .. } => return None,
        Event::ToolRequested { name, input, .. } => {
            format!("{name} {}", crate::convo::summarize_input(name, input))
        }
        Event::ToolCompleted {
            name,
            status,
            duration_ms,
            ..
        } => format!("{name} {status:?} in {duration_ms} ms").to_lowercase(),
        Event::ToolApprovalRequired { prompt, .. } => format!("needs approval: {prompt}"),
        Event::ToolApprovalResolved { decision, .. } => format!("approval {decision}"),
        Event::UserMessage { text, .. } => format!("you: {}", text.lines().next().unwrap_or("")),
        Event::Notice { text, .. } | Event::ModelError { message: text, .. } => text.clone(),
        _ => name.replace(['.', '_'], " "),
    };
    Some(Row {
        time: clock(&e.ts),
        who: e.agent_id.clone().unwrap_or_else(|| "synthesis".into()),
        text: text.chars().take(160).collect(),
        kind: kind_of(e, &name),
    })
}

pub fn rows(events: &[Envelope], f: Filter) -> Vec<Row> {
    let mut v: Vec<Row> = events
        .iter()
        .filter_map(row)
        .filter(|r| f == Filter::All || r.kind == f)
        .collect();
    v.reverse(); // newest first
    v
}

/// The `type` of an event as a plain string (used to tell hooks/subagents apart on the topology).
pub fn type_of(e: &Envelope) -> Option<String> {
    serde_json::to_value(&e.event)
        .ok()?
        .get("type")
        .and_then(Value::as_str)
        .map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn ev(t: &str, agent: Option<&str>, payload: Value) -> Envelope {
        let mut v = json!({"v":1,"seq":1,"ts":"2026-01-01T09:08:07Z","session_id":"s","task_id":null,"turn_id":null,"type":t});
        v["payload"] = payload;
        let mut e: Envelope = serde_json::from_value(v).unwrap();
        e.agent_id = agent.map(str::to_string);
        e
    }

    #[test]
    fn filters_split_agents_tools_and_events_and_drop_deltas() {
        let events = [
            ev("notice", None, json!({"level":"info","text":"hello"})),
            ev(
                "model.delta",
                None,
                json!({"block_index":0,"kind":"text","text":"a"}),
            ),
            ev(
                "tool.requested",
                None,
                json!({"tool_call_id":"t","name":"Bash","input":{"command":"ls"},"input_truncated":false}),
            ),
            ev(
                "tool.requested",
                Some("developer"),
                json!({"tool_call_id":"u","name":"Read","input":{"file_path":"a.rs"},"input_truncated":false}),
            ),
        ];
        assert_eq!(rows(&events, Filter::All).len(), 3);
        assert_eq!(rows(&events, Filter::Tools).len(), 1);
        assert_eq!(rows(&events, Filter::Agents)[0].who, "developer");
        assert_eq!(rows(&events, Filter::Events).len(), 1);
        assert_eq!(rows(&events, Filter::All)[0].time, "09:08:07");
    }
}
