//! JS-compatible string helpers shared by the models (moved from `synthesis-tui-render::util`,
//! which re-exports them).

/// `user.message.display_text` of the harness's own notes to the model (empty, verification or
/// stop-hook feedback): not something the person said, so never a prompt, title or chat row.
pub fn is_harness_note(display_text: Option<&str>) -> bool {
    matches!(
        display_text,
        Some("" | "verification feedback" | "stop hook feedback")
    )
}

pub fn u16len(s: &str) -> usize {
    s.chars().map(char::len_utf16).sum()
}

/// `s.slice(0, n)` counted in UTF-16 units; a cut inside a surrogate pair drops the pair.
pub fn slice_u16(s: &str, n: usize) -> &str {
    let mut used = 0;
    for (i, c) in s.char_indices() {
        let l = c.len_utf16();
        if used + l > n {
            return &s[..i];
        }
        used += l;
    }
    s
}

/// Collapse every run of whitespace to one space and trim (`s.replace(/\s+/g,' ').trim()`).
pub fn one_line(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for w in s
        .split(synthesis_tui_base::text::is_js_space)
        .filter(|w| !w.is_empty())
    {
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(w);
    }
    out
}

/// parser.js `short`.
pub fn short(text: &str, max: usize) -> String {
    let s = one_line(text);
    if u16len(&s) > max {
        format!("{}…", slice_u16(&s, max.saturating_sub(1)))
    } else {
        s
    }
}

/// `String(p).split(/[\\/]/).filter(Boolean).pop() || ''`.
pub fn basename(p: &str) -> String {
    p.split(['\\', '/'])
        .rfind(|s| !s.is_empty())
        .unwrap_or("")
        .to_string()
}
