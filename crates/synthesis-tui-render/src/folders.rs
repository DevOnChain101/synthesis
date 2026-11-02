//! Path shortening for the sidebar and chat header (the pure parts of `folders.js`).

use synthesis_tui_base::text::width;

fn segments(p: &str) -> impl Iterator<Item = &str> {
    p.split(['\\', '/']).filter(|s| !s.is_empty())
}

/// Cut to `max` cells with a trailing ellipsis (the head stays).
pub fn clip(s: &str, max: usize) -> String {
    if width(s) <= max {
        return s.to_string();
    }
    if max < 1 {
        return String::new();
    }
    let mut out = String::new();
    for ch in s.chars() {
        let mut t = out.clone();
        t.push(ch);
        if width(&t) > max - 1 {
            break;
        }
        out = t;
    }
    out.push('…');
    out
}

/// Cut from the left, at a separator when there is one: "…\workspace\synthesis-code".
pub fn shorten_left(p: &str, max: usize) -> String {
    if width(p) <= max {
        return p.to_string();
    }
    if max < 2 {
        return "…".chars().take(max).collect();
    }
    let chars: Vec<char> = p.chars().collect();
    let tail: String = chars[chars.len().saturating_sub(max - 1)..]
        .iter()
        .collect();
    match tail.find(['\\', '/']).filter(|i| *i > 0) {
        Some(i) => format!("…{}", &tail[i..]),
        None => format!("…{tail}"),
    }
}

/// "synthesis-code (D:\workspace)", the parent cut from the left to fit `max`.
pub fn folder_with_parent(cwd: &str, max: usize) -> String {
    let base = segments(cwd).last().unwrap_or(cwd).to_string();
    // cwd.replace(/[\\/]+[^\\/]+[\\/]*$/, '')
    let trimmed = cwd.trim_end_matches(['\\', '/']);
    let parent = match trimmed.rfind(['\\', '/']) {
        Some(i) => trimmed[..i].trim_end_matches(['\\', '/']).to_string(),
        _ => cwd.to_string(),
    };
    if parent.is_empty() || parent == cwd {
        return clip(&base, max);
    }
    let room = max as i64 - width(&base) as i64 - 3;
    if room >= 4 {
        format!("{base} ({})", shorten_left(&parent, room as usize))
    } else {
        clip(&base, max)
    }
}
