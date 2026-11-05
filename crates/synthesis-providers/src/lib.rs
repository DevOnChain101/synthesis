//! Model providers (R3): thin clients on `reqwest` for the Anthropic Messages API and
//! OpenAI-compatible Chat Completions (no vendor SDK), normalized stream events, one retry layer,
//! redacted error normalization and a capability registry. Ports Node `src/native/providers/*`
//! and `net.js`; golden cases in `tests/golden/providers`.
//!
//! Environment: nothing here reads the process environment. Callers pass an [`synthesis_config::Env`]
//! (proxy `HTTP(S)_PROXY`/`ALL_PROXY`/`NO_PROXY`, CA `SSL_CERT_FILE`/`SSL_CERT_DIR`) and a
//! credential already resolved by `synthesis_config::resolve_credential`.

pub mod accumulator;
pub mod anthropic;
pub mod capabilities;
pub mod error;
pub mod http;
pub mod modelinfo;
pub mod openai;
pub mod provider;
pub mod retry;
pub mod sse;
pub mod types;

pub use accumulator::{AssistantMessage, InvalidToolInput, MessageBuilder};
pub use capabilities::{CapabilityRegistry, ProviderCapabilities};
pub use error::{ErrorKind, ProviderError, Redaction};
pub use http::{EventStream, NetSettings, build_client};
pub use provider::{
    Auth, ModelInfo, ModelProvider, ProviderSpec, Secret, anthropic_uses_bearer, normalize_model,
};
pub use retry::{RetryNotice, RetryPolicy};
pub use sse::{SseEvent, SseParser};
pub use types::*;
