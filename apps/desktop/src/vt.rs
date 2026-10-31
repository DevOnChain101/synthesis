//! Terminal grid for Mode 1 (docs/specs/desktop.md section 2): `alacritty_terminal` parses the VT
//! bytes into a cell grid, our own GPUI element paints the cells, and [`encode_key`] turns key
//! strokes back into bytes. Written from the VT/xterm behaviour, not from Zed's GPL terminal
//! crates.

use alacritty_terminal::event::VoidListener;
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::{Column, Line};
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::{Config, Term};
use alacritty_terminal::vte::ansi::{Color, Processor, Rgb};
use gpui::{App, Bounds, Hsla, Pixels, SharedString, TextAlign, TextRun, Window, font, px};

use crate::theme;

struct Size {
    cols: usize,
    rows: usize,
}

impl Dimensions for Size {
    fn total_lines(&self) -> usize {
        self.rows
    }
    fn screen_lines(&self) -> usize {
        self.rows
    }
    fn columns(&self) -> usize {
        self.cols
    }
}

/// A run of cells with one style, as painted.
#[derive(Clone, Debug, PartialEq)]
pub struct Run {
    pub text: String,
    pub fg: Hsla,
    pub bg: Option<Hsla>,
    pub cells: usize,
    pub bold: bool,
}

pub struct Grid {
    term: Term<VoidListener>,
    parser: Processor,
    cols: usize,
    rows: usize,
}

impl Grid {
    pub fn new(cols: usize, rows: usize) -> Self {
        let (cols, rows) = (cols.max(2), rows.max(1));
        Self {
            term: Term::new(Config::default(), &Size { cols, rows }, VoidListener),
            parser: Processor::new(),
            cols,
            rows,
        }
    }

    pub fn size(&self) -> (usize, usize) {
        (self.cols, self.rows)
    }

    pub fn feed(&mut self, bytes: &[u8]) {
        self.parser.advance(&mut self.term, bytes);
    }

    pub fn resize(&mut self, cols: usize, rows: usize) {
        let (cols, rows) = (cols.max(2), rows.max(1));
        if (cols, rows) != (self.cols, self.rows) {
            self.term.resize(Size { cols, rows });
            (self.cols, self.rows) = (cols, rows);
        }
    }

    /// Cursor position (column, row) on the visible screen.
    pub fn cursor(&self) -> (usize, usize) {
        let p = self.term.grid().cursor.point;
        (p.column.0, p.line.0.max(0) as usize)
    }

    /// The visible rows as text, trailing blanks removed: what a person reads in the window.
    pub fn lines(&self) -> Vec<String> {
        (0..self.rows).map(|r| self.row_text(r)).collect()
    }

    fn row_text(&self, r: usize) -> String {
        let row = &self.term.grid()[Line(r as i32)];
        let mut s = String::new();
        for c in 0..self.cols {
            let cell = &row[Column(c)];
            if !cell.flags.contains(Flags::WIDE_CHAR_SPACER) {
                s.push(cell.c);
            }
        }
        s.trim_end().to_string()
    }

    /// Row `r` as styled runs (`default_*` are the theme's text and background).
    pub fn runs(&self, r: usize) -> Vec<Run> {
        let row = &self.term.grid()[Line(r as i32)];
        let mut out: Vec<Run> = Vec::new();
        for c in 0..self.cols {
            let cell = &row[Column(c)];
            if cell.flags.contains(Flags::WIDE_CHAR_SPACER) {
                continue;
            }
            let (mut fg, mut bg) = (
                colour(cell.fg).unwrap_or_else(|| theme::hsla(theme::TEXT)),
                colour(cell.bg),
            );
            if cell.flags.contains(Flags::INVERSE) {
                (fg, bg) = (bg.unwrap_or_else(|| theme::hsla(theme::BG)), Some(fg));
            }
            let bold = cell.flags.contains(Flags::BOLD);
            let width = if cell.flags.contains(Flags::WIDE_CHAR) {
                2
            } else {
                1
            };
            match out.last_mut() {
                Some(l) if l.fg == fg && l.bg == bg && l.bold == bold => {
                    l.text.push(cell.c);
                    l.cells += width;
                }
                _ => out.push(Run {
                    text: cell.c.to_string(),
                    fg,
                    bg,
                    cells: width,
                    bold,
                }),
            }
        }
        out
    }
}

const ANSI16: [u32; 16] = [
    0x1B1F25, 0xB65E60, 0xA0BDAA, 0xD5C2AD, 0x7F9AB8, 0xB09AB8, 0x8DB5B5, 0xC6C9CE, 0x4A515C,
    0xD27F81, 0xB6D4C1, 0xE8D8C4, 0x9DB6D1, 0xC9B4D1, 0xA9D0D0, 0xEEF0F2,
];

fn rgb(rgb: Rgb) -> Hsla {
    theme::hsla(u32::from(rgb.r) << 16 | u32::from(rgb.g) << 8 | u32::from(rgb.b))
}

fn indexed(i: u8) -> Hsla {
    match i {
        0..=15 => theme::hsla(ANSI16[i as usize]),
        16..=231 => {
            let n = i - 16;
            let lvl = |v: u8| if v == 0 { 0 } else { 55 + 40 * v };
            rgb(Rgb {
                r: lvl(n / 36),
                g: lvl(n / 6 % 6),
                b: lvl(n % 6),
            })
        }
        _ => {
            let g = 8 + 10 * (i - 232);
            rgb(Rgb { r: g, g, b: g })
        }
    }
}

/// `None`: the default foreground or background (the theme's).
fn colour(c: Color) -> Option<Hsla> {
    match c {
        Color::Spec(c) => Some(rgb(c)),
        Color::Indexed(i) => Some(indexed(i)),
        Color::Named(n) => match n as usize {
            i @ 0..=15 => Some(indexed(i as u8)),
            _ => None, // foreground, background, cursor and the dim variants
        },
    }
}

/// xterm byte sequences for one key press; `None` for keys that produce nothing.
pub fn encode_key(
    key: &str,
    key_char: Option<&str>,
    ctrl: bool,
    alt: bool,
    shift: bool,
) -> Option<Vec<u8>> {
    let named: Option<&[u8]> = match key {
        "enter" => Some(b"\r"),
        "backspace" => Some(b"\x7f"),
        "tab" if shift => Some(b"\x1b[Z"),
        "tab" => Some(b"\t"),
        "escape" => Some(b"\x1b"),
        "up" => Some(b"\x1b[A"),
        "down" => Some(b"\x1b[B"),
        "right" => Some(b"\x1b[C"),
        "left" => Some(b"\x1b[D"),
        "home" => Some(b"\x1b[H"),
        "end" => Some(b"\x1b[F"),
        "pageup" => Some(b"\x1b[5~"),
        "pagedown" => Some(b"\x1b[6~"),
        "delete" => Some(b"\x1b[3~"),
        _ => None,
    };
    let mut out = match named {
        Some(b) => b.to_vec(),
        None if ctrl && key.len() == 1 && key.as_bytes()[0].is_ascii_alphabetic() => {
            vec![key.as_bytes()[0].to_ascii_lowercase() - b'a' + 1]
        }
        None => key_char.filter(|s| !s.is_empty())?.as_bytes().to_vec(),
    };
    if alt {
        out.insert(0, 0x1b);
    }
    Some(out)
}

/// Fixed-pitch text metrics shared by painting and resizing.
pub struct Metrics {
    pub cell_w: Pixels,
    pub line_h: Pixels,
}

pub fn metrics(family: &str, size: Pixels, window: &Window) -> Metrics {
    let ts = window.text_system();
    let id = ts.resolve_font(&font(family.to_string()));
    Metrics {
        cell_w: ts.em_advance(id, size).unwrap_or(size * 0.6),
        line_h: size * 1.3,
    }
}

/// Paints `rows` (styled runs per row) and a block cursor at `cursor` into `bounds`.
pub fn paint(
    rows: &[Vec<Run>],
    cursor: (usize, usize),
    family: &SharedString,
    size: Pixels,
    bounds: Bounds<Pixels>,
    window: &mut Window,
    cx: &mut App,
) {
    let m = metrics(family, size, window);
    window.paint_quad(gpui::fill(bounds, theme::hsla(theme::BG)));
    for (r, runs) in rows.iter().enumerate() {
        let y = bounds.origin.y + m.line_h * r as f32;
        let mut x = bounds.origin.x;
        for run in runs {
            let w = m.cell_w * run.cells as f32;
            if let Some(bg) = run.bg {
                window.paint_quad(gpui::fill(
                    Bounds::new(gpui::point(x, y), gpui::size(w, m.line_h)),
                    bg,
                ));
            }
            if !run.text.trim().is_empty() {
                let mut f = font(family.clone());
                if run.bold {
                    f = f.bold();
                }
                let tr = TextRun {
                    len: run.text.len(),
                    font: f,
                    color: run.fg,
                    background_color: None,
                    underline: None,
                    strikethrough: None,
                };
                let line = window.text_system().shape_line(
                    run.text.clone().into(),
                    size,
                    &[tr],
                    Some(m.cell_w),
                );
                let _ = line.paint(
                    gpui::point(x, y),
                    m.line_h,
                    TextAlign::Left,
                    None,
                    window,
                    cx,
                );
            }
            x += w;
        }
    }
    let (cc, cr) = cursor;
    let caret = Bounds::new(
        gpui::point(
            bounds.origin.x + m.cell_w * cc as f32,
            bounds.origin.y + m.line_h * cr as f32,
        ),
        gpui::size(px(2.), m.line_h),
    );
    window.paint_quad(gpui::fill(caret, theme::hsla(theme::PEARL)));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_text_and_cursor_addressing() {
        let mut g = Grid::new(20, 4);
        g.feed(b"hello\r\nworld");
        g.feed(b"\x1b[1;3HXY");
        assert_eq!(g.lines(), ["heXYo", "world", "", ""]);
        assert_eq!(g.cursor(), (4, 0));
    }

    #[test]
    fn clear_and_erase_in_line() {
        let mut g = Grid::new(10, 3);
        g.feed(b"abcdef\x1b[1;3H\x1b[K");
        assert_eq!(g.lines()[0], "ab");
        g.feed(b"\x1b[2J\x1b[1;1Hnew");
        assert_eq!(g.lines(), ["new", "", ""]);
    }

    #[test]
    fn colours_make_separate_runs() {
        let mut g = Grid::new(10, 1);
        g.feed(b"a\x1b[38;2;10;20;30mb\x1b[0mc");
        let runs = g.runs(0);
        assert_eq!(
            runs[..2]
                .iter()
                .map(|r| r.text.as_str())
                .collect::<Vec<_>>(),
            ["a", "b"]
        );
        assert!(runs[2].text.starts_with('c'));
        assert_eq!(runs[1].fg, theme::hsla(0x0A141E));
        assert_eq!(runs[0].fg, runs[2].fg);
    }

    #[test]
    fn wide_characters_take_two_cells() {
        let mut g = Grid::new(6, 1);
        g.feed("a世b".as_bytes());
        assert_eq!(g.lines()[0], "a世b");
        // every cell of the 6-column row is accounted for: a, wide(2), b and two blanks
        assert_eq!(g.runs(0).iter().map(|r| r.cells).sum::<usize>(), 6);
        assert!(g.runs(0)[0].text.starts_with("a世b"));
    }

    #[test]
    fn resize_keeps_content() {
        let mut g = Grid::new(10, 3);
        g.feed(b"abc");
        g.resize(20, 5);
        assert_eq!(g.size(), (20, 5));
        assert_eq!(g.lines()[0], "abc");
    }

    #[test]
    fn keys_encode_as_xterm() {
        let k = |key, ch, c, a, s| encode_key(key, ch, c, a, s);
        assert_eq!(k("enter", None, false, false, false), Some(b"\r".to_vec()));
        assert_eq!(k("up", None, false, false, false), Some(b"\x1b[A".to_vec()));
        assert_eq!(k("tab", None, false, false, true), Some(b"\x1b[Z".to_vec()));
        assert_eq!(k("c", Some("c"), true, false, false), Some(vec![3]));
        assert_eq!(
            k("x", Some("x"), false, true, false),
            Some(b"\x1bx".to_vec())
        );
        assert_eq!(
            k("a", Some("é"), false, false, false),
            Some("é".as_bytes().to_vec())
        );
        assert_eq!(k("shift", None, false, false, true), None);
    }
}
