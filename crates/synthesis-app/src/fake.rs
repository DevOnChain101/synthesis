//! `--provider fake:<script.json>` (feature `test-provider`): replays a scripted conversation.
//! Script = JSON array of turns: `{"text"?: str, "tools"?: [{"id"?: str, "name": str, "input": obj}],
//! "delay_ms"?: u64, "usage"?: [input, output]}`.

use std::path::Path;
use std::sync::Arc;

use serde_json::Value;
use synthesis_harness::testing::{FakeProvider, Turn};
use synthesis_providers::ModelProvider;

pub fn load(path: &Path) -> Result<Arc<dyn ModelProvider>, String> {
    let raw = std::fs::read_to_string(path)
        .map_err(|e| format!("fake script {}: {e}", path.display()))?;
    let script: Vec<Value> = serde_json::from_str(&raw).map_err(|e| format!("fake script: {e}"))?;
    let mut turns = Vec::new();
    for (i, t) in script.iter().enumerate() {
        let text = t.get("text").and_then(Value::as_str).unwrap_or("");
        let calls: Vec<(String, String, Value)> = t
            .get("tools")
            .and_then(Value::as_array)
            .map(|a| a.as_slice())
            .unwrap_or_default()
            .iter()
            .enumerate()
            .map(|(k, c)| {
                (
                    c.get("id")
                        .and_then(Value::as_str)
                        .map_or(format!("fake_{i}_{k}"), String::from),
                    c.get("name")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                    c.get("input").cloned().unwrap_or(Value::Null),
                )
            })
            .collect();
        let refs: Vec<(&str, &str, Value)> = calls
            .iter()
            .map(|(a, b, c)| (a.as_str(), b.as_str(), c.clone()))
            .collect();
        let mut turn = if refs.is_empty() {
            Turn::text(text)
        } else {
            Turn::text_and_tools(text, &refs)
        };
        if let Some(ms) = t.get("delay_ms").and_then(Value::as_u64) {
            turn = turn.delayed(ms);
        }
        if let Some([a, b]) = t.get("usage").and_then(Value::as_array).map(Vec::as_slice) {
            turn = turn.with_usage(a.as_u64().unwrap_or(0), b.as_u64().unwrap_or(0));
        }
        turns.push(turn);
    }
    Ok(Arc::new(FakeProvider::new(turns)))
}
