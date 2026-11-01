//! Thinking block: a dim header (`✻ Thinking · 4.2s`) and the whole text behind a left gutter bar.
//! The gutter, not italics, carries the "this is reasoning" signal, so the block reads the same
//! without colour. Layout rules (one blank line between blocks, none at the end) live in
//! `item_rows` / `render_agent`; the block itself starts with its blank separator row.

use super::*;

/// `4.2s`, `1m 05s`.
pub(super) fn fmt_secs(ms: i64) -> String {
    let ms = ms.max(0);
    if ms < 60_000 {
        format!("{:.1}s", ms as f64 / 1000.0)
    } else {
        format!("{}m {:02}s", ms / 60_000, ms / 1000 % 60)
    }
}

/// The header's duration: while streaming the live elapsed time (injected clock); once finished
/// the span between the first and the last thinking delta event (`it.ms`), else start..end.
fn duration(it: &Item, now: i64) -> Option<i64> {
    let start = it.start.filter(|s| *s != 0);
    if it.streaming {
        return start.map(|s| now - s);
    }
    if it.ms > 0 {
        return Some(it.ms);
    }
    Some(it.end.filter(|e| *e != 0)? - start?)
}

pub(super) fn rows(it: &Item, w: usize, frame: usize, now: i64) -> Vec<Row> {
    let text = it.text.replace('\r', "");
    let text = text.trim_matches(['\n', ' ', '\t']);
    // whitespace-only thinking is not a block
    if text.is_empty() {
        return vec![];
    }
    let mut head = vec![
        if it.streaming {
            r(format!("{} ", spin(frame)), C_LIGHT)
        } else {
            r("✻ ", C_DIM)
        },
        r("Thinking", C_DIM),
    ];
    if let Some(ms) = duration(it, now) {
        head.push(r(format!(" · {}", fmt_secs(ms)), C_DIM));
    }
    let mut rows: Vec<Row> = vec![vec![], head];
    let gutter = [deco("│ ", C_SUBTLE)];
    for ln in text.split('\n') {
        rows.extend(wrap(
            &[run_full(
                ln.replace('\t', "  "),
                C_SUBTLE,
                false,
                true,
                false,
            )],
            w,
            &gutter,
            &gutter,
        ));
    }
    rows
}
