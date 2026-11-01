//! MCP tool search (Claude Code's "Scale with MCP tool search"): when the MCP tool definitions
//! are big (or `synthesis.mcp.toolSearch` is `on`) the request lists only their names and the
//! model loads schemas with `ToolSearch`. Permissions are unchanged: a loaded tool is still
//! checked under its full `mcp__server__tool` name, and a deny rule hides it from the search.

use std::collections::BTreeSet;

use serde_json::{Value, json};
use synthesis_events::ToolStatus;
use synthesis_tools::{SideEffectClass, ToolRuntime, ToolSpec};

use crate::context::wrap_tool_output;
use crate::harness::{AgentCtx, Harness, TaskRun, ToolSearchMode};
use crate::util::lock;

pub(crate) const NAME: &str = "ToolSearch";
const DEFAULT_RESULTS: usize = 5;
/// `auto`: defer once the MCP definitions take this share of the context window (docs: 10%).
const AUTO_PERCENT: u64 = 10;

/// Per task: deferral is on (it stays on, as in the docs) and which deferred tools were loaded.
#[derive(Default)]
pub(crate) struct Loaded {
    active: bool,
    names: BTreeSet<String>,
}

pub(crate) fn spec() -> ToolSpec {
    ToolSpec {
        name: NAME.into(),
        description: "Loads the full definitions of deferred MCP tools so you can call them. \
            Deferred tools are listed by name only. query: \"select:name1,name2\" loads those \
            exact tools; otherwise space-separated keywords are matched against tool names and \
            descriptions. A loaded tool can be called in the rest of this task."
            .into(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "query": {"type": "string"},
                "max_results": {"type": "integer", "minimum": 1, "maximum": 20}
            },
            "required": ["query"]
        }),
        side_effect: SideEffectClass::Read,
        runtime: ToolRuntime::Harness,
    }
}

fn is_mcp(s: &ToolSpec) -> bool {
    matches!(s.runtime, ToolRuntime::Mcp { .. })
}

impl Harness {
    /// `(deferral is on, names whose schemas are not loaded)` among the tools `agent` may see.
    pub(crate) fn deferred(
        &self,
        task: &TaskRun,
        visible: &[ToolSpec],
    ) -> (bool, BTreeSet<String>) {
        let mcp: Vec<&ToolSpec> = visible.iter().filter(|s| is_mcp(s)).collect();
        let usable = visible.iter().any(|s| s.name == NAME) && !mcp.is_empty();
        if !usable || self.config.tool_search == ToolSearchMode::Off {
            return (false, BTreeSet::new());
        }
        let mut st = lock(&task.tool_search);
        if !st.active {
            let chars: usize = mcp
                .iter()
                .map(|s| s.name.len() + s.description.len() + s.input_schema.to_string().len())
                .sum();
            let tokens = (chars / 4) as u64;
            st.active = self.config.tool_search == ToolSearchMode::On
                || tokens * 100 >= AUTO_PERCENT * Self::window(task);
        }
        if !st.active {
            return (false, BTreeSet::new());
        }
        let names = mcp
            .iter()
            .filter(|s| !st.names.contains(&s.name))
            .map(|s| s.name.clone())
            .collect();
        (true, names)
    }

    /// True when `name` is an MCP tool whose schema the model has not loaded yet.
    pub(crate) fn is_deferred(&self, task: &TaskRun, agent: &AgentCtx, name: &str) -> bool {
        self.deferred(task, &self.visible_specs(agent))
            .1
            .contains(name)
    }

    pub(crate) fn run_tool_search(
        &self,
        task: &TaskRun,
        agent: &AgentCtx,
        input: &Value,
    ) -> crate::exec::Res {
        let visible = self.visible_specs(agent);
        let (_, deferred) = self.deferred(task, &visible);
        let pool: Vec<&ToolSpec> = visible
            .iter()
            .filter(|s| deferred.contains(&s.name))
            .collect();
        let query = input
            .get("query")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim();
        let max = input
            .get("max_results")
            .and_then(Value::as_u64)
            .map_or(DEFAULT_RESULTS, |n| n.clamp(1, 20) as usize);
        let (mut picked, mut missing) = (Vec::<&ToolSpec>::new(), Vec::<&str>::new());
        if let Some(list) = query.strip_prefix("select:") {
            for n in list.split(',').map(str::trim).filter(|n| !n.is_empty()) {
                match pool.iter().find(|s| s.name == n) {
                    Some(s) if !picked.iter().any(|p| p.name == n) => picked.push(s),
                    Some(_) => {}
                    None => missing.push(n),
                }
            }
        } else {
            let terms: Vec<String> = query.split_whitespace().map(str::to_lowercase).collect();
            let mut scored: Vec<(usize, &ToolSpec)> = pool
                .iter()
                .map(|s| {
                    let hay = format!("{} {}", s.name, s.description).to_lowercase();
                    (
                        terms.iter().filter(|t| hay.contains(t.as_str())).count(),
                        *s,
                    )
                })
                .filter(|(n, _)| *n > 0)
                .collect();
            scored.sort_by_key(|(n, _)| std::cmp::Reverse(*n));
            picked = scored.into_iter().take(max).map(|(_, s)| s).collect();
        }
        lock(&task.tool_search)
            .names
            .extend(picked.iter().map(|s| s.name.clone()));
        let mut text = if picked.is_empty() {
            format!(
                "No deferred tool matched. Deferred tools: {}",
                deferred.iter().cloned().collect::<Vec<_>>().join(", ")
            )
        } else {
            let defs: Vec<Value> = picked
                .iter()
                .map(|s| json!({"name": s.name, "description": s.description, "input_schema": s.input_schema}))
                .collect();
            format!(
                "Loaded {} tool(s); call them by these names now.\n{}",
                defs.len(),
                Value::Array(defs)
            )
        };
        if !missing.is_empty() {
            text.push_str(&format!(
                "\nNot a deferred tool (unknown, already loaded or not available): {}",
                missing.join(", ")
            ));
        }
        let mut res = self.plain(text.clone(), false, ToolStatus::Ok);
        // descriptions come from the MCP server: data, not instructions
        res.content = Value::String(wrap_tool_output(NAME, &text));
        res
    }
}
