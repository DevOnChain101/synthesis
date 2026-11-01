//! Diff of a file change under an Edit / Write / NotebookEdit row: header `Update(path)`, a
//! summary line and a body with an old/new line-number gutter. The numbers come from the
//! structured `diff` of `tool.completed` (computed by the tool where the whole file is known,
//! see docs/specs/tools.md); without it (old logs) a diff is built from the tool input and
//! numbered from 1, with a note. The body is a code block: clamped like every code block.

use super::*;

struct DLine {
    tag: char,
    old: Option<usize>,
    new: Option<usize>,
    text: String,
}

struct Hunk {
    old_start: usize,
    old_lines: usize,
    new_start: usize,
    new_lines: usize,
    lines: Vec<DLine>,
}

struct Diff {
    path: String,
    kind: String,
    added: usize,
    removed: usize,
    hunks: Vec<Hunk>,
    truncated: bool,
    /// Secret file: counts only.
    hidden: bool,
    /// The numbers are the file's real line numbers.
    numbered: bool,
}

pub(super) struct View {
    /// Replaces the tool name in the header.
    pub name: &'static str,
    /// Replaces the header's argument (the path as the tool call gave it).
    pub path: String,
    pub summary: String,
    pub body: Vec<Row>,
    pub footer: Option<String>,
}

fn num(v: &Value, k: &str) -> usize {
    v.get(k).and_then(Value::as_u64).unwrap_or(0) as usize
}

fn parse(v: &Value) -> Option<Diff> {
    let hunks = v.get("hunks")?.as_array()?;
    Some(Diff {
        path: v.get("path").and_then(Value::as_str)?.to_string(),
        kind: v.get("kind").and_then(Value::as_str)?.to_string(),
        added: num(v, "added"),
        removed: num(v, "removed"),
        truncated: v.get("truncated").and_then(Value::as_bool) == Some(true),
        hidden: v.get("hidden").and_then(Value::as_bool) == Some(true),
        numbered: true,
        hunks: hunks
            .iter()
            .map(|h| Hunk {
                old_start: num(h, "old_start"),
                old_lines: num(h, "old_lines"),
                new_start: num(h, "new_start"),
                new_lines: num(h, "new_lines"),
                lines: h
                    .get("lines")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .map(|l| DLine {
                        tag: l
                            .get("tag")
                            .and_then(Value::as_str)
                            .unwrap_or(" ")
                            .chars()
                            .next()
                            .unwrap_or(' '),
                        old: l.get("old_no").and_then(Value::as_u64).map(|n| n as usize),
                        new: l.get("new_no").and_then(Value::as_u64).map(|n| n as usize),
                        text: l
                            .get("text")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string(),
                    })
                    .collect(),
            })
            .collect(),
    })
}

/// A diff from the tool input alone (no `diff` in the event): numbered from 1 per edit.
fn fallback(it: &Item) -> Option<Diff> {
    let inp = &it.input;
    let str_of = |k: &str| inp.get(k).and_then(Value::as_str).unwrap_or("");
    let one = |old: Vec<String>, new: Vec<String>| {
        let mut lines: Vec<DLine> = vec![];
        for (i, t) in old.iter().enumerate() {
            lines.push(DLine {
                tag: '-',
                old: Some(i + 1),
                new: None,
                text: t.clone(),
            });
        }
        for (i, t) in new.iter().enumerate() {
            lines.push(DLine {
                tag: '+',
                old: None,
                new: Some(i + 1),
                text: t.clone(),
            });
        }
        Hunk {
            old_start: 1,
            old_lines: old.len(),
            new_start: 1,
            new_lines: new.len(),
            lines,
        }
    };
    let (path, kind, hunks) = match base_name(&it.name) {
        "Edit" | "MultiEdit" => (
            str_of("file_path"),
            "update",
            input_edits(inp)
                .into_iter()
                .map(|e| {
                    one(
                        edit_lines(e.get("old_string")),
                        edit_lines(e.get("new_string")),
                    )
                })
                .collect::<Vec<_>>(),
        ),
        "Write" => (
            str_of("file_path"),
            "write",
            vec![one(vec![], edit_lines(inp.get("content")))],
        ),
        _ => (
            str_of("notebook_path"),
            "update",
            vec![one(vec![], edit_lines(inp.get("new_source")))],
        ),
    };
    let sum = |f: fn(&Hunk) -> usize| hunks.iter().map(f).sum();
    let (added, removed): (usize, usize) = (sum(|h| h.new_lines), sum(|h| h.old_lines));
    (added + removed > 0).then(|| Diff {
        path: path.to_string(),
        kind: kind.into(),
        added,
        removed,
        hunks,
        truncated: false,
        hidden: false,
        numbered: false,
    })
}

/// Tabs to the next multiple-of-4 column.
fn expand_tabs(s: &str) -> String {
    if !s.contains('\t') {
        return s.to_string();
    }
    let mut out = String::new();
    let mut col = 0;
    for ch in s.chars() {
        if ch == '\t' {
            let n = 4 - col % 4;
            out.push_str(&" ".repeat(n));
            col += n;
        } else {
            out.push(ch);
            col += cw(ch);
        }
    }
    out
}

fn lines_word(n: usize) -> String {
    format!("{n} line{}", if n == 1 { "" } else { "s" })
}

fn body_rows(d: &Diff, w: usize) -> Vec<Row> {
    let max_no = d
        .hunks
        .iter()
        .flat_map(|h| &h.lines)
        .flat_map(|l| [l.old, l.new])
        .flatten()
        .max()
        .unwrap_or(0);
    let nw = max_no.to_string().len();
    let blank = " ".repeat(nw);
    let cell = |n: Option<usize>| n.map_or(blank.clone(), |n| format!("{n:>nw$}"));
    let gutter_w = 2 * nw + 3;
    // the caller indents the body by 5 columns
    let avail = w.saturating_sub(5).max(gutter_w + 2 + 8);
    let mut rows: Vec<Row> = vec![];
    for h in &d.hunks {
        if d.hunks.len() > 1 {
            rows.push(vec![deco(
                &format!(
                    "@@ -{},{} +{},{} @@",
                    h.old_start, h.old_lines, h.new_start, h.new_lines
                ),
                C_SUBTLE,
            )]);
        }
        for l in &h.lines {
            let (mark, col) = match l.tag {
                '+' => ("+", C_ADD),
                '-' => ("−", C_BAD),
                _ => (" ", C_DIM),
            };
            let first = [
                deco(&format!(" {} {} ", cell(l.old), cell(l.new)), C_SUBTLE),
                deco(&format!("{mark} "), col),
            ];
            let rest = [deco(&" ".repeat(gutter_w + 2), C_TEXT)];
            rows.extend(wrap(&[r(expand_tabs(&l.text), col)], avail, &first, &rest));
        }
    }
    if d.truncated {
        rows.push(vec![r(
            "… diff truncated (the rest of the change is not shown)",
            C_SUBTLE,
        )]);
    }
    rows
}

fn summary(d: &Diff) -> String {
    let p = &d.path;
    let tail = if d.hidden {
        " (content hidden: secret file)"
    } else if !d.numbered {
        " (line numbers unavailable)"
    } else {
        ""
    };
    match d.kind.as_str() {
        "create" => format!("Created {p} ({}){tail}", lines_word(d.added)),
        "write" => format!("Wrote {p} ({}){tail}", lines_word(d.added)),
        "delete" => format!("Deleted from {p} — −{}{tail}", d.removed),
        _ if d.added + d.removed == 0 => format!("Updated {p} — no changes"),
        _ => {
            let range = match (d.numbered, d.hunks.first(), d.hunks.last()) {
                (true, Some(a), Some(b)) => {
                    let end = (b.new_start + b.new_lines.saturating_sub(1)).max(a.new_start);
                    if end == a.new_start {
                        format!(" (line {end})")
                    } else {
                        format!(" (lines {}–{end})", a.new_start)
                    }
                }
                _ => String::new(),
            };
            format!("Updated {p} — +{} −{}{range}{tail}", d.added, d.removed)
        }
    }
}

/// Rows of a diff in the approval dialog: a `+N −M` line, then at most `max` numbered diff rows
/// and a footer for the rest. `None` when `v` is not a diff.
pub(super) fn dialog_rows(v: &Value, w: usize, max: usize) -> Option<Vec<Row>> {
    let d = parse(v)?;
    let mut rows = vec![vec![r(summary_counts(&d), C_SUBTLE)]];
    let all = body_rows(&d, w + 5);
    let hidden = all.len().saturating_sub(max);
    rows.extend(all.into_iter().take(max));
    if hidden > 0 {
        rows.push(vec![r(
            format!("… +{hidden} more lines (x shows the whole input)"),
            C_SUBTLE,
        )]);
    }
    Some(rows)
}

fn summary_counts(d: &Diff) -> String {
    let range = match d.hunks.first() {
        Some(h) if d.numbered && d.kind == "update" => format!(" · from line {}", h.new_start),
        _ => String::new(),
    };
    let hidden = if d.hidden {
        " · content hidden: secret file"
    } else {
        ""
    };
    format!("{} +{} −{}{range}{hidden}", d.kind, d.added, d.removed)
}

/// `None`: not a file-change tool, not finished, or nothing to show: the generic rows apply.
pub(super) fn view(it: &Item, w: usize, expanded: bool) -> Option<View> {
    let base = base_name(&it.name);
    if it.status != "done" || !matches!(base, "Edit" | "MultiEdit" | "Write" | "NotebookEdit") {
        return None;
    }
    let d = parse(&it.diff).or_else(|| fallback(it))?;
    let name = match (base, d.kind.as_str()) {
        ("Write", "create") => "Create",
        ("Write", _) => "Write",
        _ => "Update",
    };
    let (body, footer) = clamp(body_rows(&d, w), expanded, false);
    Some(View {
        name,
        path: d.path.clone(),
        summary: summary(&d),
        body,
        footer,
    })
}

/// `lines X–Y of Z` for a Read result (`{:>6}\t{text}` rows). Z is known only when the read ended
/// before the limit, i.e. at the end of the file.
pub(super) fn read_summary(it: &Item) -> Option<String> {
    let no = |l: &str| l.split_once('\t')?.0.trim().parse::<usize>().ok();
    let mut ls = it.result.lines();
    let first = no(ls.next()?)?;
    let last = it.result.lines().next_back().and_then(no)?;
    let n = it.result.lines().count();
    let limit = it
        .input
        .get("limit")
        .and_then(Value::as_u64)
        .filter(|l| *l > 0)
        .unwrap_or(2000) as usize;
    Some(if n < limit {
        format!("lines {first}–{last} of {last}")
    } else {
        format!("lines {first}–{last}")
    })
}
