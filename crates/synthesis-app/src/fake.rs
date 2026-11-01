//! `--provider fake:<script.json>` (feature `test-provider`): replays a scripted conversation.
//! Script = JSON array of turns: `{"text"?: str, "tools"?: [{"id"?: str, "name": str, "input": obj}],
//! "delay_ms"?: u64, "usage"?: [input, output]}`.
//!
//! Test-only extras for scripts that drive several agents of one process (cross-session peers):
//! - `"for": "responder" | "delegate"`: the turn is served to the peer responder / the peer
//!   delegate worker of this session (found by their system prompt), not to the main agent, so
//!   the order of requests between agents does not matter. Each kind has its own queue; an empty
//!   queue answers `DECLINED: nothing scripted`.
//! - `"seen": {"needle": str, "yes": str, "no": str}`: the text is `yes` when the request's
//!   messages contain `needle`, else `no` (lets a cross-process test see what a model was shown).

use std::collections::VecDeque;
use std::path::Path;
use std::sync::{Arc, Mutex};

use serde_json::Value;
use synthesis_harness::testing::{FakeProvider, Turn};
use synthesis_providers::{ModelProvider, Request};

fn turn_of(i: usize, t: &Value, req: Option<&Request>) -> Turn {
    let mut text = t
        .get("text")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    if let (Some(s), Some(r)) = (t.get("seen"), req) {
        let get = |k: &str| s.get(k).and_then(Value::as_str).unwrap_or("");
        let seen = format!("{:?}", r.messages).contains(get("needle"));
        text = get(if seen { "yes" } else { "no" }).to_string();
    }
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
        Turn::text(&text)
    } else {
        Turn::text_and_tools(&text, &refs)
    };
    if let Some(ms) = t.get("delay_ms").and_then(Value::as_u64) {
        turn = turn.delayed(ms);
    }
    if let Some([a, b]) = t.get("usage").and_then(Value::as_array).map(Vec::as_slice) {
        turn = turn.with_usage(a.as_u64().unwrap_or(0), b.as_u64().unwrap_or(0));
    }
    turn
}

pub fn load(path: &Path) -> Result<Arc<dyn ModelProvider>, String> {
    let raw = std::fs::read_to_string(path)
        .map_err(|e| format!("fake script {}: {e}", path.display()))?;
    let script: Vec<Value> = serde_json::from_str(&raw).map_err(|e| format!("fake script: {e}"))?;
    let role = |t: &Value| t.get("for").and_then(Value::as_str).map(String::from);
    let main: Vec<Turn> = script
        .iter()
        .enumerate()
        .filter(|(_, t)| role(t).is_none())
        .map(|(i, t)| turn_of(i, t, None))
        .collect();
    let queue = |r: &str| -> VecDeque<(usize, Value)> {
        script
            .iter()
            .enumerate()
            .filter(|(_, t)| role(t).as_deref() == Some(r))
            .map(|(i, t)| (i, t.clone()))
            .collect()
    };
    let queues = Mutex::new((queue("responder"), queue("delegate")));
    let mut fake = FakeProvider::new(main);
    if script.iter().any(|t| role(t).is_some()) {
        fake = fake.with_responder(Arc::new(move |req: &Request| {
            let system = serde_json::to_string(&req.system).unwrap_or_default();
            let mut q = queues.lock().unwrap_or_else(|e| e.into_inner());
            let q = if system.contains("read-only peer responder") {
                &mut q.0
            } else if system.contains("peer delegate worker") {
                &mut q.1
            } else {
                return None;
            };
            Some(match q.pop_front() {
                Some((i, t)) => turn_of(i, &t, Some(req)),
                None => Turn::text("DECLINED: nothing scripted"),
            })
        }));
    }
    Ok(Arc::new(fake))
}
