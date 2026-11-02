//! Normalized request / response / stream types. The wire shape of Anthropic Messages is the
//! normal form (as in Node); the OpenAI client maps Chat Completions onto it.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    User,
    Assistant,
    /// Tolerated in history like Node does: OpenAI skips it, Anthropic passes it through.
    System,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Content {
    Text(String),
    Blocks(Vec<Block>),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ImageSource {
    Base64 {
        media_type: String,
        data: String,
    },
    Url {
        url: String,
    },
    #[serde(other)]
    Other,
}

/// Content block. `Thinking.signature` and `RedactedThinking.data` are provider-issued: they are
/// carried back verbatim and never synthesized (a missing signature stays `None`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Block {
    Text {
        text: String,
    },
    Image {
        source: ImageSource,
    },
    ToolUse {
        id: String,
        name: String,
        /// `None` only for a call whose streamed arguments were not valid JSON (see
        /// [`crate::AssistantMessage::invalid_tool_inputs`]).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        input: Option<Value>,
    },
    ToolResult {
        tool_use_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        content: Option<Value>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        is_error: Option<bool>,
    },
    Thinking {
        thinking: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        signature: Option<String>,
    },
    RedactedThinking {
        data: String,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    pub content: Content,
}

/// Replaces every `{"type":"image",...}` object, at any depth, by `{"type":"text","text":"[image]"}`.
fn strip_value(v: &mut Value) -> usize {
    match v {
        Value::Object(o) if o.get("type").and_then(Value::as_str) == Some("image") => {
            *v = serde_json::json!({"type": "text", "text": "[image]"});
            1
        }
        Value::Object(o) => o.values_mut().map(strip_value).sum(),
        Value::Array(a) => a.iter_mut().map(strip_value).sum(),
        _ => 0,
    }
}

impl Content {
    /// Replaces the images by the text `[image]`: image blocks of the message AND the images inside
    /// tool results (a screenshot a tool returned), however deeply nested. Returns how many.
    /// `tool_use` input is not touched (it is not content the model reads as an image).
    pub fn strip_images(&mut self) -> usize {
        let Content::Blocks(bs) = self else { return 0 };
        let mut n = 0;
        for b in bs {
            match b {
                Block::Image { .. } => {
                    *b = Block::Text {
                        text: "[image]".into(),
                    };
                    n += 1;
                }
                Block::ToolResult {
                    content: Some(c), ..
                } => n += strip_value(c),
                _ => {}
            }
        }
        n
    }
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct SystemBlock {
    #[serde(default, rename = "type", skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum System {
    Text(String),
    Blocks(Vec<SystemBlock>),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolDef {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub input_schema: Value,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThinkingConfig {
    pub budget_tokens: u32,
}

/// Provider-neutral request (field names follow the Node `req` object, hence camelCase).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Request {
    pub model: String,
    pub max_tokens: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system: Option<System>,
    pub messages: Vec<Message>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<ToolDef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f64>,
    /// Anthropic extended thinking. Suppresses `temperature`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking: Option<ThinkingConfig>,
    /// OpenAI `reasoning_effort`; set only for models that report effort tiers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<String>,
}

/// Token counts. Every field is optional: streams report them in pieces and a provider that
/// does not report a number leaves it `None` instead of a made-up zero.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Usage {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_creation_input_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_read_input_tokens: Option<u64>,
}

impl Usage {
    /// Fields reported by `later` replace ours (Anthropic usage in `message_delta` is cumulative).
    pub fn merge(&mut self, later: &Usage) {
        macro_rules! m {
            ($f:ident) => {
                if later.$f.is_some() {
                    self.$f = later.$f;
                }
            };
        }
        m!(input_tokens);
        m!(output_tokens);
        m!(cache_creation_input_tokens);
        m!(cache_read_input_tokens);
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(from = "String", into = "String")]
pub enum StopReason {
    EndTurn,
    ToolUse,
    MaxTokens,
    StopSequence,
    PauseTurn,
    Refusal,
    Other(String),
}

impl From<String> for StopReason {
    fn from(s: String) -> Self {
        match s.as_str() {
            "end_turn" => Self::EndTurn,
            "tool_use" => Self::ToolUse,
            "max_tokens" => Self::MaxTokens,
            "stop_sequence" => Self::StopSequence,
            "pause_turn" => Self::PauseTurn,
            "refusal" => Self::Refusal,
            _ => Self::Other(s),
        }
    }
}

impl From<StopReason> for String {
    fn from(r: StopReason) -> Self {
        match r {
            StopReason::EndTurn => "end_turn".into(),
            StopReason::ToolUse => "tool_use".into(),
            StopReason::MaxTokens => "max_tokens".into(),
            StopReason::StopSequence => "stop_sequence".into(),
            StopReason::PauseTurn => "pause_turn".into(),
            StopReason::Refusal => "refusal".into(),
            StopReason::Other(s) => s,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MessageStart {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default)]
    pub usage: Usage,
}

fn empty_object() -> Value {
    Value::Object(Map::new())
}

/// Block as announced by `content_block_start` (tool input always starts empty; the arguments
/// arrive as `input_json_delta`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum StartBlock {
    Text {
        text: String,
    },
    Thinking {
        thinking: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        signature: Option<String>,
    },
    RedactedThinking {
        data: String,
    },
    ToolUse {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
        #[serde(default = "empty_object")]
        input: Value,
    },
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Delta {
    TextDelta {
        text: String,
    },
    ThinkingDelta {
        thinking: String,
    },
    SignatureDelta {
        signature: String,
    },
    InputJsonDelta {
        partial_json: String,
    },
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct MessageDeltaBody {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop_reason: Option<StopReason>,
}

/// Normalized stream event (Anthropic streaming vocabulary).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum StreamEvent {
    MessageStart {
        message: MessageStart,
    },
    ContentBlockStart {
        index: usize,
        content_block: StartBlock,
    },
    ContentBlockDelta {
        index: usize,
        delta: Delta,
    },
    ContentBlockStop {
        index: usize,
    },
    MessageDelta {
        delta: MessageDeltaBody,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        usage: Option<Usage>,
    },
    MessageStop,
    Ping,
    #[serde(other)]
    Unknown,
}
