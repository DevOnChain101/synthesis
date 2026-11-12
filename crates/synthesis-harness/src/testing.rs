//! Test utilities exported for other crates: a scripted [`FakeProvider`] and an in-memory event
//! buffer. No network, no runtime beyond Tokio timers.

use std::collections::VecDeque;
use std::io::Write;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::future::BoxFuture;
use serde_json::Value;
use synthesis_config::Protocol;
use synthesis_events::Envelope;
use synthesis_providers::{
    Delta, ErrorKind, EventStream, MessageDeltaBody, MessageStart, ModelInfo, ModelProvider,
    ProviderCapabilities, ProviderError, Request, StartBlock, StopReason, StreamEvent, Usage,
};
use tokio_util::sync::CancellationToken;

#[derive(Debug, Clone)]
pub enum Step {
    Event(StreamEvent),
    Delay(Duration),
    /// Waits until the file exists (cancellable): a test opens it when the world is ready.
    Gate(std::path::PathBuf),
    Fail(ProviderError),
}

/// One scripted provider response (one `stream` call).
#[derive(Debug, Clone, Default)]
pub struct Turn {
    pub steps: Vec<Step>,
}

fn ev(e: StreamEvent) -> Step {
    Step::Event(e)
}

impl Turn {
    fn begin() -> Vec<Step> {
        vec![ev(StreamEvent::MessageStart {
            message: MessageStart {
                id: Some("msg_fake".into()),
                model: Some("fake".into()),
                usage: Usage {
                    input_tokens: Some(10),
                    output_tokens: Some(1),
                    ..Usage::default()
                },
            },
        })]
    }

    fn end(steps: &mut Vec<Step>, reason: StopReason) {
        steps.push(ev(StreamEvent::MessageDelta {
            delta: MessageDeltaBody {
                stop_reason: Some(reason),
            },
            usage: Some(Usage {
                output_tokens: Some(5),
                ..Usage::default()
            }),
        }));
        steps.push(ev(StreamEvent::MessageStop));
    }

    fn text_block(steps: &mut Vec<Step>, index: usize, text: &str) {
        steps.push(ev(StreamEvent::ContentBlockStart {
            index,
            content_block: StartBlock::Text {
                text: String::new(),
            },
        }));
        // Two deltas so streaming consumers see more than one.
        let mid = text
            .char_indices()
            .nth(text.chars().count() / 2)
            .map_or(0, |(i, _)| i);
        for part in [&text[..mid], &text[mid..]] {
            steps.push(ev(StreamEvent::ContentBlockDelta {
                index,
                delta: Delta::TextDelta { text: part.into() },
            }));
        }
        steps.push(ev(StreamEvent::ContentBlockStop { index }));
    }

    fn tool_block(
        steps: &mut Vec<Step>,
        index: usize,
        id: &str,
        name: &str,
        json: &str,
        stop: bool,
    ) {
        steps.push(ev(StreamEvent::ContentBlockStart {
            index,
            content_block: StartBlock::ToolUse {
                id: Some(id.into()),
                name: Some(name.into()),
                input: Value::Object(Default::default()),
            },
        }));
        steps.push(ev(StreamEvent::ContentBlockDelta {
            index,
            delta: Delta::InputJsonDelta {
                partial_json: json.into(),
            },
        }));
        if stop {
            steps.push(ev(StreamEvent::ContentBlockStop { index }));
        }
    }

    /// Plain assistant text, `end_turn`.
    pub fn text(text: &str) -> Self {
        let mut steps = Self::begin();
        Self::text_block(&mut steps, 0, text);
        Self::end(&mut steps, StopReason::EndTurn);
        Self { steps }
    }

    /// One tool call, `tool_use`.
    pub fn tool(id: &str, name: &str, input: Value) -> Self {
        Self::tools(&[(id, name, input)])
    }

    /// Several tool calls in one assistant message.
    pub fn tools(calls: &[(&str, &str, Value)]) -> Self {
        Self::text_and_tools("", calls)
    }

    pub fn text_and_tools(text: &str, calls: &[(&str, &str, Value)]) -> Self {
        let mut steps = Self::begin();
        let mut i = 0;
        if !text.is_empty() {
            Self::text_block(&mut steps, 0, text);
            i = 1;
        }
        for (k, (id, name, input)) in calls.iter().enumerate() {
            Self::tool_block(&mut steps, i + k, id, name, &input.to_string(), true);
        }
        Self::end(&mut steps, StopReason::ToolUse);
        Self { steps }
    }

    /// Output cut off at `max_tokens` in the middle of a tool call: no block stop, partial JSON.
    pub fn truncated_tool(id: &str, name: &str, partial_json: &str) -> Self {
        let mut steps = Self::begin();
        Self::tool_block(&mut steps, 0, id, name, partial_json, false);
        Self::end(&mut steps, StopReason::MaxTokens);
        Self { steps }
    }

    /// A finished tool block whose arguments are not valid JSON.
    pub fn malformed_tool(id: &str, name: &str, raw_json: &str) -> Self {
        let mut steps = Self::begin();
        Self::tool_block(&mut steps, 0, id, name, raw_json, true);
        Self::end(&mut steps, StopReason::ToolUse);
        Self { steps }
    }

    /// The request fails before any byte arrives.
    pub fn error(e: ProviderError) -> Self {
        Self {
            steps: vec![Step::Fail(e)],
        }
    }

    pub fn context_overflow() -> Self {
        Self::error(ProviderError::new(
            ErrorKind::ContextLength,
            "prompt is too long",
        ))
    }

    /// Replaces the end of the stream with an error (mid-stream failure).
    pub fn then_error(mut self, e: ProviderError) -> Self {
        while matches!(
            self.steps.last(),
            Some(Step::Event(
                StreamEvent::MessageStop | StreamEvent::MessageDelta { .. }
            ))
        ) {
            self.steps.pop();
        }
        self.steps.push(Step::Fail(e));
        self
    }

    /// Waits `ms` before the first event (slow model; cancellable).
    pub fn delayed(mut self, ms: u64) -> Self {
        self.steps.insert(0, Step::Delay(Duration::from_millis(ms)));
        self
    }

    /// Waits until `path` exists before the first event.
    pub fn delayed_until(mut self, path: &std::path::Path) -> Self {
        self.steps.insert(0, Step::Gate(path.to_path_buf()));
        self
    }

    /// Waits `ms` after the first event, i.e. mid-stream.
    pub fn delayed_mid_stream(mut self, ms: u64) -> Self {
        self.steps.insert(1, Step::Delay(Duration::from_millis(ms)));
        self
    }

    /// A provider that sends no usage at all.
    pub fn without_usage(mut self) -> Self {
        for s in &mut self.steps {
            if let Step::Event(StreamEvent::MessageStart { message }) = s {
                message.usage = Usage::default();
            }
            if let Step::Event(StreamEvent::MessageDelta { usage, .. }) = s {
                *usage = None;
            }
        }
        self
    }

    /// Cache tokens of the response (on top of `with_usage`).
    pub fn with_cache(mut self, read: u64, create: u64) -> Self {
        for s in &mut self.steps {
            if let Step::Event(StreamEvent::MessageStart { message }) = s {
                message.usage.cache_read_input_tokens = Some(read);
                message.usage.cache_creation_input_tokens = Some(create);
            }
        }
        self
    }

    pub fn with_usage(mut self, input: u64, output: u64) -> Self {
        for s in &mut self.steps {
            if let Step::Event(StreamEvent::MessageStart { message }) = s {
                message.usage.input_tokens = Some(input);
            }
            if let Step::Event(StreamEvent::MessageDelta { usage: Some(u), .. }) = s {
                u.output_tokens = Some(output);
            }
        }
        self
    }
}

/// Plays scripted turns in order and records every request it receives.
pub struct FakeProvider {
    caps: ProviderCapabilities,
    script: Mutex<VecDeque<Turn>>,
    requests: Mutex<Vec<Request>>,
    responder: Option<Responder>,
}

/// Chooses the answer from the request (for tests with several agents asking in any order);
/// `None` falls back to the script.
pub type Responder = Arc<dyn Fn(&Request) -> Option<Turn> + Send + Sync>;

impl FakeProvider {
    pub fn new(turns: impl IntoIterator<Item = Turn>) -> Self {
        Self {
            caps: ProviderCapabilities {
                // a known window: an unknown one adds a notice to every run
                context_window: Some(200_000),
                ..ProviderCapabilities::defaults(Protocol::Anthropic)
            },
            script: Mutex::new(turns.into_iter().collect()),
            requests: Mutex::default(),
            responder: None,
        }
    }

    pub fn with_responder(mut self, f: Responder) -> Self {
        self.responder = Some(f);
        self
    }

    pub fn with_caps(mut self, caps: ProviderCapabilities) -> Self {
        self.caps = caps;
        self
    }

    pub fn requests(&self) -> Vec<Request> {
        self.requests
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    pub fn remaining(&self) -> usize {
        self.script.lock().unwrap_or_else(|e| e.into_inner()).len()
    }
}

impl ModelProvider for FakeProvider {
    fn id(&self) -> &str {
        "fake"
    }

    fn protocol(&self) -> Protocol {
        Protocol::Anthropic
    }

    fn capabilities(&self, _model: &str) -> ProviderCapabilities {
        self.caps.clone()
    }

    fn stream(&self, req: &Request, cancel: CancellationToken) -> EventStream {
        self.requests
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(req.clone());
        let turn = self.responder.as_ref().and_then(|f| f(req)).or_else(|| {
            self.script
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .pop_front()
        });
        let steps: VecDeque<Step> = match turn {
            Some(t) => t.steps.into(),
            None => [Step::Fail(ProviderError::new(
                ErrorKind::InvalidRequest,
                "FakeProvider script exhausted",
            ))]
            .into(),
        };
        Box::pin(futures_util::stream::unfold(
            (steps, cancel),
            |(mut steps, cancel)| async move {
                match steps.pop_front()? {
                    Step::Event(e) => Some((Ok(e), (steps, cancel))),
                    Step::Fail(e) => Some((Err(e), (VecDeque::new(), cancel))),
                    wait @ (Step::Delay(_) | Step::Gate(_)) => {
                        let waited = async {
                            match wait {
                                Step::Delay(d) => tokio::time::sleep(d).await,
                                Step::Gate(p) => {
                                    while !p.exists() {
                                        tokio::time::sleep(Duration::from_millis(20)).await;
                                    }
                                }
                                _ => {}
                            }
                        };
                        tokio::select! {
                            () = waited => {}
                            () = cancel.cancelled() => {
                                return Some((Err(ProviderError::cancelled()), (VecDeque::new(), cancel)));
                            }
                        }
                        // Next step, without yielding an item for the delay itself.
                        match steps.pop_front()? {
                            Step::Event(e) => Some((Ok(e), (steps, cancel))),
                            Step::Fail(e) => Some((Err(e), (VecDeque::new(), cancel))),
                            Step::Delay(_) | Step::Gate(_) => None,
                        }
                    }
                }
            },
        ))
    }

    fn list_models<'a>(
        &'a self,
        _cancel: &'a CancellationToken,
    ) -> BoxFuture<'a, Result<Vec<ModelInfo>, ProviderError>> {
        Box::pin(async { Ok(vec![]) })
    }
}

/// In-memory JSONL buffer usable as the event sink output; clone it to read what was written.
#[derive(Clone, Default)]
pub struct SharedBuf(Arc<Mutex<Vec<u8>>>);

impl Write for SharedBuf {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl SharedBuf {
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().unwrap_or_else(|e| e.into_inner())).into_owned()
    }

    /// Every line parsed as an envelope; panics on a malformed line (tests only).
    pub fn envelopes(&self) -> Vec<Envelope> {
        self.text()
            .lines()
            .map(|l| serde_json::from_str(l).expect("event line is a valid envelope"))
            .collect()
    }
}
