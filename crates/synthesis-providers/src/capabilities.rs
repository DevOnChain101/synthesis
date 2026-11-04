//! Per-model capability registry so the harness can degrade gracefully (drop images for a
//! no-vision model, serialize tool calls, skip thinking). Values come from the provider's
//! `models` metadata in config; anything undeclared falls back to the protocol default, which
//! mirrors what Node always assumed.

use indexmap::IndexMap;
use serde_json::{Map, Value};
use synthesis_config::{ModelMeta, Protocol, Provider};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderCapabilities {
    pub tools: bool,
    pub parallel_tools: bool,
    pub images: bool,
    pub reasoning: bool,
    pub streaming: bool,
    /// No client sends a response schema yet; true only if config declares it.
    pub structured_output: bool,
    /// Explicit cache markers are sent (Anthropic). OpenAI caching is automatic and only reported.
    pub prompt_cache: bool,
    pub context_window: Option<u64>,
    pub max_output: Option<u64>,
    /// `capabilities.effort_tiers` of the model: the only effort values a request may carry.
    pub effort_tiers: Vec<String>,
}

fn flag(caps: &Map<String, Value>, names: &[&str]) -> Option<bool> {
    names
        .iter()
        .find_map(|n| caps.get(*n).and_then(Value::as_bool))
}

impl ProviderCapabilities {
    pub fn defaults(protocol: Protocol) -> Self {
        let anthropic = protocol == Protocol::Anthropic;
        Self {
            tools: true,
            parallel_tools: true,
            images: true,
            // Anthropic thinking is opt-out (Node routing.js reasoningFor); OpenAI needs effort tiers.
            reasoning: anthropic,
            streaming: true,
            structured_output: false,
            prompt_cache: anthropic,
            context_window: None,
            max_output: None,
            effort_tiers: vec![],
        }
    }

    pub fn resolve(protocol: Protocol, meta: Option<&ModelMeta>) -> Self {
        let mut c = Self::defaults(protocol);
        let Some(m) = meta else { return c };
        let caps = &m.capabilities;
        let set = |slot: &mut bool, v: Option<bool>| {
            if let Some(v) = v {
                *slot = v;
            }
        };
        set(
            &mut c.tools,
            flag(caps, &["tools", "toolCalling", "tool_calling"]),
        );
        set(
            &mut c.parallel_tools,
            flag(
                caps,
                &["parallel_tools", "parallelToolCalls", "parallel_tool_calls"],
            ),
        );
        set(&mut c.images, flag(caps, &["images", "vision"]));
        set(&mut c.streaming, flag(caps, &["streaming"]));
        set(
            &mut c.structured_output,
            flag(caps, &["structured_output", "structuredOutput"]),
        );
        set(
            &mut c.prompt_cache,
            flag(caps, &["prompt_cache", "promptCache"]),
        );
        c.effort_tiers = caps
            .get("effort_tiers")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();
        let has_tiers = !c.effort_tiers.is_empty();
        let top = [&m.reasoning, &m.thinking]
            .into_iter()
            .flatten()
            .find_map(Value::as_bool);
        if let Some(r) = flag(caps, &["reasoning", "thinking"]).or(top) {
            c.reasoning = r;
        } else if protocol == Protocol::OpenAi && has_tiers {
            c.reasoning = true;
        }
        c.context_window = m.context_window.map(|n| n as u64);
        c.max_output = m.max_output.map(|n| n as u64);
        c
    }
}

/// Capabilities by `(provider id, model id)`. Holds no credentials.
#[derive(Debug, Clone, Default)]
pub struct CapabilityRegistry {
    providers: IndexMap<String, (Protocol, IndexMap<String, ModelMeta>)>,
}

impl CapabilityRegistry {
    pub fn from_providers(providers: &IndexMap<String, Provider>) -> Self {
        Self {
            providers: providers
                .iter()
                .map(|(id, p)| {
                    (
                        id.clone(),
                        (p.protocol, p.models_meta.clone().unwrap_or_default()),
                    )
                })
                .collect(),
        }
    }

    /// Unknown provider: Anthropic-protocol defaults would over-promise, so fall back to OpenAI's.
    pub fn get(&self, provider_id: &str, model: &str) -> ProviderCapabilities {
        match self.providers.get(provider_id) {
            Some((proto, metas)) => ProviderCapabilities::resolve(*proto, metas.get(model)),
            None => ProviderCapabilities::defaults(Protocol::OpenAi),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_overrides_defaults() {
        let meta = ModelMeta {
            context_window: Some(200000.0),
            capabilities: serde_json::from_str(
                r#"{"vision":false,"effort_tiers":["low"],"parallel_tools":false}"#,
            )
            .unwrap(),
            ..Default::default()
        };
        let c = ProviderCapabilities::resolve(Protocol::OpenAi, Some(&meta));
        assert!(!c.images && !c.parallel_tools && c.reasoning && c.tools);
        assert_eq!(c.context_window, Some(200000));
        let d = ProviderCapabilities::defaults(Protocol::OpenAi);
        assert!(!d.reasoning && !d.prompt_cache && d.images);
    }
}
