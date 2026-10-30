//! The `/config` screen (docs/specs/tui-design.md §7): a rounded frame with a filter line, the four
//! groups (Panels, Appearance, Behaviour, Mouse) and a row list. Pure: the owner of the settings
//! builds the rows and the cursor, this draws them. The frame sits in the chat column in Columns
//! mode (so the sidebars preview each change) and takes the whole body otherwise.

use crate::layout::PanelId;
use crate::picker::matches;
use crate::theme::*;
use synthesis_tui_base::text::width;

pub const GROUPS: [&str; 4] = ["Panels", "Appearance", "Behaviour", "Mouse"];

#[derive(Clone, Debug, PartialEq)]
pub enum State {
    Toggle(bool),
    /// A choice among several values (the current one is shown).
    Choice(String),
}

#[derive(Clone, Debug, PartialEq)]
pub struct Row {
    pub group: usize,
    pub section: &'static str,
    /// Stable key, e.g. `panels.session` or `theme`.
    pub id: String,
    pub label: String,
    pub desc: String,
    pub state: State,
    /// `Some(reason)`: the row cannot be changed; the reason is what the hint line says.
    pub lock: Option<String>,
    pub panel: Option<PanelId>,
}

#[derive(Clone, Debug, Default)]
pub struct ConfigView {
    pub rows: Vec<Row>,
    pub group: usize,
    /// Index into `visible()`.
    pub cursor: usize,
    pub filter: String,
    /// The filter line has the keyboard (typing edits the query).
    pub filtering: bool,
    /// What the hint line says instead of the key list: a lock reason or a diagnostic.
    pub hint: Option<String>,
}

impl ConfigView {
    /// Indices into `rows` of what is listed: the rows of the group, or, with a filter, every row
    /// of every group that matches it.
    pub fn visible(&self) -> Vec<usize> {
        self.rows
            .iter()
            .enumerate()
            .filter(|(_, r)| {
                if self.filter.is_empty() {
                    r.group == self.group
                } else {
                    [
                        r.label.as_str(),
                        r.desc.as_str(),
                        r.section,
                        GROUPS[r.group],
                    ]
                    .iter()
                    .any(|t| matches(&self.filter, t))
                }
            })
            .map(|(i, _)| i)
            .collect()
    }
}

/// What the renderer knows that the settings do not: which panels have no room at this size, and a
/// note for sizes where the sidebars cannot be shown.
#[derive(Clone, Debug, Default)]
pub struct Context<'a> {
    pub starved: &'a [PanelId],
    pub note: Option<&'a str>,
}

pub struct Rendered {
    pub lines: Vec<String>,
    /// `(row within lines, index into ConfigView::visible())` of every listed row.
    pub hits: Vec<(i64, usize)>,
}

enum Item {
    Heading(String),
    Row(usize),
}

const KEYS: [(u8, &str, &str); 7] = [
    (1, "↑↓", "move"),
    (2, "space", "toggle"),
    (3, "tab", "group"),
    (4, "/", "filter"),
    (5, "r", "reset"),
    (6, "esc", "close"),
    (7, "←→", "choose"),
];

fn items(v: &ConfigView, vis: &[usize]) -> Vec<Item> {
    let mut out = vec![];
    let mut last: Option<(usize, &str)> = None;
    for (k, &i) in vis.iter().enumerate() {
        let r = &v.rows[i];
        let head = if v.filter.is_empty() {
            r.section
        } else {
            GROUPS[r.group]
        };
        if last != Some((r.group, head)) && !head.is_empty() {
            if !out.is_empty() {
                out.push(Item::Heading(String::new()));
            }
            out.push(Item::Heading(head.to_uppercase()));
        }
        last = Some((r.group, head));
        out.push(Item::Row(k));
    }
    out
}

fn row_line(r: &Row, selected: bool, ctx: &Context, w: usize, label_w: usize) -> String {
    let (th, g) = (t(), glyphs());
    let bar = if selected {
        paint(th.pearl, g.focus)
    } else {
        " ".into()
    };
    let starved = r.panel.is_some_and(|p| ctx.starved.contains(&p));
    let (mark, tail) = if r.lock.is_some() {
        (
            paint(th.muted, g.locked),
            paint(th.muted, &format!("locked · {}", r.desc)),
        )
    } else {
        match &r.state {
            State::Toggle(on) => (
                if *on {
                    paint(th.ok, g.live)
                } else {
                    paint(th.muted, g.idle)
                },
                if starved && *on {
                    paint(th.warn, "on · no room at this size")
                } else {
                    paint(th.muted, &r.desc)
                },
            ),
            State::Choice(v) => (
                paint(th.accent, "‹"),
                format!(
                    "{} {}",
                    paint(th.strong, v),
                    paint(th.muted, &format!("› {}", r.desc))
                ),
            ),
        }
    };
    let label = paint(if selected { th.strong } else { th.body }, &r.label);
    let pad = " ".repeat(label_w.saturating_sub(width(&r.label)));
    fit(&format!("{bar}{mark} {label}{pad}  {tail}"), w)
}

/// Draws the screen in `w` x `h` cells.
pub fn render(v: &ConfigView, w: i64, h: i64, ctx: &Context) -> Rendered {
    let th = t();
    let (w, h) = (w.max(20) as usize, h.max(6) as usize);
    let inner = w - 2;
    let b = |s: &str| paint(th.border, s);
    let pad = |s: &str| format!("{} {}", b("│"), fit(s, inner - 2)) + &format!(" {}", b("│"));
    let roomy = h >= 16;
    let title = " C O N F I G ";
    let close = " esc close ";
    let top = format!(
        "{}{}{}{}{}",
        b("╭─"),
        paint(th.strong, title),
        b(&"─".repeat((inner).saturating_sub(1 + width(title) + width(close)))),
        paint(th.muted, close),
        b("─╮")
    );
    let mut lines = vec![fit(&top, w)];
    let filter_line = if v.filtering || !v.filter.is_empty() {
        format!(
            "{} {}{}",
            paint(th.accent, "/"),
            paint(th.strong, &v.filter),
            if v.filtering {
                paint(th.pearl, "▏")
            } else {
                String::new()
            }
        )
    } else {
        format!("{} {}", paint(th.muted, "/"), paint(th.muted, "filter"))
    };
    lines.push(pad(&format!(" {filter_line}")));
    if roomy {
        lines.push(pad(""));
    }
    let tabs: Vec<String> = GROUPS
        .iter()
        .enumerate()
        .map(|(i, name)| {
            if v.filter.is_empty() && i == v.group {
                paint(th.strong, &name.to_uppercase())
            } else {
                paint(
                    if v.filter.is_empty() {
                        th.muted
                    } else {
                        th.border
                    },
                    name,
                )
            }
        })
        .collect();
    let sep = paint(th.border, &format!("   {}   ", glyphs().dot));
    lines.push(pad(&format!(" {}", tabs.join(&sep))));
    if roomy {
        lines.push(pad(""));
    }
    let hint_rows = if roomy { 2 } else { 1 };
    let note_rows = usize::from(ctx.note.is_some());
    let cap = h
        .saturating_sub(lines.len() + 1 + hint_rows + note_rows)
        .max(1);
    let vis = v.visible();
    let its = items(v, &vis);
    let cursor_item = its
        .iter()
        .position(|i| matches!(i, Item::Row(k) if *k == v.cursor))
        .unwrap_or(0);
    let off = if its.len() <= cap {
        0
    } else {
        cursor_item.saturating_sub(cap / 2).min(its.len() - cap)
    };
    let label_w = v
        .rows
        .iter()
        .map(|r| width(&r.label))
        .max()
        .unwrap_or(10)
        .min(20);
    let mut hits = vec![];
    if let Some(n) = ctx.note {
        lines.push(pad(&format!(" {}", paint(th.warn, n))));
    }
    for (n, it) in its.iter().enumerate().skip(off).take(cap) {
        let more_up = n == off && off > 0;
        let more_down = n + 1 == off + cap && off + cap < its.len();
        let text = if more_up {
            paint(th.muted, &format!("  ↑ {off} more"))
        } else if more_down {
            paint(th.muted, &format!("  ↓ {} more", its.len() - n))
        } else {
            match it {
                Item::Heading(s) => format!(" {}", paint(th.label, s)),
                Item::Row(k) => {
                    hits.push((lines.len() as i64, *k));
                    format!(
                        " {}",
                        row_line(&v.rows[vis[*k]], *k == v.cursor, ctx, inner - 3, label_w)
                    )
                }
            }
        };
        lines.push(pad(&text));
    }
    let fill = h.saturating_sub(lines.len() + 1 + hint_rows);
    for _ in 0..fill {
        lines.push(pad(""));
    }
    if roomy {
        lines.push(pad(""));
    }
    let hint = match &v.hint {
        Some(t) => paint(th.warn, t),
        None => {
            let items: Vec<(u8, String)> = KEYS
                .iter()
                .map(|(p, k, d)| (*p, format!("{} {}", paint(th.body, k), paint(th.muted, d))))
                .collect();
            fit_items(&items, "  ", inner - 4)
        }
    };
    lines.push(pad(&format!(" {hint}")));
    lines.truncate(h - 1);
    while lines.len() < h - 1 {
        lines.push(pad(""));
    }
    lines.push(fit(&b(&format!("╰{}╯", "─".repeat(inner))), w));
    Rendered { lines, hits }
}

#[cfg(test)]
mod tests {
    use super::*;
    use synthesis_tui_base::text::strip_ansi;

    fn rows() -> Vec<Row> {
        let r =
            |group, section, id: &str, label: &str, desc: &str, on: bool, lock: Option<&str>| Row {
                group,
                section,
                id: id.into(),
                label: label.into(),
                desc: desc.into(),
                state: State::Toggle(on),
                lock: lock.map(String::from),
                panel: None,
            };
        vec![
            r(
                0,
                "Left sidebar",
                "panels.workspace",
                "Workspace",
                "always visible",
                true,
                Some("always visible"),
            ),
            r(
                0,
                "Left sidebar",
                "panels.session",
                "Session",
                "model, mode",
                true,
                None,
            ),
            r(
                0,
                "Left sidebar",
                "panels.tools_skills",
                "Tools & skills",
                "calls",
                false,
                None,
            ),
            r(1, "", "theme", "Theme", "colours", true, None),
            r(2, "", "motion", "Motion", "spinners", true, None),
        ]
    }

    #[test]
    fn frame_has_the_exact_size_and_lists_the_group() {
        let v = ConfigView {
            rows: rows(),
            ..Default::default()
        };
        for (w, h) in [(60, 20), (40, 11), (100, 30), (200, 50)] {
            let out = render(&v, w, h, &Context::default());
            assert_eq!(out.lines.len(), h as usize);
            for l in &out.lines {
                assert_eq!(width(l), w as usize, "{}", strip_ansi(l));
            }
        }
        let text = render(&v, 80, 24, &Context::default())
            .lines
            .iter()
            .map(|l| strip_ansi(l))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            text.contains("C O N F I G") && text.contains("PANELS") && text.contains("Session"),
            "{text}"
        );
        assert!(
            text.contains("▣ Workspace") && text.contains("locked · always visible"),
            "{text}"
        );
        assert!(
            !text.contains("Theme"),
            "other groups are not listed: {text}"
        );
    }

    #[test]
    fn filter_spans_all_groups_and_hits_follow_the_rows() {
        let mut v = ConfigView {
            rows: rows(),
            filter: "mot".into(),
            ..Default::default()
        };
        assert_eq!(v.visible(), vec![4]);
        v.filter = "e".into();
        assert!(v.visible().len() > 3);
        let out = render(&v, 80, 24, &Context::default());
        assert_eq!(out.hits.len(), v.visible().len());
    }

    #[test]
    fn starved_panel_says_so() {
        let mut rs = rows();
        rs[1].panel = Some(PanelId::Session);
        let v = ConfigView {
            rows: rs,
            cursor: 1,
            ..Default::default()
        };
        let text = render(
            &v,
            80,
            24,
            &Context {
                starved: &[PanelId::Session],
                note: None,
            },
        )
        .lines
        .iter()
        .map(|l| strip_ansi(l))
        .collect::<String>();
        assert!(text.contains("on · no room at this size"), "{text}");
    }
}
