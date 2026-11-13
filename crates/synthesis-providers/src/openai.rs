//! OpenAI-compatible Chat Completions client. Port of Node `providers/openai.js`: Anthropic-format
//! requests are converted to Chat Completions and the chunk stream is mapped back to normalized
//! events. (Node does not use the Responses API, neither do we.)

use crate::error::{ErrorKind, ProviderError, Redaction};
use crate::http::Mapper;
use crate::sse::SseEvent;
use crate::types::*;
use serde::Deserialize;
use serde_json::{Map, Value, json};
use std::collections::{BTreeSet, HashMap};

pub const DEFAULT_BASE_URL: &str = "https://api.openai.com/v1";

pub fn base_url(configured: Option<&str>) -> String {
    configured
        .filter(|u| !u.is_empty())
        .unwrap_or(DEFAULT_BASE_URL)
        .to_string()
}

fn image_url(s: &ImageSource) -> String {
    match s {
        ImageSource::Url { url } => url.clone(),
        ImageSource::Base64 { media_type, data } => format!("data:{media_type};base64,{data}"),
        ImageSource::Other => String::new(),
    }
}

/// Node `toolResultText`.
fn tool_result_text(c: Option<&Value>) -> String {
    let s = |v: &Value| serde_json::to_string(v).unwrap_or_default();
    match c {
        None => String::new(),
        Some(Value::String(t)) => t.clone(),
        Some(Value::Array(items)) => items
            .iter()
            .map(|i| {
                match (
                    i.get("type").and_then(Value::as_str),
                    i.get("text").and_then(Value::as_str),
                ) {
                    (Some("text"), Some(t)) => t.to_string(),
                    _ => s(i),
                }
            })
            .collect::<Vec<_>>()
            .join("\n"),
        Some(other) => s(other),
    }
}

fn convert_user(content: &Content, out: &mut Vec<Value>) {
    let blocks = match content {
        Content::Text(t) => return out.push(json!({"role": "user", "content": t})),
        Content::Blocks(b) => b,
    };
    let (mut parts, mut tools) = (Vec::new(), Vec::new());
    for b in blocks {
        match b {
            Block::ToolResult { tool_use_id, content, .. } => tools
                .push(json!({"role": "tool", "tool_call_id": tool_use_id, "content": tool_result_text(content.as_ref())})),
            Block::Text { text } => parts.push(json!({"type": "text", "text": text})),
            Block::Image { source } => parts.push(json!({"type": "image_url", "image_url": {"url": image_url(source)}})),
            _ => {}
        }
    }
    // Tool messages must directly follow the assistant tool_calls message; Node emitted the user
    // parts first, which the API rejects when a turn mixes tool results and text.
    out.extend(tools);
    if !parts.is_empty() {
        out.push(json!({"role": "user", "content": parts}));
    }
}

fn convert_assistant(content: &Content) -> Value {
    let blocks = match content {
        Content::Text(t) => return json!({"role": "assistant", "content": t}),
        Content::Blocks(b) => b,
    };
    let mut text = String::new();
    let mut calls = Vec::new();
    for b in blocks {
        match b {
            Block::Text { text: t } => text.push_str(t),
            Block::ToolUse { id, name, input } => {
                let args = serde_json::to_string(&input.clone().unwrap_or_else(|| json!({})))
                    .unwrap_or_default();
                calls.push(json!({"id": id, "type": "function", "function": {"name": name, "arguments": args}}));
            }
            _ => {} // thinking blocks have no Chat Completions equivalent
        }
    }
    let mut m = Map::new();
    m.insert("role".into(), "assistant".into());
    m.insert(
        "content".into(),
        if text.is_empty() {
            Value::Null
        } else {
            text.into()
        },
    );
    if !calls.is_empty() {
        m.insert("tool_calls".into(), calls.into());
    }
    Value::Object(m)
}

/// Reasoning models (o-series, gpt-5) reject `temperature`; match on the id after the last `/`.
fn rejects_temperature(model: &str) -> bool {
    let id = model.rsplit('/').next().unwrap_or(model).to_lowercase();
    id.starts_with("gpt-5")
        || (id.starts_with('o') && id[1..].starts_with(|c: char| c.is_ascii_digit()))
}

pub fn build_body(req: &Request) -> Value {
    let mut messages = Vec::new();
    let sys = match &req.system {
        Some(System::Text(t)) => t.clone(),
        Some(System::Blocks(b)) => b
            .iter()
            .map(|b| b.text.as_deref().unwrap_or(""))
            .collect::<Vec<_>>()
            .join("\n"),
        None => String::new(),
    };
    if !sys.is_empty() {
        messages.push(json!({"role": "system", "content": sys}));
    }
    for m in &req.messages {
        match m.role {
            Role::User => convert_user(&m.content, &mut messages),
            Role::Assistant => messages.push(convert_assistant(&m.content)),
            Role::System => {}
        }
    }
    let mut body = Map::new();
    body.insert("model".into(), req.model.clone().into());
    body.insert("messages".into(), messages.into());
    body.insert("max_completion_tokens".into(), req.max_tokens.into());
    body.insert("stream".into(), true.into());
    body.insert("stream_options".into(), json!({"include_usage": true}));
    if !req.tools.is_empty() {
        let tools: Vec<Value> = req
            .tools
            .iter()
            .map(|t| {
                let mut f = Map::new();
                f.insert("name".into(), t.name.clone().into());
                if let Some(d) = &t.description {
                    f.insert("description".into(), d.clone().into());
                }
                f.insert("parameters".into(), t.input_schema.clone());
                json!({"type": "function", "function": f})
            })
            .collect();
        body.insert("tools".into(), tools.into());
    }
    if let Some(e) = req.reasoning_effort.as_ref().filter(|e| !e.is_empty()) {
        body.insert("reasoning_effort".into(), e.clone().into());
    }
    if let Some(t) = req.temperature
        && !rejects_temperature(&req.model)
    {
        body.insert("temperature".into(), json!(t));
    }
    Value::Object(body)
}

#[derive(Deserialize, Default)]
struct Chunk {
    id: Option<String>,
    model: Option<String>,
    #[serde(default)]
    choices: Option<Vec<Choice>>,
    usage: Option<OaUsage>,
}

#[derive(Deserialize)]
struct Choice {
    delta: Option<OaDelta>,
    finish_reason: Option<String>,
}

#[derive(Deserialize, Default)]
struct OaDelta {
    content: Option<String>,
    tool_calls: Option<Vec<ToolCallDelta>>,
}

#[derive(Deserialize)]
struct ToolCallDelta {
    index: Option<u64>,
    id: Option<String>,
    function: Option<FnDelta>,
}

#[derive(Deserialize)]
struct FnDelta {
    name: Option<String>,
    arguments: Option<String>,
}

#[derive(Deserialize)]
struct OaUsage {
    prompt_tokens: Option<u64>,
    completion_tokens: Option<u64>,
    prompt_tokens_details: Option<PromptDetails>,
}

#[derive(Deserialize)]
struct PromptDetails {
    cached_tokens: Option<u64>,
}

fn map_finish(r: &str) -> StopReason {
    match r {
        "tool_calls" => StopReason::ToolUse,
        "length" => StopReason::MaxTokens,
        _ => StopReason::EndTurn, // "stop", "function_call" and anything else
    }
}

/// `prompt_tokens` includes the cached ones; `input_tokens` here excludes them (Anthropic's
/// accounting) so a cached token is priced once, at the cache-read price.
fn map_usage(u: &OaUsage) -> Usage {
    let cached = u
        .prompt_tokens_details
        .as_ref()
        .and_then(|d| d.cached_tokens)
        .filter(|c| *c > 0);
    Usage {
        input_tokens: Some(
            u.prompt_tokens
                .unwrap_or(0)
                .saturating_sub(cached.unwrap_or(0)),
        ),
        output_tokens: Some(u.completion_tokens.unwrap_or(0)),
        cache_read_input_tokens: cached,
        cache_creation_input_tokens: None,
    }
}

pub struct OpenAiMapper {
    redaction: Redaction,
    started: bool,
    text_index: Option<usize>,
    next_index: usize,
    tool_blocks: HashMap<Option<u64>, usize>,
    open: BTreeSet<usize>,
    stop_reason: Option<StopReason>,
    finish_seen: bool,
    usage_seen: bool,
}

impl OpenAiMapper {
    pub fn new(redaction: Redaction) -> Self {
        Self {
            redaction,
            started: false,
            text_index: None,
            next_index: 0,
            tool_blocks: HashMap::new(),
            open: BTreeSet::new(),
            stop_reason: None,
            finish_seen: false,
            usage_seen: false,
        }
    }

    /// One decoded chunk; mirrors the Node generator step for step.
    pub fn push_chunk(&mut self, v: Value) -> Result<Vec<StreamEvent>, ProviderError> {
        if v.get("error").is_some_and(|e| !e.is_null()) {
            return Err(self.redaction.api_error(None, &v.to_string(), None));
        }
        let chunk: Chunk = serde_json::from_value(v).map_err(|e| {
            ProviderError::new(
                ErrorKind::InvalidResponse,
                self.redaction.text(&format!("malformed stream chunk: {e}")),
            )
        })?;
        let mut out = Vec::new();
        if !self.started {
            self.started = true;
            out.push(StreamEvent::MessageStart {
                message: MessageStart {
                    id: chunk.id.clone(),
                    model: chunk.model.clone(),
                    usage: Usage {
                        input_tokens: Some(0),
                        output_tokens: Some(0),
                        ..Default::default()
                    },
                },
            });
        }
        for choice in chunk.choices.unwrap_or_default() {
            let delta = choice.delta.unwrap_or_default();
            if let Some(text) = delta.content.filter(|t| !t.is_empty()) {
                let idx = match self.text_index {
                    Some(i) => i,
                    None => {
                        let i = self.alloc();
                        self.text_index = Some(i);
                        out.push(StreamEvent::ContentBlockStart {
                            index: i,
                            content_block: StartBlock::Text {
                                text: String::new(),
                            },
                        });
                        i
                    }
                };
                out.push(StreamEvent::ContentBlockDelta {
                    index: idx,
                    delta: Delta::TextDelta { text },
                });
            }
            for tc in delta.tool_calls.unwrap_or_default() {
                let idx = match self.tool_blocks.get(&tc.index) {
                    Some(i) => *i,
                    None => {
                        let i = self.alloc();
                        self.tool_blocks.insert(tc.index, i);
                        out.push(StreamEvent::ContentBlockStart {
                            index: i,
                            content_block: StartBlock::ToolUse {
                                id: tc.id.clone(),
                                name: tc.function.as_ref().and_then(|f| f.name.clone()),
                                input: json!({}),
                            },
                        });
                        i
                    }
                };
                if let Some(args) = tc
                    .function
                    .and_then(|f| f.arguments)
                    .filter(|a| !a.is_empty())
                {
                    out.push(StreamEvent::ContentBlockDelta {
                        index: idx,
                        delta: Delta::InputJsonDelta { partial_json: args },
                    });
                }
            }
            if let Some(reason) = choice.finish_reason.filter(|r| !r.is_empty()) {
                out.extend(
                    std::mem::take(&mut self.open)
                        .into_iter()
                        .map(|index| StreamEvent::ContentBlockStop { index }),
                );
                self.stop_reason = Some(map_finish(&reason));
                self.finish_seen = true;
            }
        }
        if let Some(u) = &chunk.usage {
            self.usage_seen = true;
            out.push(self.message_end(Some(map_usage(u))));
            out.push(StreamEvent::MessageStop);
        }
        Ok(out)
    }

    fn alloc(&mut self) -> usize {
        let i = self.next_index;
        self.next_index += 1;
        self.open.insert(i);
        i
    }

    fn message_end(&self, usage: Option<Usage>) -> StreamEvent {
        StreamEvent::MessageDelta {
            delta: MessageDeltaBody {
                stop_reason: Some(self.stop_reason.clone().unwrap_or(StopReason::EndTurn)),
            },
            usage,
        }
    }
}

impl Mapper for OpenAiMapper {
    fn feed(&mut self, ev: SseEvent) -> Result<Vec<StreamEvent>, ProviderError> {
        let d = ev.data.trim();
        if d.is_empty() || d == "[DONE]" {
            return Ok(vec![]);
        }
        let v = serde_json::from_str::<Value>(d).map_err(|e| {
            ProviderError::new(
                ErrorKind::InvalidResponse,
                self.redaction.text(&format!("malformed stream chunk: {e}")),
            )
        })?;
        self.push_chunk(v)
    }

    /// Some gateways never send the usage-only chunk. If a finish_reason arrived the message is
    /// closed here (usage stays unreported); otherwise the stream was cut short.
    fn finish(&mut self) -> Result<Vec<StreamEvent>, ProviderError> {
        if self.usage_seen {
            return Ok(vec![]);
        }
        if self.finish_seen {
            return Ok(vec![self.message_end(None), StreamEvent::MessageStop]);
        }
        let what = if self.started {
            "stream ended before finish_reason"
        } else {
            "empty stream"
        };
        Err(ProviderError::new(ErrorKind::InvalidResponse, what))
    }
}
