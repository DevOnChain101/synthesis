//! Character-precise text selection of the file explorer's preview. Pure logic (no GPUI): the preview is
//! monospace, so a pixel x maps to a cell by the font's advance; a cell maps to a char by walking the line
//! (a tab runs to the next stop, a wide char takes two cells). Columns are char indices, never byte offsets.
//! shortcut: wide chars are assumed to be exactly two cells (a fallback font may differ a little), combining
//! marks one cell; upgrade to the real shaped layout if that drifts visibly.

/// Fira Code's advance (0.6 em) at the preview's 12.5 px.
pub const FONT_PX: f32 = 12.5;
pub const ADVANCE: f32 = FONT_PX * 0.6;
const TAB: usize = 4;

/// A point between chars: 0-based line, char column (0..=chars of the line).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub struct Pos {
    pub line: usize,
    pub col: usize,
}

/// Anchor (where the press was) and head (where the pointer is now), in either order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Sel {
    pub anchor: Pos,
    pub head: Pos,
}

impl Sel {
    /// Start and end, in file order.
    pub fn range(&self) -> (Pos, Pos) {
        (self.anchor.min(self.head), self.anchor.max(self.head))
    }

    pub fn is_empty(&self) -> bool {
        self.anchor == self.head
    }

    /// Whole lines `a..=b` (0-based, either order).
    pub fn lines(lines: &[String], a: usize, b: usize) -> Self {
        let (a, b) = (a.min(b), a.max(b).min(lines.len().saturating_sub(1)));
        Self {
            anchor: Pos { line: a, col: 0 },
            head: Pos {
                line: b,
                col: chars(lines.get(b)),
            },
        }
    }

    /// The whole file.
    pub fn all(lines: &[String]) -> Self {
        Self::lines(lines, 0, lines.len().saturating_sub(1))
    }

    /// The columns of line `i` this selection covers (`None`: none).
    pub fn span(&self, i: usize, len: usize) -> Option<(usize, usize)> {
        let (s, e) = self.range();
        if i < s.line || i > e.line {
            return None;
        }
        let a = if i == s.line { s.col.min(len) } else { 0 };
        let b = if i == e.line { e.col.min(len) } else { len };
        (a < b).then_some((a, b))
    }

    /// Whether `p` lies inside the selection.
    pub fn contains(&self, p: Pos) -> bool {
        let (s, e) = self.range();
        s <= p && p < e
    }
}

fn chars(l: Option<&String>) -> usize {
    l.map_or(0, |l| l.chars().count())
}

fn wide(c: char) -> bool {
    matches!(c as u32,
        0x1100..=0x115F | 0x2E80..=0xA4CF | 0xAC00..=0xD7A3 | 0xF900..=0xFAFF | 0xFE30..=0xFE6F
        | 0xFF00..=0xFF60 | 0xFFE0..=0xFFE6 | 0x1F300..=0x1FAFF | 0x20000..=0x3FFFD)
}

/// Cells char `c` takes when it starts at cell `at`.
fn cells(c: char, at: usize) -> usize {
    match c {
        '\t' => TAB - at % TAB,
        c if wide(c) => 2,
        _ => 1,
    }
}

/// The line as drawn: tabs expanded to spaces (the copy keeps the tab).
pub fn display(line: &str) -> String {
    let (mut out, mut at) = (String::with_capacity(line.len()), 0);
    for c in line.chars() {
        let n = cells(c, at);
        if c == '\t' {
            out.extend(std::iter::repeat_n(' ', n));
        } else {
            out.push(c);
        }
        at += n;
    }
    out
}

/// The cell where char `col` starts.
pub fn cell_of(line: &str, col: usize) -> usize {
    line.chars().take(col).fold(0, |at, c| at + cells(c, at))
}

/// The cells chars `a..b` cover.
pub fn width(line: &str, a: usize, b: usize) -> usize {
    cell_of(line, b) - cell_of(line, a)
}

/// The char column for a pixel `x` from the text's left edge: `nearest` gives the char boundary closest to
/// `x` (a caret), else the char under `x` (clamped to the last char).
pub fn col_at(line: &str, x: f32, nearest: bool) -> usize {
    let target = x.max(0.) / ADVANCE;
    let (mut at, mut n) = (0usize, 0usize);
    for c in line.chars() {
        let w = cells(c, at);
        let hit = if nearest {
            target < (at as f32) + (w as f32) / 2.
        } else {
            target < (at + w) as f32
        };
        if hit {
            return n;
        }
        at += w;
        n += 1;
    }
    if nearest { n } else { n.saturating_sub(1) }
}

fn class(c: char) -> u8 {
    if c.is_alphanumeric() || c == '_' {
        0
    } else if c.is_whitespace() {
        1
    } else {
        2
    }
}

/// The run of same-class chars (word, blanks or punctuation) around char `col`.
pub fn word_at(line: &str, col: usize) -> (usize, usize) {
    let cs: Vec<char> = line.chars().collect();
    if cs.is_empty() {
        return (0, 0);
    }
    let col = col.min(cs.len() - 1);
    let k = class(cs[col]);
    let a = cs[..col]
        .iter()
        .rposition(|c| class(*c) != k)
        .map_or(0, |i| i + 1);
    let b = cs[col..]
        .iter()
        .position(|c| class(*c) != k)
        .map_or(cs.len(), |i| col + i);
    (a, b)
}

fn byte(line: &str, col: usize) -> usize {
    line.char_indices().nth(col).map_or(line.len(), |(b, _)| b)
}

/// The selected text of each line it touches.
pub fn pieces(lines: &[String], s: &Sel) -> Vec<String> {
    let (a, z) = s.range();
    if s.is_empty() || a.line >= lines.len() {
        return vec![];
    }
    let z_line = z.line.min(lines.len() - 1);
    (a.line..=z_line)
        .map(|i| {
            let l = &lines[i];
            let from = if i == a.line { byte(l, a.col) } else { 0 };
            let to = if i == z.line { byte(l, z.col) } else { l.len() };
            l[from..to].to_string()
        })
        .collect()
}

/// The selected text, lines joined with `\n` (no gutter, no line numbers).
pub fn extract(lines: &[String], s: &Sel) -> String {
    pieces(lines, s).join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(line: usize, col: usize) -> Pos {
        Pos { line, col }
    }
    fn sel(a: Pos, h: Pos) -> Sel {
        Sel { anchor: a, head: h }
    }
    fn file() -> Vec<String> {
        ["fn main() {", "\tlet s = \"héllo 日本\";", "", "}"]
            .map(String::from)
            .to_vec()
    }

    #[test]
    fn pixels_map_to_the_char_under_or_nearest_to_them() {
        assert_eq!(col_at("abcd", -5., true), 0);
        assert_eq!(col_at("abcd", ADVANCE * 1.4, true), 1);
        assert_eq!(col_at("abcd", ADVANCE * 1.6, true), 2);
        assert_eq!(col_at("abcd", ADVANCE * 1.6, false), 1);
        assert_eq!(col_at("abcd", 9999., true), 4);
        assert_eq!(col_at("abcd", 9999., false), 3);
        assert_eq!(col_at("", 50., true), 0);
        assert_eq!(col_at("", 50., false), 0);
    }

    #[test]
    fn tabs_and_wide_chars_take_their_cells() {
        assert_eq!(cell_of("\tx", 1), 4);
        assert_eq!(cell_of("a\tx", 2), 4);
        assert_eq!(display("a\tb"), "a   b");
        // "日本x": the pointer on the second wide char is char 1, on x char 2
        assert_eq!(col_at("日本x", ADVANCE * 2.5, false), 1);
        assert_eq!(col_at("日本x", ADVANCE * 4.5, false), 2);
        assert_eq!(width("日本x", 0, 2), 4);
        assert_eq!(col_at("\tx", ADVANCE * 4.2, false), 1);
    }

    #[test]
    fn a_drag_in_either_direction_extracts_the_same_text() {
        let f = file();
        let fwd = sel(p(0, 3), p(1, 5));
        let back = sel(p(1, 5), p(0, 3));
        assert_eq!(extract(&f, &fwd), "main() {\n\tlet ");
        assert_eq!(extract(&f, &back), extract(&f, &fwd));
        assert_eq!(fwd.range(), back.range());
    }

    #[test]
    fn multibyte_chars_are_cut_on_char_boundaries() {
        let f = file();
        assert_eq!(extract(&f, &sel(p(1, 10), p(1, 16))), "héllo ");
        assert_eq!(extract(&f, &sel(p(1, 16), p(1, 18))), "日本");
        // columns past the end are clamped, never a panic
        assert_eq!(extract(&f, &sel(p(1, 16), p(1, 999))), "日本\";");
        assert_eq!(extract(&f, &sel(p(3, 0), p(99, 5))), "}");
    }

    #[test]
    fn an_empty_selection_extracts_nothing() {
        let f = file();
        assert!(sel(p(0, 2), p(0, 2)).is_empty());
        assert_eq!(extract(&f, &sel(p(0, 2), p(0, 2))), "");
        assert!(pieces(&f, &sel(p(9, 0), p(9, 3))).is_empty());
        assert_eq!(extract(&f, &sel(p(2, 0), p(2, 0))), "");
    }

    #[test]
    fn all_and_lines_cover_whole_lines() {
        let f = file();
        assert_eq!(extract(&f, &Sel::all(&f)), f.join("\n"));
        assert_eq!(
            extract(&f, &Sel::lines(&f, 1, 0)),
            "fn main() {\n\tlet s = \"héllo 日本\";"
        );
        assert_eq!(extract(&[], &Sel::all(&[])), "");
    }

    #[test]
    fn spans_and_words() {
        let s = sel(p(0, 3), p(2, 0));
        assert_eq!(s.span(0, 11), Some((3, 11)));
        assert_eq!(s.span(1, 20), Some((0, 20)));
        assert_eq!(s.span(2, 0), None);
        assert_eq!(s.span(3, 1), None);
        assert!(s.contains(p(1, 4)) && !s.contains(p(0, 2)) && !s.contains(p(2, 0)));
        assert_eq!(word_at("let héllo_x = 1", 6), (4, 11));
        assert_eq!(word_at("a  b", 1), (1, 3));
        assert_eq!(word_at("a.b", 1), (1, 2));
        assert_eq!(word_at("abc", 99), (0, 3));
        assert_eq!(word_at("", 0), (0, 0));
    }
}
