//! TodoWrite / Task* (one shared in-memory list per session, as in Node) and the two
//! interface tools whose answer comes from the user (`AskUserQuestion`, `ExitPlanMode`).
//! None of them does I/O; the harness reads `meta` to emit events or prompt the user.

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use synthesis_core::ToolContext;

use crate::ToolOutput;
use crate::native::{Shared, lock};
use crate::util::str_field;

/// One entry of the session list, shared by `TodoWrite` and the Task* tools. Only the first five
/// fields reach `meta` (and so the `todo.updated` event); the rest is Task* state.
#[derive(Debug, Clone, PartialEq, Serialize, Default)]
pub(crate) struct Todo {
    pub id: String,
    pub content: String,
    pub status: String,
    #[serde(rename = "activeForm")]
    pub active_form: String,
    pub description: String,
    #[serde(skip)]
    pub blocks: Vec<String>,
    #[serde(skip)]
    pub blocked_by: Vec<String>,
    #[serde(skip)]
    pub owner: Option<String>,
    #[serde(skip)]
    pub metadata: serde_json::Map<String, Value>,
}

fn with_todos(
    sh: &Shared,
    ctx: &ToolContext,
    f: impl FnOnce(&mut Vec<Todo>) -> ToolOutput,
) -> ToolOutput {
    let s = sh.session(ctx);
    let mut todos = lock(&s.todos);
    let mut out = f(&mut todos);
    if !out.is_error {
        out.meta = json!({"todos": *todos});
    }
    out
}

pub(crate) fn todo_write(sh: &Shared, input: &Value, ctx: &ToolContext) -> ToolOutput {
    let items = input["todos"].as_array().map_or(&[][..], Vec::as_slice);
    if items
        .iter()
        .any(|t| t["content"].as_str().is_none_or(|c| c.trim().is_empty()))
    {
        return ToolOutput::error("TodoWrite: every todo needs a non-empty \"content\".");
    }
    with_todos(sh, ctx, |todos| {
        *todos = items
            .iter()
            .enumerate()
            .map(|(i, t)| Todo {
                id: (i + 1).to_string(),
                content: t["content"].as_str().unwrap_or_default().into(),
                status: t["status"].as_str().unwrap_or_default().into(),
                active_form: str_field(t, "activeForm")
                    .or(t["content"].as_str())
                    .unwrap_or_default()
                    .into(),
                ..Todo::default()
            })
            .collect();
        ToolOutput::text(
            "Todos have been modified successfully. Ensure that you continue to use the \
             todo list to track your progress. Please proceed with the current tasks if \
             applicable.",
        )
    })
}

// Task* shapes follow the Agent SDK reference (code.claude.com/docs/en/agent-sdk/typescript,
// "Tool Input Types" / "Tool Output Types"); `description` stays optional on TaskCreate and
// TaskGet adds `activeForm` (both lenient supersets).

fn task_id(input: &Value) -> &str {
    input["taskId"].as_str().unwrap_or_default()
}

fn metadata(input: &Value) -> serde_json::Map<String, Value> {
    input["metadata"].as_object().cloned().unwrap_or_default()
}

pub(crate) fn task_create(sh: &Shared, input: &Value, ctx: &ToolContext) -> ToolOutput {
    let Some(subject) = str_field(input, "subject").filter(|s| !s.trim().is_empty()) else {
        return ToolOutput::error("TaskCreate: \"subject\" must be a non-empty string.");
    };
    with_todos(sh, ctx, |todos| {
        let id = (todos
            .iter()
            .filter_map(|t| t.id.parse::<u64>().ok())
            .max()
            .unwrap_or(0)
            + 1)
        .to_string();
        todos.push(Todo {
            id: id.clone(),
            content: subject.into(),
            status: "pending".into(),
            active_form: str_field(input, "activeForm").unwrap_or(subject).into(),
            description: str_field(input, "description").unwrap_or_default().into(),
            metadata: metadata(input),
            ..Todo::default()
        });
        ToolOutput::text(json!({"task": {"id": id, "subject": subject}}).to_string())
    })
}

pub(crate) fn task_get(sh: &Shared, input: &Value, ctx: &ToolContext) -> ToolOutput {
    let id = task_id(input);
    with_todos(sh, ctx, |todos| {
        let task = todos.iter().find(|t| t.id == id).map(|t| {
            let mut v = json!({"id": t.id, "subject": t.content, "description": t.description,
                "status": t.status, "blocks": t.blocks, "blockedBy": t.blocked_by,
                "activeForm": t.active_form});
            if let Some(o) = &t.owner {
                v["owner"] = json!(o);
            }
            if !t.metadata.is_empty() {
                v["metadata"] = json!(t.metadata);
            }
            v
        });
        ToolOutput::text(json!({"task": task}).to_string())
    })
}

pub(crate) fn task_list(sh: &Shared, _input: &Value, ctx: &ToolContext) -> ToolOutput {
    with_todos(sh, ctx, |todos| {
        let tasks: Vec<Value> = todos
            .iter()
            .map(|t| {
                let mut v = json!({"id": t.id, "subject": t.content, "status": t.status,
                    "blockedBy": t.blocked_by});
                if let Some(o) = &t.owner {
                    v["owner"] = json!(o);
                }
                v
            })
            .collect();
        ToolOutput::text(json!({"tasks": tasks}).to_string())
    })
}

fn update_failed(id: &str, error: String) -> ToolOutput {
    ToolOutput::error(
        json!({"success": false, "taskId": id, "updatedFields": [], "error": error}).to_string(),
    )
}

pub(crate) fn task_update(sh: &Shared, input: &Value, ctx: &ToolContext) -> ToolOutput {
    let id = task_id(input);
    with_todos(sh, ctx, |todos| {
        let Some(at) = todos.iter().position(|t| t.id == id) else {
            return update_failed(id, format!("Task #{id} not found"));
        };
        let ids = |k: &str| -> Vec<String> {
            input[k]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        };
        let (add_blocks, add_blocked_by) = (ids("addBlocks"), ids("addBlockedBy"));
        if let Some(bad) = add_blocks
            .iter()
            .chain(&add_blocked_by)
            .find(|o| o.as_str() == id || !todos.iter().any(|t| &t.id == *o))
        {
            return update_failed(id, format!("Task #{bad} not found or is the task itself"));
        }
        let from = todos[at].status.clone();
        if input["status"] == "deleted" {
            todos.remove(at);
            for t in todos.iter_mut() {
                t.blocks.retain(|b| b != id);
                t.blocked_by.retain(|b| b != id);
            }
            return ToolOutput::text(
                json!({"success": true, "taskId": id, "updatedFields": ["status"],
                    "statusChange": {"from": from, "to": "deleted"}})
                .to_string(),
            );
        }
        let mut fields: Vec<&str> = vec![];
        let t = &mut todos[at];
        let mut set = |name: &'static str, cur: &mut String, v: Option<&str>| {
            if let Some(v) = v.filter(|v| *v != cur) {
                *cur = v.into();
                fields.push(name);
            }
        };
        set("status", &mut t.status, str_field(input, "status"));
        set("subject", &mut t.content, str_field(input, "subject"));
        set(
            "activeForm",
            &mut t.active_form,
            str_field(input, "activeForm"),
        );
        set(
            "description",
            &mut t.description,
            input["description"].as_str(),
        );
        if let Some(o) = str_field(input, "owner").filter(|o| t.owner.as_deref() != Some(o)) {
            t.owner = Some(o.into());
            fields.push("owner");
        }
        // a null value removes the key, as in Claude Code
        for (k, v) in metadata(input) {
            if v.is_null() {
                t.metadata.remove(&k);
            } else {
                t.metadata.insert(k, v);
            }
            if !fields.contains(&"metadata") {
                fields.push("metadata");
            }
        }
        let status_change = fields
            .contains(&"status")
            .then(|| json!({"from": from, "to": t.status}));
        // blocks and blockedBy are two views of one edge
        for (list, mine_blocks) in [(&add_blocks, true), (&add_blocked_by, false)] {
            for o in list {
                let mine = if mine_blocks {
                    &mut todos[at].blocks
                } else {
                    &mut todos[at].blocked_by
                };
                if !mine.contains(o) {
                    mine.push(o.clone());
                }
                if let Some(x) = todos.iter_mut().find(|x| &x.id == o) {
                    let theirs = if mine_blocks {
                        &mut x.blocked_by
                    } else {
                        &mut x.blocks
                    };
                    if !theirs.iter().any(|s| s == id) {
                        theirs.push(id.into());
                    }
                }
            }
        }
        if !add_blocks.is_empty() {
            fields.push("blocks");
        }
        if !add_blocked_by.is_empty() {
            fields.push("blockedBy");
        }
        let mut out = json!({"success": true, "taskId": id, "updatedFields": fields});
        if let Some(c) = status_change {
            out["statusChange"] = c;
        }
        ToolOutput::text(out.to_string())
    })
}

/// What the harness must obtain from the user before the turn can continue. Returned in
/// `ToolOutput.meta["needs_user"]`; the tool itself never blocks.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum NeedsUser {
    /// Show the questions, answer with the chosen option labels.
    AskUserQuestion { questions: Vec<Question> },
    /// Show the markdown plan; on approval leave plan mode and start implementing.
    ExitPlanMode { plan: String },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Question {
    pub question: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub header: Option<String>,
    #[serde(default, rename = "multiSelect")]
    pub multi_select: bool,
    pub options: Vec<QuestionOption>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QuestionOption {
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

fn needs_user(req: &NeedsUser) -> ToolOutput {
    let what = match req {
        NeedsUser::AskUserQuestion { .. } => "AskUserQuestion: waiting for the user's answer.",
        NeedsUser::ExitPlanMode { .. } => "ExitPlanMode: waiting for the user to approve the plan.",
    };
    ToolOutput::text(what).with_meta(json!({"needs_user": req}))
}

pub(crate) fn ask_user_question(_: &Shared, input: &Value, _: &ToolContext) -> ToolOutput {
    match serde_json::from_value::<Vec<Question>>(input["questions"].clone()) {
        Ok(q) if !q.is_empty() && q.iter().all(|q| !q.options.is_empty()) => {
            needs_user(&NeedsUser::AskUserQuestion { questions: q })
        }
        Ok(_) => {
            ToolOutput::error("AskUserQuestion: need at least one question, each with options.")
        }
        Err(e) => ToolOutput::error(format!("AskUserQuestion: invalid questions: {e}")),
    }
}

pub(crate) fn exit_plan_mode(_: &Shared, input: &Value, _: &ToolContext) -> ToolOutput {
    match str_field(input, "plan").filter(|p| !p.trim().is_empty()) {
        Some(plan) => needs_user(&NeedsUser::ExitPlanMode { plan: plan.into() }),
        None => ToolOutput::error("ExitPlanMode: \"plan\" is required."),
    }
}
