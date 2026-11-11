//! Error normalization. Messages are redacted at construction: configured secrets (API key,
//! token, proxy credentials) and key-shaped strings never reach `Display`/`Debug`/events.

use serde_json::Value;
use std::sync::Arc;
use std::time::Duration;
use synthesis_events::Redactor;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorKind {
    RateLimit,
    Auth,
    Overloaded,
    ContextLength,
    InvalidRequest,
    /// 5xx and other server-side failures.
    Server,
    Network,
    /// The provider answered 2xx but the stream/body is malformed or truncated.
    InvalidResponse,
    Cancelled,
}

impl ErrorKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::RateLimit => "rate_limit",
            Self::Auth => "auth",
            Self::Overloaded => "overloaded",
            Self::ContextLength => "context_length",
            Self::InvalidRequest => "invalid_request",
            Self::Server => "server",
            Self::Network => "network",
            Self::InvalidResponse => "invalid_response",
            Self::Cancelled => "cancelled",
        }
    }
}

#[derive(Clone)]
pub struct ProviderError {
    pub kind: ErrorKind,
    pub status: Option<u16>,
    pub retry_after: Option<Duration>,
    /// Raised after the response started streaming: the provider layer never replays these.
    pub mid_stream: bool,
    retryable: bool,
    message: String,
}

impl ProviderError {
    /// `message` must already be redacted (use [`Redaction`]) or contain no secrets.
    pub fn new(kind: ErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            status: None,
            retry_after: None,
            mid_stream: false,
            retryable: matches!(
                kind,
                ErrorKind::RateLimit
                    | ErrorKind::Overloaded
                    | ErrorKind::Server
                    | ErrorKind::Network
            ),
            message: message.into(),
        }
    }

    pub fn cancelled() -> Self {
        Self::new(ErrorKind::Cancelled, "request cancelled")
    }

    pub fn message(&self) -> &str {
        &self.message
    }

    /// True when the retry layer may send the request again (never after streaming started).
    pub fn retryable(&self) -> bool {
        self.retryable && !self.mid_stream
    }

    pub fn context_overflow(&self) -> bool {
        self.kind == ErrorKind::ContextLength
    }

    pub(crate) fn mid_stream(mut self) -> Self {
        self.mid_stream = true;
        self
    }
}

impl std::fmt::Display for ProviderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.status {
            Some(s) => write!(f, "{} ({s}): {}", self.kind.as_str(), self.message),
            None => write!(f, "{}: {}", self.kind.as_str(), self.message),
        }
    }
}

impl std::fmt::Debug for ProviderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "ProviderError({self})")
    }
}

impl std::error::Error for ProviderError {}

/// Secret scrubber shared by a provider client.
#[derive(Clone)]
pub struct Redaction(Arc<Redactor>);

impl Redaction {
    pub fn new(secrets: impl IntoIterator<Item = String>) -> Self {
        Self(Arc::new(Redactor::new(secrets)))
    }

    pub fn text(&self, s: &str) -> String {
        self.0.redact_str(s)
    }

    pub fn transport(&self, e: reqwest::Error) -> ProviderError {
        let kind = if e.is_builder() {
            ErrorKind::InvalidRequest
        } else {
            ErrorKind::Network
        };
        // Gateway down: host[:port] only (userinfo/query may carry credentials), a retry hint, and no
        // retries: waiting seconds for a refused connection helps nobody.
        if e.is_connect()
            && let Some(u) = e.url()
            && let Some(h) = u.host_str()
        {
            let mut root = String::new();
            let mut src = std::error::Error::source(&e);
            while let Some(s) = src {
                root = s.to_string();
                src = s.source();
            }
            let at = u.port().map_or(h.to_string(), |p| format!("{h}:{p}"));
            let mut err = ProviderError::new(
                kind,
                self.text(&format!(
                    "cannot reach {at} ({root}). Check that the gateway is running and the provider baseURL is right, then retry"
                )),
            );
            err.retryable = false;
            return err;
        }
        // `without_url`: the URL may carry credentials in userinfo or query.
        let e = e.without_url();
        let mut msg = e.to_string();
        let mut src = std::error::Error::source(&e);
        while let Some(s) = src {
            msg.push_str(": ");
            msg.push_str(&s.to_string());
            src = s.source();
        }
        ProviderError::new(kind, self.text(&msg))
    }

    /// Classifies a non-2xx response (or an in-stream `error` object when `status` is `None`).
    pub fn api_error(
        &self,
        status: Option<u16>,
        body: &str,
        retry_after: Option<Duration>,
    ) -> ProviderError {
        let json: Option<Value> = serde_json::from_str(body).ok();
        let err = json.as_ref().and_then(|j| j.get("error")).or(json.as_ref());
        let text = |k: &str| err.and_then(|e| e.get(k)).and_then(Value::as_str);
        let etype = text("type").or(text("code")).unwrap_or("");
        let message = match text("message") {
            Some(m) => m.to_string(),
            None => body.chars().take(500).collect(),
        };
        // Node isContextOverflow, plus the common "context window" phrasing.
        let hay = format!("{etype} {message}").to_lowercase();
        let overflow = [
            "prompt is too long",
            "context_length_exceeded",
            "maximum context length",
            "context window",
            // request-size rejections (Anthropic `request_too_large`, OpenAI `string_above_max_length`,
            // gateways); image-heavy histories hit these before the token limit
            "request_too_large",
            "request too large",
            "payload too large",
            "exceeds the maximum",
            "too many images",
            "image exceeds",
            "string too long",
        ]
        .iter()
        .any(|p| hay.contains(p))
            || status == Some(413);
        let kind = match (status, etype) {
            _ if overflow && status.is_none_or(|s| (400..500).contains(&s)) => {
                ErrorKind::ContextLength
            }
            (Some(401 | 403), _) | (_, "authentication_error" | "permission_error") => {
                ErrorKind::Auth
            }
            (Some(529), _) | (_, "overloaded_error") => ErrorKind::Overloaded,
            (Some(429), _) | (_, "rate_limit_error") => ErrorKind::RateLimit,
            (Some(s), _) if s >= 500 => ErrorKind::Server,
            (Some(408 | 409), _) | (_, "api_error") => ErrorKind::Server,
            _ => ErrorKind::InvalidRequest,
        };
        let mut e = ProviderError::new(kind, self.text(&message));
        e.status = status;
        e.retry_after = retry_after;
        // A spent quota will not recover by waiting.
        if etype == "insufficient_quota" {
            e.retryable = false;
        }
        e
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn size_rejections_are_context_overflow() {
        let r = Redaction::new([]);
        let kind = |status: Option<u16>, body: &str| r.api_error(status, body, None).kind;
        // shapes modelled on the documented Anthropic and OpenAI errors (not captured live)
        let anthropic_413 = r#"{"type":"error","error":{"type":"request_too_large","message":"Request exceeds the maximum allowed number of bytes."}}"#;
        let anthropic_img = r#"{"type":"error","error":{"type":"invalid_request_error","message":"messages.0.content.1.image.source.base64: image exceeds 5 MB maximum: 6291456 bytes > 5242880 bytes"}}"#;
        let openai = r#"{"error":{"message":"Invalid 'input[0].content[1].image_url': string too long. Expected a string with maximum length 20971520","type":"invalid_request_error","code":"string_above_max_length"}}"#;
        let openai_ctx = r#"{"error":{"message":"This model's maximum context length is 128000 tokens.","type":"invalid_request_error","code":"context_length_exceeded"}}"#;
        for (s, b) in [
            (413, anthropic_413),
            (400, anthropic_img),
            (400, openai),
            (400, openai_ctx),
            (413, "<html>Request Entity Too Large</html>"),
            (400, "too many images in request"),
        ] {
            assert_eq!(kind(Some(s), b), ErrorKind::ContextLength, "{s} {b}");
        }
        assert_eq!(
            kind(Some(400), r#"{"error":{"message":"bad field"}}"#),
            ErrorKind::InvalidRequest
        );
    }
}
