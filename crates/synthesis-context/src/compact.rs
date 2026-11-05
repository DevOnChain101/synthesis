//! Compaction (port of compact.js + agent.js autocompact). The summary replaces only the head of
//! the history; the tail keeps raw recent evidence, durable `TaskState` lives outside history, and
//! the result is always pairing-valid. Failure never touches the input and never loops.

use crate::{TaskState, TokenEstimator, message_tokens, messages_tokens, omit_old_images, pairing};
use std::future::Future;
use synthesis_providers::{Block, Content, Message, Role};

pub const SUMMARY_SYSTEM_PROMPT: &str = "Summarize the conversation so far for a coding assistant that is about to lose this context. Preserve: the user's overall goal, decisions made, files touched and why, commands run and their outcomes, open TODOs, and anything the assistant still needs to do. Be concise but keep anything a continuation would need. Plain text, no markdown headers.";
pub const SUMMARY_PREFIX: &str = "Summary of the conversation so far: ";

/// Prefix of a `SummarizeError` whose request was too large; `compact` then retries on less input.
pub const OVERFLOW_MARK: &str = "context overflow: ";
/// Attempts at the summary when the request is too large; each drops the older half of what is left.
const OVERFLOW_RETRIES: usize = 5;
/// Images kept in the verbatim tail: only the newest message that has any, and only within this.
const TAIL_IMAGE_BYTES: u64 = 4 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SummarizeError(pub String);

impl std::fmt::Display for SummarizeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "summarizer failed: {}", self.0)
    }
}
impl std::error::Error for SummarizeError {}

/// One-shot, tool-less model call. Implemented by the harness over a provider; tests use a fake.
pub trait Summarizer {
    fn summarize(
        &self,
        system: &str,
        prompt: &str,
    ) -> impl Future<Output = Result<String, SummarizeError>> + Send;
}

#[derive(Debug, Clone, Copy)]
pub enum Threshold {
    /// 80% of the context window (Node `auto`).
    Auto,
    Tokens(u64),
}

#[derive(Debug, Clone)]
pub struct CompactConfig {
    pub threshold: Threshold,
    /// Below this many message tokens compaction is never worth it (Node MIN_AUTOCOMPACT_TOKENS).
    pub min_tokens: u64,
    /// Newest messages kept verbatim (extended backwards to a safe cut).
    pub keep_recent_tokens: u64,
}

impl Default for CompactConfig {
    fn default() -> Self {
        Self {
            threshold: Threshold::Auto,
            min_tokens: 1000,
            keep_recent_tokens: 4000,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CompactError {
    Nothing,
    Summarizer(SummarizeError),
    EmptySummary,
    /// The result would not be smaller; rejected so the caller cannot loop on it.
    NoProgress {
        before: u64,
        after: u64,
    },
}

fn is_summary_only(ms: &[Message]) -> bool {
    matches!(ms, [Message { content: Content::Text(t), .. }] if t.starts_with(SUMMARY_PREFIX))
        || matches!(ms, [Message { content: Content::Blocks(b), .. }] if matches!(b.first(), Some(Block::Text { text }) if text.starts_with(SUMMARY_PREFIX)))
}

/// Node rule: the whole context (fixed part + messages) reaches the threshold, the threshold is at
/// least `fixed + min_tokens` (shrinking messages must be able to get below it), and there are
/// at least `min_tokens` of messages that are not already just a summary.
pub fn should_compact(
    est: &dyn TokenEstimator,
    messages: &[Message],
    fixed_tokens: u64,
    window: u64,
    cfg: &CompactConfig,
) -> bool {
    let msgs = messages_tokens(est, messages);
    if messages.is_empty() || is_summary_only(messages) || msgs < cfg.min_tokens {
        return false;
    }
    let threshold = match cfg.threshold {
        Threshold::Auto => window / 10 * 8,
        Threshold::Tokens(t) => t,
    };
    fixed_tokens + msgs >= threshold.max(fixed_tokens + cfg.min_tokens)
}

fn clip(s: &str, max: usize) -> String {
    let n = s.chars().count();
    if n <= max {
        return s.to_string();
    }
    let h = max / 2;
    let head: String = s.chars().take(h).collect();
    let tail: String = s.chars().skip(n - h).collect();
    format!("{head}\n[... {} chars omitted ...]\n{tail}", n - 2 * h)
}

fn render_block(b: &Block) -> Option<String> {
    Some(match b {
        Block::Text { text } => text.clone(),
        Block::ToolUse { name, input, .. } => format!(
            "[tool_use {name} {}]",
            input.as_ref().map_or(String::new(), |v| v.to_string())
        ),
        Block::ToolResult {
            content, is_error, ..
        } => {
            let c = match content {
                Some(serde_json::Value::String(s)) => s.clone(),
                Some(v) => v.to_string(),
                None => String::new(),
            };
            format!(
                "[tool_result{} {c}]",
                if *is_error == Some(true) {
                    " error"
                } else {
                    ""
                }
            )
        }
        Block::Image { .. } => "[image]".into(),
        Block::Thinking { .. } | Block::RedactedThinking { .. } => return None,
    })
}

const BLOCK_CAP: usize = 2_000;
const PROMPT_CAP: usize = 200_000;

pub fn build_summary_prompt(messages: &[Message], state: &TaskState, instructions: &str) -> String {
    let rendered: Vec<String> = messages
        .iter()
        .map(|m| {
            let text = match &m.content {
                Content::Text(t) => clip(t, BLOCK_CAP * 4),
                Content::Blocks(bs) => bs
                    .iter()
                    .filter_map(render_block)
                    .map(|s| clip(&s, BLOCK_CAP))
                    .collect::<Vec<_>>()
                    .join("\n"),
            };
            format!("{:?}: {text}", m.role).to_lowercase()
        })
        .collect();
    let kept = state
        .render()
        .map(|s| format!("\n\nDurable task state (kept separately, do not repeat it):\n{s}"))
        .unwrap_or_default();
    let extra = if instructions.is_empty() {
        String::new()
    } else {
        format!("\n\nAdditional instructions for this summary: {instructions}")
    };
    format!(
        "Conversation to summarize:\n\n{}{kept}{extra}",
        clip(&rendered.join("\n\n"), PROMPT_CAP)
    )
}

/// Summarises the head of `messages` and returns `[summary, ..tail]`. On any error the caller
/// still owns the untouched input.
pub async fn compact(
    est: &dyn TokenEstimator,
    summarizer: &impl Summarizer,
    messages: &[Message],
    state: &TaskState,
    instructions: &str,
    cfg: &CompactConfig,
) -> Result<Vec<Message>, CompactError> {
    if messages.is_empty() || is_summary_only(messages) {
        return Err(CompactError::Nothing);
    }
    // the newest message is always kept verbatim, however large (it is what the person just sent)
    let (mut want, mut sum) = (messages.len() - 1, 0);
    for (i, m) in messages.iter().enumerate().rev() {
        sum += message_tokens(est, m);
        if sum > cfg.keep_recent_tokens {
            break;
        }
        want = i;
    }
    let cut = pairing::safe_cut(messages, want);
    let mut skip = 0;
    let mut attempt = 0;
    let summary = loop {
        let prompt = build_summary_prompt(&messages[skip..cut], state, instructions);
        match summarizer.summarize(SUMMARY_SYSTEM_PROMPT, &prompt).await {
            Ok(s) => break s,
            // Codex-style: too large, so drop the oldest part of the input and ask again
            Err(e)
                if e.0.starts_with(OVERFLOW_MARK)
                    && attempt < OVERFLOW_RETRIES
                    && skip + 1 < cut =>
            {
                attempt += 1;
                skip += (cut - skip).div_ceil(2);
            }
            Err(e) => return Err(CompactError::Summarizer(e)),
        }
    };
    let summary = summary.trim();
    if summary.is_empty() {
        return Err(CompactError::EmptySummary);
    }
    let text = format!("{SUMMARY_PREFIX}{summary}");
    let mut out: Vec<Message> = messages[cut..].to_vec();
    omit_old_images(&mut out, 1, TAIL_IMAGE_BYTES);
    match out.first_mut() {
        Some(first) if first.role == Role::User && !pairing::has_tool_result(first) => {
            let mut bs = match std::mem::replace(&mut first.content, Content::Blocks(vec![])) {
                Content::Blocks(b) => b,
                Content::Text(t) => vec![Block::Text { text: t }],
            };
            bs.insert(0, Block::Text { text });
            first.content = Content::Blocks(bs);
        }
        _ => out.insert(
            0,
            Message {
                role: Role::User,
                content: Content::Text(text),
            },
        ),
    }
    let out = pairing::repair_pairing(out);
    let (before, after) = (messages_tokens(est, messages), messages_tokens(est, &out));
    if after >= before {
        return Err(CompactError::NoProgress { before, after });
    }
    Ok(out)
}

/// Auto-compaction driver. After `max_failures` consecutive failures it stops trying by itself
/// (a broken summarizer must not be called again every turn); `manual` always tries.
pub struct Compactor<S> {
    pub summarizer: S,
    pub cfg: CompactConfig,
    pub max_failures: u32,
    failures: u32,
}

impl<S: Summarizer> Compactor<S> {
    pub fn new(summarizer: S, cfg: CompactConfig) -> Self {
        Self {
            summarizer,
            cfg,
            max_failures: 2,
            failures: 0,
        }
    }

    pub fn failures(&self) -> u32 {
        self.failures
    }

    /// `None`: not needed, or suppressed after repeated failures.
    pub async fn auto(
        &mut self,
        est: &dyn TokenEstimator,
        messages: &[Message],
        state: &TaskState,
        fixed_tokens: u64,
        window: u64,
    ) -> Option<Result<Vec<Message>, CompactError>> {
        if self.failures >= self.max_failures
            || !should_compact(est, messages, fixed_tokens, window, &self.cfg)
        {
            return None;
        }
        Some(self.manual(est, messages, state, "").await)
    }

    pub async fn manual(
        &mut self,
        est: &dyn TokenEstimator,
        messages: &[Message],
        state: &TaskState,
        instructions: &str,
    ) -> Result<Vec<Message>, CompactError> {
        let r = compact(
            est,
            &self.summarizer,
            messages,
            state,
            instructions,
            &self.cfg,
        )
        .await;
        match &r {
            Ok(_) => self.failures = 0,
            Err(CompactError::Nothing) => {}
            Err(_) => self.failures += 1,
        }
        r
    }
}
