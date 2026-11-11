//! Anthropic Messages API client. Port of Node `providers/anthropic.js`: events already have the
//! normalized shape; the request gets prompt-cache markers on the last system block and on the
//! last block of the last user message.

use crate::error::{ErrorKind, ProviderError, Redaction};
use crate::http::Mapper;
use crate::sse::SseEvent;
use crate::types::{Request, StreamEvent, System};
use serde_json::{Map, Value, json};

pub const DEFAULT_BASE_URL: &str = "https://api.anthropic.com";
pub const DEFAULT_VERSION: &str = "2023-06-01";

/// Node `stripV1`: a trailing `/v1` is dropped (we append `/v1/...` ourselves); default host if unset.
pub fn base_url(configured: Option<&str>) -> String {
    let u = match configured.filter(|u| !u.is_empty()) {
        None => return DEFAULT_BASE_URL.to_string(),
        Some(u) => u,
    };
    let t = u.strip_suffix('/').unwrap_or(u);
    t.strip_suffix("/v1").unwrap_or(t).to_string()
}

fn ephemeral() -> Value {
    json!({"type": "ephemeral"})
}

fn mark_last(arr: &mut [Value]) {
    if let Some(Value::Object(o)) = arr.last_mut() {
        o.insert("cache_control".into(), ephemeral());
    }
}

/// Request body exactly as Node builds it.
pub fn build_body(req: &Request) -> Value {
    let mut messages = serde_json::to_value(&req.messages).unwrap_or_default();
    if let Value::Array(msgs) = &mut messages
        && let Some(m) = msgs.iter_mut().rev().find(|m| m["role"] == "user")
        && let Some(Value::Array(blocks)) = m.get_mut("content")
    {
        mark_last(blocks);
    }
    let mut body = Map::new();
    body.insert("model".into(), req.model.clone().into());
    body.insert("max_tokens".into(), req.max_tokens.into());
    body.insert("messages".into(), messages);
    body.insert("stream".into(), true.into());
    match &req.system {
        Some(System::Blocks(b)) => {
            let mut v = serde_json::to_value(b).unwrap_or_default();
            if let Value::Array(a) = &mut v {
                mark_last(a);
            }
            body.insert("system".into(), v);
        }
        Some(System::Text(t)) => {
            body.insert("system".into(), t.clone().into());
        }
        None => {}
    }
    if !req.tools.is_empty() {
        let tools: Vec<Value> = req
            .tools
            .iter()
            .map(|t| {
                let mut o = Map::new();
                o.insert("name".into(), t.name.clone().into());
                if let Some(d) = &t.description {
                    o.insert("description".into(), d.clone().into());
                }
                o.insert("input_schema".into(), t.input_schema.clone());
                Value::Object(o)
            })
            .collect();
        body.insert("tools".into(), tools.into());
    }
    match (&req.thinking, req.temperature) {
        (Some(t), _) => {
            body.insert(
                "thinking".into(),
                json!({"type": "enabled", "budget_tokens": t.budget_tokens}),
            );
        }
        (None, Some(temp)) => {
            body.insert("temperature".into(), json!(temp));
        }
        _ => {}
    }
    // Anthropic's effort parameter (`output_config.effort`)
    if let Some(e) = req.reasoning_effort.as_ref().filter(|e| !e.is_empty()) {
        body.insert("output_config".into(), json!({"effort": e}));
    }
    Value::Object(body)
}

pub struct AnthropicMapper {
    redaction: Redaction,
    started: bool,
    stopped: bool,
}

impl AnthropicMapper {
    pub fn new(redaction: Redaction) -> Self {
        Self {
            redaction,
            started: false,
            stopped: false,
        }
    }

    /// One decoded SSE `data` payload. In-stream `error` events become errors; unknown event
    /// types are dropped so new server events do not break old clients.
    pub fn push_value(&mut self, v: Value) -> Result<Vec<StreamEvent>, ProviderError> {
        if v.get("type").and_then(Value::as_str) == Some("error") {
            return Err(self.redaction.api_error(None, &v.to_string(), None));
        }
        let ev: StreamEvent = serde_json::from_value(v).map_err(|e| {
            ProviderError::new(
                ErrorKind::InvalidResponse,
                self.redaction.text(&format!("malformed stream event: {e}")),
            )
        })?;
        self.started = true;
        Ok(match ev {
            StreamEvent::Unknown => vec![],
            StreamEvent::MessageStop => {
                self.stopped = true;
                vec![ev]
            }
            e => vec![e],
        })
    }
}

impl Mapper for AnthropicMapper {
    fn feed(&mut self, ev: SseEvent) -> Result<Vec<StreamEvent>, ProviderError> {
        if ev.data.trim().is_empty() {
            return Ok(vec![]);
        }
        let v = serde_json::from_str::<Value>(&ev.data).map_err(|e| {
            ProviderError::new(
                ErrorKind::InvalidResponse,
                self.redaction.text(&format!("malformed stream event: {e}")),
            )
        })?;
        self.push_value(v)
    }

    fn finish(&mut self) -> Result<Vec<StreamEvent>, ProviderError> {
        if self.stopped {
            return Ok(vec![]);
        }
        let what = if self.started {
            "stream ended before message_stop"
        } else {
            "empty stream"
        };
        Err(ProviderError::new(ErrorKind::InvalidResponse, what))
    }
}
