//! The design's own icons: the 24x24 `<symbol>` paths of `cli.html` (stroke 1.6, round caps), drawn as inline
//! SVG (the technique of `synthesis-cli-ui/gpui/src/cli.rs` `icon()`), so a header or tab icon is the same shape
//! as in the design and takes the platinum colour through `text_color`. [`for_lucide`] maps the Lucide names the
//! app uses elsewhere to the design's.

use gpui::{Hsla, Styled as _, Svg, px, svg};

/// SVG body of the design icon `name`.
pub fn body(name: &str) -> Option<&'static str> {
    Some(match name {
        "folder" => {
            r#"<path d="M3 7a2 2 0 0 1 2-2h4l2 2h8a2 2 0 0 1 2 2v8a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2z"/>"#
        }
        "term" => r#"<path d="M4 5h16v14H4z"/><path d="M7 10l3 2-3 2M12 15h5"/>"#,
        "chat" => r#"<path d="M4 5h16v11H9l-5 4z"/><path d="M8 9h8M8 12h5"/>"#,
        "wrench" => {
            r#"<path d="M14.7 6.3a4 4 0 0 0-5 5L4 17l3 3 5.7-5.7a4 4 0 0 0 5-5l-2.4 2.4-2.3-.7-.7-2.3z"/>"#
        }
        "pulse" => r#"<path d="M3 12h4l2-6 4 12 2-6h6"/>"#,
        "bars" => r#"<path d="M5 20V10M12 20V4M19 20v-7"/>"#,
        "spark" => {
            r#"<path d="M12 3l1.8 5.2L19 10l-5.2 1.8L12 17l-1.8-5.2L5 10l5.2-1.8zM19 16l.7 2 2 .7-2 .7-.7 2-.7-2-2-.7 2-.7z"/>"#
        }
        "layers" => r#"<path d="M12 3l9 5-9 5-9-5zM3 13l9 5 9-5"/>"#,
        "graph" => {
            r#"<circle cx="12" cy="12" r="3"/><circle cx="5" cy="5" r="2"/><circle cx="19" cy="5" r="2"/><circle cx="5" cy="19" r="2"/><circle cx="19" cy="19" r="2"/><path d="M7 7l3 3M17 7l-3 3M7 17l3-3M17 17l-3-3"/>"#
        }
        "checkc" => r#"<circle cx="12" cy="12" r="9"/><path d="M8 12l3 3 5-6"/>"#,
        "user" => r#"<circle cx="12" cy="8" r="4"/><path d="M4 21c0-4 4-6 8-6s8 2 8 6"/>"#,
        "shield" => r#"<path d="M12 3l8 3v6c0 5-4 8-8 9-4-1-8-4-8-9V6z"/>"#,
        "users" => {
            r#"<circle cx="9" cy="8" r="3.5"/><path d="M2 20c0-3.5 3-5.5 7-5.5s7 2 7 5.5M16 5a3.5 3.5 0 0 1 0 7M18 15c2.5.6 4 2.2 4 5"/>"#
        }
        "grid" => {
            r#"<rect x="4" y="4" width="7" height="7" rx="1"/><rect x="13" y="4" width="7" height="7" rx="1"/><rect x="4" y="13" width="7" height="7" rx="1"/><rect x="13" y="13" width="7" height="7" rx="1"/>"#
        }
        "link" => {
            r#"<path d="M10 14a4 4 0 0 0 6 0l3-3a4 4 0 0 0-6-6l-1 1M14 10a4 4 0 0 0-6 0l-3 3a4 4 0 0 0 6 6l1-1"/>"#
        }
        "play" => r#"<circle cx="12" cy="12" r="9"/><path d="M10 8l6 4-6 4z"/>"#,
        "db" => {
            r#"<ellipse cx="12" cy="6" rx="8" ry="3"/><path d="M4 6v12c0 1.7 3.6 3 8 3s8-1.3 8-3V6M4 12c0 1.7 3.6 3 8 3s8-1.3 8-3"/>"#
        }
        "cloud" => r#"<path d="M7 18a4 4 0 0 1-.5-8A6 6 0 0 1 18 9a4.5 4.5 0 0 1 0 9z"/>"#,
        "hook" => r#"<circle cx="12" cy="4.5" r="1.8"/><path d="M12 6.3V20M5 13a7 7 0 0 0 14 0"/>"#,
        "book" => {
            r#"<path d="M12 6.2C10.5 4.9 8.5 4.2 6 4.2H3.5v14H6c2.5 0 4.5.7 6 2 1.5-1.3 3.5-2 6-2h2.5v-14H18c-2.5 0-4.5.7-6 2z"/><path d="M12 6.2v14"/>"#
        }
        "search" => r#"<circle cx="11" cy="11" r="6"/><path d="M21 21l-5-5"/>"#,
        "pencil" => r#"<path d="M4 20l1-4L16 5l3 3L8 19z"/>"#,
        "file" => r#"<path d="M6 3h8l4 4v14H6zM14 3v4h4"/>"#,
        // a pencil with a check: edits are accepted without asking
        "editok" => {
            r#"<path d="M3 16l1-3.5L13 3.5 16 6.5 7 15.5z"/><path d="M12.5 18.5l3 3 6-7"/>"#
        }
        "branch" => {
            r#"<circle cx="6" cy="5" r="2"/><circle cx="6" cy="19" r="2"/><circle cx="18" cy="8" r="2"/><path d="M6 7v10M18 10c0 4-6 3-12 7"/>"#
        }
        "globe" => {
            r#"<circle cx="12" cy="12" r="9"/><path d="M3 12h18M12 3c3 3 3 15 0 18M12 3c-3 3-3 15 0 18"/>"#
        }
        "flask" => {
            r#"<path d="M9 3h6M10 3v6L4 19a1.5 1.5 0 0 0 1.3 2h13.4A1.5 1.5 0 0 0 20 19l-6-10V3"/>"#
        }
        "chev" => r#"<path d="M6 9l6 6 6-6"/>"#,
        "send" => r#"<path d="M21 3L3 10l7 3 3 7zM10 13l11-10"/>"#,
        "close" => r#"<path d="M6 6l12 12M18 6L6 18"/>"#,
        "clock" => r#"<circle cx="12" cy="12" r="9"/><path d="M12 7v5l3 2"/>"#,
        "plus" => r#"<path d="M12 5v14M5 12h14"/>"#,
        "fast" => r#"<path d="M4 6l7 6-7 6zM13 6l7 6-7 6z"/>"#,
        "cpu" => {
            r#"<rect x="6" y="6" width="12" height="12" rx="2"/><path d="M9 2v4M15 2v4M9 18v4M15 18v4M2 9h4M2 15h4M18 9h4M18 15h4"/>"#
        }
        // the Tauri app's title bar (`index.html` `#chrome`)
        "tui" => r#"<path d="M4 6h16M4 12h10M4 18h16"/><circle cx="19" cy="12" r="1.6"/>"#,
        "agent" => {
            r#"<path d="M12 3l1.8 5.2L19 10l-5.2 1.8L12 17l-1.8-5.2L5 10l5.2-1.8zM18 16l.8 2.2L21 19l-2.2.8L18 22l-.8-2.2L15 19l2.2-.8z"/>"#
        }
        "gear" => {
            r#"<circle cx="12" cy="12" r="3"/><path d="M19.4 15a1.7 1.7 0 0 0 .3 1.8l.1.1a2 2 0 1 1-2.8 2.8l-.1-.1a1.7 1.7 0 0 0-1.8-.3 1.7 1.7 0 0 0-1 1.5V21a2 2 0 1 1-4 0v-.1a1.7 1.7 0 0 0-1.1-1.5 1.7 1.7 0 0 0-1.8.3l-.1.1a2 2 0 1 1-2.8-2.8l.1-.1a1.7 1.7 0 0 0 .3-1.8 1.7 1.7 0 0 0-1.5-1H3a2 2 0 1 1 0-4h.1a1.7 1.7 0 0 0 1.5-1.1 1.7 1.7 0 0 0-.3-1.8l-.1-.1a2 2 0 1 1 2.8-2.8l.1.1a1.7 1.7 0 0 0 1.8.3H9a1.7 1.7 0 0 0 1-1.5V3a2 2 0 1 1 4 0v.1a1.7 1.7 0 0 0 1 1.5 1.7 1.7 0 0 0 1.8-.3l.1-.1a2 2 0 1 1 2.8 2.8l-.1.1a1.7 1.7 0 0 0-.3 1.8V9a1.7 1.7 0 0 0 1.5 1H21a2 2 0 1 1 0 4h-.1a1.7 1.7 0 0 0-1.5 1z"/>"#
        }
        "win-min" => r#"<path d="M5 12h14"/>"#,
        "win-max" => r#"<rect x="5" y="5" width="14" height="14" rx="1"/>"#,
        "win-close" => r#"<path d="M6 6l12 12M18 6L6 18"/>"#,
        _ => return None,
    })
}

/// The design icon that stands for a Lucide icon name of the app, if it has one.
pub fn for_lucide(name: &str) -> Option<&'static str> {
    Some(match name {
        "folder" => "folder",
        "activity" => "pulse",
        "square-terminal" | "terminal" => "term",
        "wrench" => "wrench",
        "sparkles" => "spark",
        "git-branch" => "branch",
        "message-square" => "chat",
        "chart-bar" => "bars",
        "network" => "graph",
        "webhook" => "hook",
        "database" => "db",
        "book-open" => "book",
        "search" => "search",
        "chevron-down" => "chev",
        "chevrons-right" => "fast",
        "send" => "send",
        "plus" => "plus",
        "user" => "user",
        "shield" => "shield",
        "users" => "users",
        "layers" => "layers",
        "link" => "link",
        "globe" => "globe",
        "file" => "file",
        "pencil-check" => "editok",
        "cpu" => "cpu",
        "clock" => "clock",
        "tui" | "agent" | "gear" | "win-min" | "win-max" | "win-close" => {
            return Some(match name {
                "tui" => "tui",
                "agent" => "agent",
                "gear" => "gear",
                "win-min" => "win-min",
                "win-max" => "win-max",
                _ => "win-close",
            });
        }
        _ => return None,
    })
}

/// An inline SVG of design icon `name` at `size` px.
pub fn svg_of(name: &str, size: f32, c: Hsla) -> Option<Svg> {
    let b = body(name)?;
    let data = format!(
        "<svg xmlns='http://www.w3.org/2000/svg' viewBox='0 0 24 24' fill='none' stroke='currentColor' stroke-width='1.6' stroke-linecap='round' stroke-linejoin='round'>{}</svg>",
        b.replace('"', "'")
    );
    Some(
        svg()
            .data(data.as_bytes())
            .w(px(size))
            .h(px(size))
            .text_color(c),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_mapped_name_has_a_body() {
        for n in [
            "folder",
            "activity",
            "square-terminal",
            "wrench",
            "sparkles",
            "git-branch",
            "message-square",
            "chart-bar",
            "network",
            "webhook",
            "database",
            "book-open",
            "search",
            "chevron-down",
            "chevrons-right",
            "send",
            "plus",
            "user",
            "shield",
            "users",
            "layers",
            "link",
            "globe",
            "file",
            "pencil-check",
            "cpu",
            "clock",
        ] {
            assert!(body(for_lucide(n).unwrap()).is_some(), "{n}");
        }
    }
}
