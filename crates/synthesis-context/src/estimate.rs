use serde_json::Value;
use synthesis_providers::{Block, Content, ImageSource, Message};

/// Pluggable token estimator. The default is the Node approximation (chars / 4). `Send + Sync`
/// so compaction futures that hold `&dyn TokenEstimator` can cross `.await` on any runtime.
pub trait TokenEstimator: Send + Sync {
    fn estimate(&self, text: &str) -> u64;
}

#[derive(Debug, Clone, Copy)]
pub struct CharsPerToken(pub u64);

impl Default for CharsPerToken {
    fn default() -> Self {
        Self(4)
    }
}

impl TokenEstimator for CharsPerToken {
    fn estimate(&self, text: &str) -> u64 {
        (text.chars().count() as u64).div_ceil(self.0.max(1))
    }
}

/// Floor cost of an image block (base64 payloads must not be counted as text).
pub const IMAGE_TOKENS: u64 = 1500;
/// Ceiling per image: providers downscale, so a huge file does not cost unbounded tokens.
const IMAGE_TOKENS_MAX: u64 = 8000;

/// Decoded size of a base64 image (0 for URL images, which carry no payload).
fn image_len(b: &Block) -> u64 {
    match b {
        Block::Image {
            source: ImageSource::Base64 { data, .. },
        } => data.len() as u64 / 4 * 3,
        _ => 0,
    }
}

/// Conservative, not exact: ~1 token per 600 bytes of the file, between the floor and the cap, so
/// that many or large screenshots make auto-compaction fire early.
fn image_tokens(b: &Block) -> u64 {
    (image_len(b) / 600).clamp(IMAGE_TOKENS, IMAGE_TOKENS_MAX)
}

/// `(count, decoded bytes)` of the top-level image blocks, for size checks and messages.
pub fn image_stats(ms: &[Message]) -> (usize, u64) {
    let bs = ms.iter().filter_map(|m| match &m.content {
        Content::Blocks(bs) => Some(bs),
        Content::Text(_) => None,
    });
    bs.flatten()
        .filter(|b| matches!(b, Block::Image { .. }))
        .fold((0, 0), |(n, l), b| (n + 1, l + image_len(b)))
}

pub fn block_tokens(est: &dyn TokenEstimator, b: &Block) -> u64 {
    match b {
        Block::Text { text } => est.estimate(text),
        Block::Image { .. } => image_tokens(b),
        Block::ToolUse { name, input, .. } => {
            est.estimate(name) + input.as_ref().map_or(0, |v| est.estimate(&v.to_string()))
        }
        Block::ToolResult { content, .. } => match content {
            None => 0,
            Some(Value::String(s)) => est.estimate(s),
            Some(v) => est.estimate(&v.to_string()),
        },
        Block::Thinking { thinking, .. } => est.estimate(thinking),
        Block::RedactedThinking { data } => est.estimate(data),
    }
}

pub fn message_tokens(est: &dyn TokenEstimator, m: &Message) -> u64 {
    match &m.content {
        Content::Text(t) => est.estimate(t),
        Content::Blocks(bs) => bs.iter().map(|b| block_tokens(est, b)).sum(),
    }
}

pub fn messages_tokens(est: &dyn TokenEstimator, ms: &[Message]) -> u64 {
    ms.iter().map(|m| message_tokens(est, m)).sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn img(len: usize) -> Block {
        Block::Image {
            source: ImageSource::Base64 {
                media_type: "image/png".into(),
                data: "A".repeat(len),
            },
        }
    }

    #[test]
    fn images_cost_by_size_between_floor_and_cap() {
        let est = CharsPerToken::default();
        assert_eq!(block_tokens(&est, &img(100)), IMAGE_TOKENS);
        let mid = block_tokens(&est, &img(2_000_000)); // 1.5 MB file
        assert!(mid > IMAGE_TOKENS && mid < IMAGE_TOKENS_MAX, "{mid}");
        assert_eq!(block_tokens(&est, &img(8_000_000)), IMAGE_TOKENS_MAX);
        let m = Message {
            role: synthesis_providers::Role::User,
            content: Content::Blocks(vec![img(400), img(800)]),
        };
        assert_eq!(image_stats(&[m]), (2, 300 + 600));
    }
}
