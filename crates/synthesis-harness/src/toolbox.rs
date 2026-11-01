//! Tool set seen by the loop: the shared `Registry` plus extra tools (MCP adapters, tests).
//! Schema validation happens here, before policy (tools.md §3 step 1); `Registry::execute`
//! validates again, which is harmless.

use std::sync::Arc;

use synthesis_core::ToolContext;

use crate::mcp_guard::McpGuard;
use serde_json::{Value, json};
use synthesis_tools::{Registry, Tool, ToolOutput, ToolSpec};

/// Tools that can change while the session runs (MCP `tools/list_changed`). Read on every call
/// to `specs` / `execute`, so implementations should cache.
pub trait ToolSource: Send + Sync {
    fn tools(&self) -> Vec<Arc<dyn Tool>>;
}

#[derive(Default)]
pub struct ToolBox {
    registry: Option<Arc<Registry>>,
    extra: Vec<Arc<dyn Tool>>,
    sources: Vec<Arc<dyn ToolSource>>,
    guard: Option<Arc<McpGuard>>,
}

impl ToolBox {
    pub fn new(registry: Registry) -> Self {
        Self {
            registry: Some(Arc::new(registry)),
            ..Self::default()
        }
    }

    pub fn empty() -> Self {
        Self::default()
    }

    /// Extra tools win over registry tools of the same name.
    pub fn with_tool(mut self, tool: Arc<dyn Tool>) -> Self {
        self.extra.push(tool);
        self
    }

    /// Tools that appear and disappear at run time; like `with_tool` they win over the registry.
    pub fn with_source(mut self, source: Arc<dyn ToolSource>) -> Self {
        self.sources.push(source);
        self
    }

    pub fn with_guard(mut self, guard: Arc<McpGuard>) -> Self {
        self.guard = Some(guard);
        self
    }

    pub fn guard(&self) -> Option<&McpGuard> {
        self.guard.as_deref()
    }

    fn extras(&self) -> Vec<Arc<dyn Tool>> {
        let mut v = self.extra.clone();
        v.extend(self.sources.iter().flat_map(|s| s.tools()));
        v
    }

    pub fn specs(&self) -> Vec<ToolSpec> {
        let extra = self.extras();
        let mut out: Vec<ToolSpec> = self
            .registry
            .iter()
            .flat_map(|r| r.specs().cloned().collect::<Vec<_>>())
            .filter(|s| !extra.iter().any(|t| t.spec().name == s.name))
            .collect();
        out.extend(extra.iter().map(|t| t.spec().clone()));
        out
    }

    pub fn spec(&self, name: &str) -> Option<ToolSpec> {
        if name == crate::toolsearch::NAME {
            return Some(crate::toolsearch::spec());
        }
        self.specs().into_iter().find(|s| s.name == name)
    }

    pub async fn execute(&self, name: &str, input: Value, ctx: &ToolContext) -> ToolOutput {
        if let Some(t) = self
            .extras()
            .into_iter()
            .rev()
            .find(|t| t.spec().name == name)
        {
            return t.execute(input, ctx).await;
        }
        match &self.registry {
            Some(r) => r.execute(name, input, ctx).await,
            None => ToolOutput::error(format!("Unknown tool \"{name}\""))
                .with_meta(json!({"error": "unknown_tool"})),
        }
    }
}

impl ToolBox {
    /// Input safe for events, hooks, approval prompts and history (secrets removed). Uses the
    /// registry's rule; without a registry the shared `synthesis_tools::redact_input` applies.
    pub fn redact_input(&self, name: &str, input: &Value) -> Value {
        match &self.registry {
            Some(r) => r.redact_input(name, input),
            None => synthesis_tools::redact_input(name, input),
        }
    }

    /// True when `redact_input` changes this particular input (a sensitive call).
    pub fn is_sensitive(&self, name: &str, input: &Value) -> bool {
        self.redact_input(name, input) != *input
    }

    /// True when the tool can mask its input at all, so streamed argument fragments (which
    /// cannot be redacted before they are complete) must not be emitted. Probed with a
    /// sensitive-looking input: shortcut, tools whose masking keys off other fields need
    /// `synthesis_tools` to expose that, then switch this to the real predicate.
    pub fn masks_input(&self, name: &str) -> bool {
        let probe = json!({"sensitive": true, "value": "x", "text": "x", "password": "x"});
        self.is_sensitive(name, &probe)
    }
}

/// First five violations of `input` against the tool's schema.
pub fn validate(spec: &ToolSpec, input: &Value) -> Result<(), String> {
    let v = jsonschema::validator_for(&spec.input_schema)
        .map_err(|e| format!("{}: tool schema is invalid: {e}", spec.name))?;
    let list: Vec<String> = v
        .iter_errors(input)
        .take(5)
        .map(|e| format!("{} ({})", e, e.instance_path()))
        .collect();
    if list.is_empty() {
        Ok(())
    } else {
        Err(format!("{}: invalid input: {}", spec.name, list.join("; ")))
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use synthesis_tools::{BoxFuture, SideEffectClass, ToolRuntime};

    use super::*;

    struct Named(ToolSpec);
    impl Tool for Named {
        fn spec(&self) -> &ToolSpec {
            &self.0
        }
        fn execute<'a>(&'a self, _: Value, _: &'a ToolContext) -> BoxFuture<'a, ToolOutput> {
            Box::pin(async { ToolOutput::text("ok") })
        }
    }

    struct Live(Mutex<Vec<&'static str>>);
    impl ToolSource for Live {
        fn tools(&self) -> Vec<Arc<dyn Tool>> {
            self.0
                .lock()
                .unwrap()
                .iter()
                .map(|n| -> Arc<dyn Tool> {
                    Arc::new(Named(ToolSpec {
                        name: (*n).into(),
                        description: String::new(),
                        input_schema: json!({"type": "object"}),
                        side_effect: SideEffectClass::External,
                        runtime: ToolRuntime::Mcp { server: "s".into() },
                    }))
                })
                .collect()
        }
    }

    #[test]
    fn a_source_adds_and_removes_tools_while_the_box_lives() {
        let src = Arc::new(Live(Mutex::new(vec!["mcp__s__a"])));
        let tb = ToolBox::empty().with_source(src.clone());
        let names = |t: &ToolBox| t.specs().into_iter().map(|s| s.name).collect::<Vec<_>>();
        assert_eq!(names(&tb), ["mcp__s__a"]);
        *src.0.lock().unwrap() = vec!["mcp__s__a", "mcp__s__b"];
        assert_eq!(names(&tb), ["mcp__s__a", "mcp__s__b"]);
        assert!(tb.spec("mcp__s__b").is_some());
        *src.0.lock().unwrap() = vec!["mcp__s__b"];
        assert_eq!(names(&tb), ["mcp__s__b"]);
        assert!(tb.spec("mcp__s__a").is_none());
    }
}
