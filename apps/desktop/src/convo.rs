//! The conversation of one session as the window shows it: a reducer over the typed event stream
//! (`synthesis-events`), the same events `synt run` writes as JSONL. Pure state, no GPUI.

use serde_json::Value;
use synthesis_events::{Envelope, Event, ToolStatus};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolState {
    Running,
    Ok,
    Failed,
    NotRun,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApprovalState {
    Pending,
    Allowed,
    Denied,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Item {
    User(String),
    Assistant {
        text: String,
        streaming: bool,
    },
    Tool {
        id: String,
        name: String,
        summary: String,
        state: ToolState,
        preview: String,
        /// The file a Write/Edit-style tool changes (an evidence card with its diff).
        file: Option<String>,
    },
    /// A verification report: `status` is `passed`, `failed`, ... and `summary` one check per line.
    Verification {
        status: String,
        summary: String,
    },
    Approval {
        id: String,
        tool_call_id: String,
        prompt: String,
        state: ApprovalState,
    },
    Notice {
        warning: bool,
        text: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StepState {
    Done,
    Active,
    Todo,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Step {
    pub text: String,
    pub state: StepState,
}

#[derive(Debug, Default, Clone)]
pub struct Convo {
    pub items: Vec<Item>,
    /// A task is between `task.started` and `task.ended`.
    pub running: bool,
    pub tokens: u64,
    pub cost_usd: f64,
    /// The mission plan: the agent's todo list, latest version.
    pub plan: Vec<Step>,
    /// Sub-agents that started and have not stopped (`agent_type`).
    pub agents: Vec<(String, String)>,
}

/// The file a Write/Edit-style tool call changes.
pub fn changed_file(name: &str, input: &Value) -> Option<String> {
    matches!(name, "Write" | "Edit" | "MultiEdit" | "NotebookEdit")
        .then(|| {
            input
                .get("file_path")
                .or_else(|| input.get("notebook_path"))
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .flatten()
}

/// One line for a tool row: the command, the path, or the start of the JSON input.
pub fn summarize_input(name: &str, input: &Value) -> String {
    let s = |k: &str| input.get(k).and_then(Value::as_str);
    let text = match name {
        "Bash" => s("command"),
        _ => s("file_path")
            .or_else(|| s("path"))
            .or_else(|| s("pattern")),
    }
    .map_or_else(|| input.to_string(), str::to_string);
    text.lines()
        .next()
        .unwrap_or("")
        .chars()
        .take(120)
        .collect()
}

fn text_blocks(content: &[Value]) -> String {
    content
        .iter()
        .filter(|b| b.get("type").and_then(Value::as_str) == Some("text"))
        .filter_map(|b| b.get("text").and_then(Value::as_str))
        .collect::<Vec<_>>()
        .join("\n")
}

impl Convo {
    pub fn from_events(events: &[Envelope]) -> Self {
        let mut c = Self::default();
        for e in events {
            c.apply(e);
        }
        c
    }

    /// What the agent is doing right now, for the activity line (None when no task runs).
    pub fn activity(&self) -> Option<String> {
        if !self.running {
            return None;
        }
        if let Some((_, t)) = self.agents.last() {
            return Some(format!("{t} agent is working"));
        }
        let tool = self.items.iter().rev().find_map(|i| match i {
            Item::Tool {
                state: ToolState::Running,
                name,
                summary,
                ..
            } => Some(format!("Synthesis is running {name}  {summary}")),
            _ => None,
        });
        Some(tool.unwrap_or_else(|| "Synthesis is thinking".into()))
    }

    /// Approvals waiting for the person: the "needs you" count.
    pub fn needs_you(&self) -> usize {
        self.items
            .iter()
            .filter(|i| {
                matches!(
                    i,
                    Item::Approval {
                        state: ApprovalState::Pending,
                        ..
                    }
                )
            })
            .count()
    }

    fn close_stream(&mut self) {
        if let Some(Item::Assistant { streaming, .. }) = self.items.last_mut() {
            *streaming = false;
        }
    }

    fn tool(&mut self, id: &str) -> Option<&mut Item> {
        self.items
            .iter_mut()
            .rev()
            .find(|i| matches!(i, Item::Tool { id: t, .. } if t == id))
    }

    pub fn apply(&mut self, e: &Envelope) {
        // sub-agent chatter is shown on the Activity page, not in the main conversation
        let main = e.agent_id.is_none();
        match &e.event {
            Event::TaskStarted { .. } => self.running = true,
            Event::TaskEnded { usage, .. } => {
                self.running = false;
                self.close_stream();
                self.tokens = self.tokens.max(usage.input_tokens + usage.output_tokens);
                self.cost_usd = self.cost_usd.max(usage.cost_usd.unwrap_or(0.0));
            }
            Event::UserMessage {
                text, display_text, ..
            } if main => self.items.push(Item::User(
                display_text.clone().unwrap_or_else(|| text.clone()),
            )),
            Event::ModelDelta { kind, text, .. } if main && kind == "text" => {
                match self.items.last_mut() {
                    Some(Item::Assistant {
                        text: t,
                        streaming: true,
                    }) => t.push_str(text),
                    _ => self.items.push(Item::Assistant {
                        text: text.clone(),
                        streaming: true,
                    }),
                }
            }
            Event::AssistantMessage { content, .. } if main => {
                let full = text_blocks(content);
                match self.items.last_mut() {
                    Some(Item::Assistant { text, streaming }) if *streaming => {
                        *text = full;
                        *streaming = false;
                    }
                    _ if !full.is_empty() => self.items.push(Item::Assistant {
                        text: full,
                        streaming: false,
                    }),
                    _ => {}
                }
            }
            Event::TurnCompleted { usage: Some(u), .. } => {
                self.tokens += u.input_tokens + u.output_tokens;
                self.cost_usd += u.cost_usd.unwrap_or(0.0);
            }
            Event::TodoUpdated { todos } => {
                let first_open = todos.iter().position(|t| t.status != "completed");
                self.plan = todos
                    .iter()
                    .enumerate()
                    .map(|(i, t)| Step {
                        text: if t.status == "in_progress" && !t.active_form.is_empty() {
                            t.active_form.clone()
                        } else {
                            t.content.clone()
                        },
                        state: if t.status == "completed" {
                            StepState::Done
                        } else if t.status == "in_progress"
                            || (Some(i) == first_open && self.running)
                        {
                            StepState::Active
                        } else {
                            StepState::Todo
                        },
                    })
                    .collect();
            }
            Event::SubagentStarted(i) => {
                self.agents.push((i.agent_id.clone(), i.agent_type.clone()))
            }
            Event::SubagentStopped(i) => self.agents.retain(|(id, _)| *id != i.agent_id),
            Event::VerificationCompleted {
                status, summary, ..
            } if main => self.items.push(Item::Verification {
                status: status.clone(),
                summary: summary.clone(),
            }),
            Event::ToolRequested {
                tool_call_id,
                name,
                input,
                ..
            } if main => {
                self.close_stream();
                self.items.push(Item::Tool {
                    id: tool_call_id.clone(),
                    name: name.clone(),
                    summary: summarize_input(name, input),
                    state: ToolState::Running,
                    preview: String::new(),
                    file: changed_file(name, input),
                });
            }
            Event::ToolCompleted {
                tool_call_id,
                status,
                not_run,
                preview,
                ..
            } => {
                if let Some(Item::Tool {
                    state, preview: p, ..
                }) = self.tool(tool_call_id)
                {
                    *state = if *not_run {
                        ToolState::NotRun
                    } else if *status == ToolStatus::Ok {
                        ToolState::Ok
                    } else {
                        ToolState::Failed
                    };
                    p.clone_from(preview);
                }
            }
            Event::ToolApprovalRequired {
                approval_id,
                tool_call_id,
                prompt,
                ..
            } => self.items.push(Item::Approval {
                id: approval_id.clone(),
                tool_call_id: tool_call_id.clone(),
                prompt: prompt.clone(),
                state: ApprovalState::Pending,
            }),
            Event::ToolApprovalResolved {
                approval_id,
                decision,
                ..
            } => {
                for i in &mut self.items {
                    if let Item::Approval { id, state, .. } = i
                        && id == approval_id
                    {
                        *state = if decision == "allow" {
                            ApprovalState::Allowed
                        } else {
                            ApprovalState::Denied
                        };
                    }
                }
            }
            Event::ModelError { message, .. } => self.items.push(Item::Notice {
                warning: true,
                text: message.clone(),
            }),
            Event::Notice { level, text } => self.items.push(Item::Notice {
                warning: level != "info",
                text: text.clone(),
            }),
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn ev(t: &str, payload: Value) -> Envelope {
        let mut v = json!({"v":1,"seq":1,"ts":"2026-01-01T00:00:00Z","session_id":"s","task_id":null,"turn_id":null,"type":t});
        v["payload"] = payload;
        serde_json::from_value(v).unwrap()
    }

    #[test]
    fn deltas_stream_into_one_assistant_item_and_the_final_message_replaces_them() {
        let mut c = Convo::default();
        for t in ["Hel", "lo"] {
            c.apply(&ev(
                "model.delta",
                json!({"block_index":0,"kind":"text","text":t}),
            ));
        }
        assert_eq!(
            c.items,
            vec![Item::Assistant {
                text: "Hello".into(),
                streaming: true
            }]
        );
        c.apply(&ev(
            "assistant.message",
            json!({"content":[{"type":"text","text":"Hello!"}],"stop_reason":"end_turn"}),
        ));
        assert_eq!(
            c.items,
            vec![Item::Assistant {
                text: "Hello!".into(),
                streaming: false
            }]
        );
    }

    #[test]
    fn tool_rows_follow_requested_then_completed() {
        let mut c = Convo::default();
        c.apply(&ev(
            "tool.requested",
            json!({"tool_call_id":"t1","name":"Bash","input":{"command":"ls -la\nmore"},"input_truncated":false}),
        ));
        assert!(
            matches!(&c.items[0], Item::Tool { summary, state: ToolState::Running, .. } if summary == "ls -la")
        );
        c.apply(&ev(
            "tool.completed",
            json!({"tool_call_id":"t1","name":"Bash","status":"ok","is_error":false,"not_run":false,"duration_ms":3,"preview":"done","bytes":4,"truncated":false}),
        ));
        assert!(
            matches!(&c.items[0], Item::Tool { state: ToolState::Ok, preview, .. } if preview == "done")
        );
    }

    #[test]
    fn approval_flow_pending_then_resolved_updates_needs_you() {
        let mut c = Convo::default();
        c.apply(&ev(
            "tool.approval_required",
            json!({"approval_id":"a1","tool_call_id":"t1","dialog":"permission","prompt":"Run ls?","options":[],"suggestions":[]}),
        ));
        assert_eq!(c.needs_you(), 1);
        c.apply(&ev(
            "tool.approval_resolved",
            json!({"approval_id":"a1","decision":"deny","reviewer":"user","input_changed":false}),
        ));
        assert_eq!(c.needs_you(), 0);
        assert!(matches!(
            &c.items[0],
            Item::Approval {
                state: ApprovalState::Denied,
                ..
            }
        ));
    }

    #[test]
    fn plan_agents_cost_verification_and_changed_files_feed_the_mission_view() {
        let mut c = Convo {
            running: true,
            ..Default::default()
        };
        c.apply(&ev(
            "todo.updated",
            json!({"todos":[
                {"content":"Read","status":"completed","active_form":"Reading"},
                {"content":"Write","status":"in_progress","active_form":"Writing"},
                {"content":"Test","status":"pending","active_form":"Testing"}]}),
        ));
        let s: Vec<_> = c.plan.iter().map(|s| (s.text.as_str(), s.state)).collect();
        assert_eq!(
            s,
            [
                ("Read", StepState::Done),
                ("Writing", StepState::Active),
                ("Test", StepState::Todo)
            ]
        );
        c.apply(&ev(
            "subagent.started",
            json!({"agent_id":"a1","agent_type":"developer"}),
        ));
        assert_eq!(c.agents, [("a1".to_string(), "developer".to_string())]);
        c.apply(&ev(
            "subagent.stopped",
            json!({"agent_id":"a1","agent_type":"developer"}),
        ));
        assert!(c.agents.is_empty());
        c.apply(&ev(
            "verification.completed",
            json!({"report_id":"r","status":"passed","attempt":1,"summary":"tests ok"}),
        ));
        assert!(matches!(&c.items[0], Item::Verification { status, .. } if status == "passed"));
        assert_eq!(
            changed_file("Write", &json!({"file_path":"/a/b.rs"})).as_deref(),
            Some("/a/b.rs")
        );
        assert_eq!(changed_file("Bash", &json!({"command":"ls"})), None);
        assert_eq!(c.activity().as_deref(), Some("Synthesis is thinking"));
        c.apply(&ev(
            "tool.requested",
            json!({"tool_call_id":"t9","name":"Bash","input":{"command":"ls"},"input_truncated":false}),
        ));
        assert_eq!(
            c.activity().as_deref(),
            Some("Synthesis is running Bash  ls")
        );
    }

    #[test]
    fn sub_agent_text_stays_out_of_the_main_conversation() {
        let mut e = ev(
            "model.delta",
            json!({"block_index":0,"kind":"text","text":"x"}),
        );
        e.agent_id = Some("a".into());
        let mut c = Convo::default();
        c.apply(&e);
        assert!(c.items.is_empty());
    }
}
