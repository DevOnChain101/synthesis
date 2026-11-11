use synthesis_providers::{Block, Content, ImageSource, Message};

/// Request-side image policy: base64 images stay only in the newest `keep` messages that carry
/// them and while their total size is within `budget` bytes (newest first); every other image
/// becomes a text placeholder. Works on a copy of the history, which the caller keeps intact.
/// Returns `(images replaced, their decoded bytes)`.
/// shortcut: only top-level image blocks, not screenshots nested in tool results; extend if those
/// turn out to dominate request size.
pub fn omit_old_images(messages: &mut [Message], keep: usize, budget: u64) -> (usize, u64) {
    let (mut kept, mut used, mut n_out, mut bytes_out) = (0, 0u64, 0, 0u64);
    for m in messages.iter_mut().rev() {
        let Content::Blocks(bs) = &mut m.content else {
            continue;
        };
        let len = |b: &Block| match b {
            Block::Image {
                source: ImageSource::Base64 { data, .. },
            } => Some(data.len() as u64 / 4 * 3),
            _ => None,
        };
        let total: u64 = bs.iter().filter_map(len).sum();
        if total == 0 {
            continue;
        }
        if kept < keep && used + total <= budget {
            kept += 1;
            used += total;
            continue;
        }
        let mut i = 0;
        for b in bs.iter_mut() {
            if let Some(l) = len(b) {
                i += 1;
                *b = Block::Text {
                    text: format!("[image: {i}, removed to save space]"),
                };
                n_out += 1;
                bytes_out += l;
            }
        }
    }
    (n_out, bytes_out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use synthesis_providers::Role;

    fn msg(n_images: usize, len: usize) -> Message {
        let img = || Block::Image {
            source: ImageSource::Base64 {
                media_type: "image/png".into(),
                data: "A".repeat(len),
            },
        };
        Message {
            role: Role::User,
            content: Content::Blocks(
                std::iter::once(Block::Text { text: "hi".into() })
                    .chain((0..n_images).map(|_| img()))
                    .collect(),
            ),
        }
    }

    fn images(m: &Message) -> usize {
        match &m.content {
            Content::Blocks(b) => b
                .iter()
                .filter(|b| matches!(b, Block::Image { .. }))
                .count(),
            Content::Text(_) => 0,
        }
    }

    #[test]
    fn keeps_the_newest_within_count_and_budget() {
        let mut ms = vec![msg(1, 400), msg(1, 400), msg(1, 400)];
        assert_eq!(omit_old_images(&mut ms, 2, 10_000), (1, 300));
        assert_eq!(ms.iter().map(images).collect::<Vec<_>>(), [0, 1, 1]);
        let mut ms = vec![msg(1, 400), msg(3, 400)]; // newest alone is over budget
        assert_eq!(omit_old_images(&mut ms, 2, 600), (3, 900));
        assert_eq!(ms.iter().map(images).collect::<Vec<_>>(), [1, 0]);
        assert!(
            matches!(&ms[1].content, Content::Blocks(b) if b.len() == 4 && matches!(&b[3], Block::Text { text } if text == "[image: 3, removed to save space]"))
        );
    }
}
