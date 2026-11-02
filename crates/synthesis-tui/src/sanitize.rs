//! Everything that came from a model, a tool or a repo file is data. Before it reaches the
//! terminal, escape sequences and control characters are made visible, never dropped
//! (`synthesis_harness::sanitize_for_display`), line structure kept where the pane lays out lines.

use serde_json::Value;
use synthesis_harness::{is_invisible_or_blank, sanitize_for_display};

/// Multi-line text: each line sanitized, newlines kept.
pub fn clean_text(s: &str) -> String {
    let odd = |c: char| c != '\n' && is_invisible_or_blank(c);
    if !s.chars().any(odd) {
        return s.to_string();
    }
    s.replace("\r\n", "\n")
        .split('\n')
        .map(|l| sanitize_for_display(&l.replace('\t', "  ")))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Single-line display (approval prompts, commands): newlines become visible `\n`.
pub fn clean_line(s: &str) -> String {
    sanitize_for_display(s)
}

/// Every string in a tool input, sanitized to one visible line, so a hidden tail of a command
/// cannot sit past a newline or behind an escape sequence in the approval dialog.
pub fn clean_input(v: &Value) -> Value {
    match v {
        Value::String(s) => Value::String(clean_line(s)),
        Value::Array(a) => Value::Array(a.iter().map(clean_input).collect()),
        Value::Object(m) => {
            Value::Object(m.iter().map(|(k, v)| (k.clone(), clean_input(v))).collect())
        }
        o => o.clone(),
    }
}

/// Like [`clean_input`] but keeps line structure (the pane draws edit/write bodies as lines);
/// keys are sanitized too.
pub fn clean_input_lines(v: &Value) -> Value {
    match v {
        Value::String(s) => Value::String(clean_text(s)),
        Value::Array(a) => Value::Array(a.iter().map(clean_input_lines).collect()),
        Value::Object(m) => Value::Object(
            m.iter()
                .map(|(k, v)| (clean_line(k), clean_input_lines(v)))
                .collect(),
        ),
        o => o.clone(),
    }
}

/// `Item { field: value, .. }`: the renderer's `Item` has a private cache field, so struct
/// update syntax is not available outside its crate.
macro_rules! item {
    (@set $it:ident;) => {};
    (@set $it:ident; $f:ident : $v:expr, $($rest:tt)*) => { $it.$f = $v; item!(@set $it; $($rest)*); };
    (@set $it:ident; $f:ident : $v:expr) => { $it.$f = $v; };
    (@set $it:ident; $f:ident, $($rest:tt)*) => { $it.$f = $f; item!(@set $it; $($rest)*); };
    (@set $it:ident; $f:ident) => { $it.$f = $f; };
    ($($body:tt)*) => {{
        let mut it = ::synthesis_tui_render::agent::Item::default();
        item!(@set it; $($body)*);
        it
    }};
}
