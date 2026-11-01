//! Selecting and copying chat text with the mouse (port of `selection.js`, minus the clipboard,
//! which lives in [`crate::clipboard`]). Mouse reporting is on, so the terminal's own selection
//! is off; the gesture is the pure state machine [`selection_transition`], the geometry works in
//! content rows (survives scrolling) and screen rows (painting).
//!
//! Cell widths use [`crate::text::char_width`] per code point, exactly like Node.

use crate::keys::Key;
use crate::text::{char_width, csi_len, is_js_space, strip_ansi};
use std::borrow::Cow;

const SELECT_ON: &str = "\x1b[38;2;245;245;245m\x1b[48;2;60;72;105m";
const SELECT_OFF: &str = "\x1b[39m\x1b[49m";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Idle,
    /// Button down.
    Dragging,
    /// Released and copied; highlight and anchor kept.
    Selected,
    /// Released with Shift/Alt: not copied yet, highlight and anchor kept.
    Pending,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Event {
    /// `text`: the cell has a glyph; `body`: it is inside the chat body.
    Down {
        text: bool,
        body: bool,
        shift: bool,
        alt: bool,
    },
    Drag,
    /// `extending`: the press was an `Extend` one.
    Up {
        moved: bool,
        shift: bool,
        alt: bool,
        extending: bool,
    },
    Wheel {
        shift: bool,
        alt: bool,
    },
    /// PageUp / PageDown when `page`.
    Key {
        page: bool,
    },
    /// Resize, submit, tab switch.
    Cancel,
    Unknown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Act {
    /// New selection at the press.
    Start,
    /// Keep the anchor; the press sets the focus.
    Extend,
    /// Keep the highlight, copy nothing.
    Pend,
    Copy,
    Clear,
    None,
}

pub fn selection_transition(mode: Mode, ev: Event) -> (Mode, Act) {
    let clear = (Mode::Idle, Act::Clear);
    let kept = matches!(mode, Mode::Selected | Mode::Pending);
    match ev {
        Event::Down {
            text,
            body,
            shift,
            alt,
        } => {
            if kept && (text || ((shift || alt) && body)) {
                (Mode::Dragging, Act::Extend)
            } else if text {
                (Mode::Dragging, Act::Start)
            } else {
                clear
            }
        }
        Event::Up {
            moved,
            shift,
            alt,
            extending,
        } => {
            if mode != Mode::Dragging {
                (mode, Act::None)
            } else if extending {
                (Mode::Selected, Act::Copy) // the click that ends the range: copy once
            } else if !moved {
                clear
            } else if shift || alt {
                (Mode::Pending, Act::Pend)
            } else {
                (Mode::Selected, Act::Copy)
            }
        }
        // the text moves under a selection nobody is scrolling for
        Event::Wheel { shift, alt } if kept && !(shift || alt) => clear,
        Event::Key { page } if mode != Mode::Idle && !page => clear,
        Event::Cancel => clear,
        _ => (mode, Act::None),
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Point {
    pub x: i64,
    pub y: i64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Selection {
    pub a: Point,
    pub b: Point,
}

/// `x0..x1` (exclusive) and `y0..=y1` screen cells a selection can be painted on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Bounds {
    pub x0: i64,
    pub x1: i64,
    pub y0: i64,
    pub y1: i64,
}

/// Dragging to (or past) the top/bottom text row scrolls: rows per tick, negative = up.
pub fn edge_scroll_step(y: i64, y0: i64, y1: i64) -> i64 {
    if y >= y1 {
        (y - y1).clamp(1, 6)
    } else if y <= y0 {
        -(y0 - y).clamp(1, 6)
    } else {
        0
    }
}

/// The `SYNTHESIS_MOUSE_DEBUG=1` line; `None` for non-mouse keys.
pub fn describe_mouse(k: &Key) -> Option<String> {
    let (mods, x, y) = match k {
        Key::Mouse { mods, x, y, .. }
        | Key::MouseDrag { mods, x, y, .. }
        | Key::Wheel { mods, x, y, .. } => (mods, x, y),
        _ => return None,
    };
    let name = match k {
        Key::Mouse { action, .. } => format!(
            "mouse {}",
            if *action == crate::keys::MouseAction::Down {
                "down"
            } else {
                "up"
            }
        ),
        k => k.name().to_string(),
    };
    Some(format!(
        "{name} shift={} alt={} ctrl={} @{x},{y}",
        mods.shift, mods.alt, mods.ctrl
    ))
}

/// The chat pane (`x`, `y`, width `w`) and the last frame's body (`total` rows, `h` visible).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rect {
    pub x: i64,
    pub y: i64,
    pub w: i64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Body {
    pub total: i64,
    pub h: i64,
}

/// Where the chat body sits on screen. `row_at0` is the content row on the first body line
/// (negative while the conversation is shorter than the window); `more` is 1 when the last body
/// line is the "N more rows" hint.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Geom {
    pub x0: i64,
    pub x1: i64,
    pub top: i64,
    pub h: i64,
    pub row_at0: i64,
    pub total: i64,
    pub more: i64,
}

pub fn body_geom(rect: Option<Rect>, body: Option<Body>, scroll: i64) -> Option<Geom> {
    let (rect, body) = (rect?, body?);
    if body.total < 1 || body.h < 2 {
        return None;
    }
    let off = scroll.min((body.total - body.h).max(0));
    Some(Geom {
        x0: rect.x + 2,
        x1: (rect.x + 3).max(rect.x + rect.w - 1),
        top: rect.y + 1,
        h: body.h,
        row_at0: body.total - off - body.h,
        total: body.total,
        more: i64::from(off > 0),
    })
}

/// The screen cells a selection can be painted on: the body lines, without the "more rows" hint.
pub fn body_bounds(g: &Geom) -> Bounds {
    Bounds {
        x0: g.x0,
        x1: g.x1,
        y0: g.top,
        y1: g.top + g.h - 1 - g.more,
    }
}

/// Pin a point inside the pane's text area.
pub fn clamp_point(p: Point, b: &Bounds) -> Point {
    Point {
        x: p.x.min(b.x1 - 1).max(b.x0),
        y: p.y.min(b.y1).max(b.y0),
    }
}

/// A screen point as a content point, pinned inside the body and the conversation.
pub fn point_to_content(g: &Geom, p: Point) -> Point {
    let c = clamp_point(p, &body_bounds(g));
    Point {
        x: c.x,
        y: (c.y - g.top + g.row_at0).min(g.total - 1).max(0),
    }
}

/// The content-row selection in screen rows (its ends may lie outside the screen).
pub fn to_screen(sel: &Selection, g: &Geom) -> Selection {
    let f = |p: Point| Point {
        x: p.x,
        y: p.y - g.row_at0 + g.top,
    };
    Selection {
        a: f(sel.a),
        b: f(sel.b),
    }
}

/// Is there a visible glyph (not a space) on cell `x` of a painted line? A wide glyph covers two.
pub fn cell_has_text(line: &str, x: i64) -> bool {
    let mut cell = 0i64;
    for ch in strip_ansi(line).chars() {
        let w = char_width(ch) as i64;
        if x >= cell && x < cell + w {
            return !is_js_space(ch);
        }
        cell += w;
    }
    false
}

/// The cell after the last visible glyph of a painted line (0 when it is blank).
fn text_end(line: &str) -> i64 {
    let (mut cell, mut end) = (0i64, 0i64);
    for ch in strip_ansi(line).chars() {
        cell += char_width(ch) as i64;
        if !is_js_space(ch) {
            end = cell;
        }
    }
    end
}

/// Reading order: the earlier point first.
fn ordered(sel: &Selection) -> (Point, Point) {
    let (a, b) = (sel.a, sel.b);
    if a.y < b.y || (a.y == b.y && a.x <= b.x) {
        (a, b)
    } else {
        (b, a)
    }
}

/// The cells `[from, to)` selected on screen row `y`.
pub fn row_range(sel: &Selection, b: &Bounds, y: i64) -> Option<(i64, i64)> {
    let (s, e) = ordered(sel);
    if y < s.y || y > e.y || y < b.y0 || y > b.y1 {
        return None;
    }
    let from = b.x0.max(if y == s.y { s.x } else { b.x0 });
    let to = b.x1.min(if y == e.y { e.x + 1 } else { b.x1 });
    (to > from).then_some((from, to))
}

/// A painted line with cells `[from, to)` on the selection colours. Colour codes inside the
/// range would change them, so they are re-asserted after each one; leaving the range replays
/// the colour codes seen since the last reset. A lone ESC that starts no CSI is dropped (as in Node).
pub fn highlight_cells(line: &str, from: i64, to: i64) -> String {
    let (mut cell, mut on) = (0i64, false);
    let mut out = String::new();
    let mut cur = String::new(); // colour codes in effect outside the highlight
    let mut i = 0;
    while i < line.len() {
        if let Some(n) = csi_len(line, i) {
            let tok = &line[i..i + n];
            i += n;
            out.push_str(tok);
            if tok == "\x1b[m" || tok == "\x1b[0m" {
                cur.clear();
            } else if tok.ends_with('m') {
                cur.push_str(tok);
            }
            if on {
                out.push_str(SELECT_ON);
            }
            continue;
        }
        let ch = line[i..].chars().next().expect("in bounds");
        i += ch.len_utf8();
        if ch == '\x1b' {
            continue;
        }
        let inside = cell >= from && cell < to;
        if inside && !on {
            out.push_str(SELECT_ON);
            on = true;
        } else if !inside && on {
            out.push_str(SELECT_OFF);
            out.push_str(&cur);
            on = false;
        }
        out.push(ch);
        cell += char_width(ch) as i64;
    }
    if on {
        out.push_str(SELECT_OFF);
    }
    out
}

/// The frame with the selection highlighted. Only text is painted: blank rows and the blank
/// cells after a row's last glyph stay as they are. Without a selection `lines` is returned as is.
pub fn apply_selection<'a>(
    lines: &'a [String],
    sel: Option<&Selection>,
    bounds: Option<&Bounds>,
) -> Cow<'a, [String]> {
    let (Some(sel), Some(b)) = (sel, bounds) else {
        return Cow::Borrowed(lines);
    };
    Cow::Owned(
        lines
            .iter()
            .enumerate()
            .map(|(y, line)| match row_range(sel, b, y as i64) {
                Some((from, r1)) => {
                    let to = r1.min(text_end(line));
                    if to > from {
                        highlight_cells(line, from, to)
                    } else {
                        line.clone()
                    }
                }
                None => line.clone(),
            })
            .collect(),
    )
}

/// Plain text of cells `[from, to)` of a line.
pub fn slice_cells(line: &str, from: i64, to: i64) -> String {
    let mut cell = 0i64;
    let mut out = String::new();
    for ch in strip_ansi(line).chars() {
        let w = char_width(ch) as i64;
        // a wide glyph is in when either of its two cells is (a drag may end on its right half)
        if cell + w > from && cell < to {
            out.push(ch);
        }
        cell += w;
    }
    out
}

/// Cells `[from, to)` of the word under `cell` (double click): a run of letters/digits/`_`, of
/// punctuation, or of the same wide script. `None` on blank cells.
pub fn word_span(line: &str, cell: i64) -> Option<(i64, i64)> {
    let class = |c: char| match c {
        c if is_js_space(c) => 0,
        c if c.is_alphanumeric() || c == '_' => 1,
        _ => 2,
    };
    let mut cells = vec![]; // (start, end, class)
    let mut at = 0i64;
    for ch in strip_ansi(line).chars() {
        let w = char_width(ch) as i64;
        cells.push((at, at + w, class(ch)));
        at += w;
    }
    let i = cells.iter().position(|&(f, t, _)| cell >= f && cell < t)?;
    let k = cells[i].2;
    if k == 0 {
        return None;
    }
    let (mut a, mut b) = (i, i);
    while a > 0 && cells[a - 1].2 == k {
        a -= 1;
    }
    while b + 1 < cells.len() && cells[b + 1].2 == k {
        b += 1;
    }
    Some((cells[a].0, cells[b].1))
}

/// What was selected, as text: rows joined by newlines, trailing spaces dropped, no blank lines
/// at either end, a run of blank lines in the middle reduced to one.
pub fn text_of_selection(
    row_at: impl Fn(i64) -> String,
    sel: Option<&Selection>,
    bounds: Option<&Bounds>,
) -> String {
    let (Some(sel), Some(b)) = (sel, bounds) else {
        return String::new();
    };
    let rows: Vec<String> = (b.y0..=b.y1)
        .filter_map(|y| {
            row_range(sel, b, y).map(|(f, t)| {
                slice_cells(&row_at(y), f, t)
                    .trim_end_matches(is_js_space)
                    .to_string()
            })
        })
        .collect();
    collapse(&rows.iter().map(|r| (r.as_str(), false)).collect::<Vec<_>>())
}

/// Rows joined by newlines (a soft-wrapped row continues straight into the next), no blank lines
/// at either end, a run of blank lines in the middle reduced to one.
fn collapse(rows: &[(&str, bool)]) -> String {
    let mut joined = String::new();
    for (i, (t, soft)) in rows.iter().enumerate() {
        joined.push_str(t);
        if i + 1 < rows.len() && !soft {
            joined.push('\n');
        }
    }
    let mut out = String::new();
    let mut run = 0;
    for c in joined.trim_matches('\n').chars() {
        run = if c == '\n' { run + 1 } else { 0 };
        if run <= 2 {
            out.push(c);
        }
    }
    out
}

/// A painted chat row for copying: `text` has decorations blanked (so cells line up with the
/// screen), `lead` leading cells that are only decoration, `soft` = the line goes on in the next row.
#[derive(Clone, Debug, Default)]
pub struct SelRow {
    pub text: String,
    pub soft: bool,
    pub lead: i64,
}

/// Like [`text_of_content`] for rows that know their decorations and soft wraps: the bar of a
/// gutter is never copied and a wrapped paragraph comes out as one line.
pub fn text_of_rows(
    sel: &Selection,
    x0: i64,
    x1: i64,
    rows_of: impl Fn(i64, i64) -> Vec<SelRow>,
) -> String {
    let (s, e) = ordered(sel);
    let rows = rows_of(s.y, e.y);
    let shift = |p: Point| Point {
        x: p.x - x0,
        y: p.y,
    };
    let (a, b) = (shift(sel.a), shift(sel.b));
    let bounds = Bounds {
        x0: 0,
        x1: x1 - x0,
        y0: s.y,
        y1: e.y,
    };
    let parts: Vec<(String, bool)> = (s.y..=e.y)
        .filter_map(|y| {
            let r = rows.get((y - s.y) as usize)?;
            let (f, t) = row_range(&Selection { a, b }, &bounds, y)?;
            let text = slice_cells(&r.text, f.max(r.lead), t);
            Some(if r.soft {
                (text, true)
            } else {
                (text.trim_end_matches(is_js_space).to_string(), false)
            })
        })
        .collect();
    let mut view: Vec<(&str, bool)> = parts.iter().map(|(t, s)| (t.as_str(), *s)).collect();
    // nothing follows the last selected row, so its soft wrap does not matter: drop its trailing blanks
    let tail = view.last().map(|(t, _)| t.trim_end_matches(is_js_space));
    if let (Some(l), Some(t)) = (view.last_mut(), tail) {
        *l = (t, false);
    }
    collapse(&view)
}

/// The text of a content-row selection, rows on or off screen alike. `rows_of(from, to)` gives
/// the plain rows `from..=to` starting at the text area's left edge; `x0`/`x1` bound that area.
pub fn text_of_content(
    sel: &Selection,
    x0: i64,
    x1: i64,
    rows_of: impl Fn(i64, i64) -> Vec<String>,
) -> String {
    let (s, e) = ordered(sel);
    let texts = rows_of(s.y, e.y);
    let shift = |p: Point| Point {
        x: p.x - x0,
        y: p.y,
    };
    let shifted = Selection {
        a: shift(sel.a),
        b: shift(sel.b),
    };
    let bounds = Bounds {
        x0: 0,
        x1: x1 - x0,
        y0: s.y,
        y1: e.y,
    };
    text_of_selection(
        |y| texts.get((y - s.y) as usize).cloned().unwrap_or_default(),
        Some(&shifted),
        Some(&bounds),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blank_runs_collapse() {
        let lines: Vec<String> = ["a", "", "", "", "b", "", ""].map(String::from).to_vec();
        let b = Bounds {
            x0: 0,
            x1: 10,
            y0: 0,
            y1: 6,
        };
        let sel = Selection {
            a: Point { x: 0, y: 0 },
            b: Point { x: 5, y: 6 },
        };
        let t = text_of_selection(|y| lines[y as usize].clone(), Some(&sel), Some(&b));
        assert_eq!(t, "a\n\nb");
    }

    #[test]
    fn word_span_picks_words_punctuation_and_wide_glyphs() {
        assert_eq!(word_span("let foo_bar = 1;", 6), Some((4, 11)));
        assert_eq!(word_span("let foo_bar = 1;", 3), None);
        assert_eq!(word_span("a.b", 1), Some((1, 2)));
        // wide glyphs take two cells; either half selects the whole word
        assert_eq!(word_span("x 日本 y", 3), Some((2, 6)));
        assert_eq!(word_span("x 日本 y", 2), Some((2, 6)));
    }

    #[test]
    fn slice_keeps_a_wide_glyph_cut_on_its_right_half() {
        assert_eq!(slice_cells("日本語", 1, 3), "日本");
        assert_eq!(slice_cells("日本語", 2, 4), "本");
    }

    #[test]
    fn lone_escape_is_dropped_by_highlight() {
        assert_eq!(highlight_cells("a\x1bb", 9, 10), "ab");
    }
}
