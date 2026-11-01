//! Flicker-free terminal output (port of `screen.js`): only changed lines are rewritten, each
//! update is wrapped in synchronized output (DEC 2026), autowrap is off so a full-width last
//! line never scrolls, and a size change clears and repaints inside the same block.

use crate::sgr::{ColorMode, encode};
use std::io::{self, Write};

/// Alt screen, hide cursor, no autowrap, mouse 1000 (buttons) + 1002 (drag while held, used for
/// text selection) + 1006 (SGR coordinates), clear, home.
pub const ENTER: &str =
    "\x1b[?1049h\x1b[?25l\x1b[?7l\x1b[?1000h\x1b[?1002h\x1b[?1006h\x1b[2J\x1b[H";
pub const TITLE: &str = "✻ Synthesis · MISSION CONTROL";
/// xterm title stack: push the old title on entering, pop it on leaving.
pub const PUSH_TITLE: &str = "\x1b[22;2t";
pub const POP_TITLE: &str = "\x1b[23;2t";
pub const SYNC_START: &str = "\x1b[?2026h";
pub const SYNC_END: &str = "\x1b[?2026l";
/// Bracketed paste: pastes arrive as one `Key::Paste`.
pub const BRACKETED_PASTE_ON: &str = "\x1b[?2004h";
pub const BRACKETED_PASTE_OFF: &str = "\x1b[?2004l";

/// OSC 2 window title; control characters become spaces.
pub fn set_title(title: &str) -> String {
    let clean: String = title
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    format!("\x1b]2;{clean}\x07")
}

/// Everything [`ENTER`] and the app switched on, off again. Pure so the crash handler, the
/// [`crate::terminal::TerminalGuard`] and the tests share it.
pub fn terminal_restore_sequence() -> String {
    format!(
        "\x1b[0m\x1b[?7h\x1b[?25h\x1b[?1006l\x1b[?1002l\x1b[?1000l{BRACKETED_PASTE_OFF}\x1b[?1049l{POP_TITLE}"
    )
}

/// Whether a cursor-position report (the terminal's real size) should replace the size we hold:
/// `Some((columns, rows))`, or `None` for an implausible size (the same bytes are also
/// ctrl/shift+F3, `ESC[1;<mod>R`) or an unchanged one.
pub fn adoptable_size(col: u32, row: u32, current: (u32, u32)) -> Option<(u32, u32)> {
    let plausible = (20..=1000).contains(&col) && (5..=500).contains(&row);
    (plausible && (col, row) != current).then_some((col, row))
}

pub struct Screen<W: Write> {
    out: W,
    color_mode: ColorMode,
    prev: Vec<String>,
    cols: u32,
    rows: u32,
    active: bool,
    force_next: u32,
    title: String,
    /// Where the hardware cursor should be (0-based `(x, y)`); `None` hides it.
    caret: Option<(u32, u32)>,
    caret_shown: Option<(u32, u32)>,
    /// Post-processes each encoded line (OSC 8 hyperlinks); the row cache compares the plain lines.
    linker: Option<Linker>,
}

pub type Linker = Box<dyn Fn(&str) -> String + Send>;

impl<W: Write> Screen<W> {
    pub fn new(out: W, color_mode: ColorMode) -> Self {
        Self {
            out,
            color_mode,
            prev: Vec::new(),
            cols: 0,
            rows: 0,
            active: false,
            force_next: 0,
            title: TITLE.into(),
            caret: None,
            caret_shown: None,
            linker: None,
        }
    }

    pub fn set_linker(&mut self, linker: Option<Linker>) {
        self.linker = linker;
    }

    /// Parks the real cursor at `caret` after the next `draw` (so an IME places its candidate window
    /// at the prompt); `None` keeps it hidden.
    pub fn set_caret(&mut self, caret: Option<(u32, u32)>) {
        self.caret = caret;
    }

    pub fn enter(&mut self) -> io::Result<()> {
        self.active = true;
        write!(self.out, "{PUSH_TITLE}{}{ENTER}", set_title(&self.title))?;
        self.out.flush()
    }

    pub fn set_title(&mut self, title: &str) -> io::Result<()> {
        self.title = if title.is_empty() {
            TITLE.into()
        } else {
            title.into()
        };
        if self.active {
            self.out.write_all(set_title(&self.title).as_bytes())?;
            self.out.flush()?;
        }
        Ok(())
    }

    /// Restores the terminal; a second call writes nothing.
    pub fn leave(&mut self) -> io::Result<()> {
        if !self.active {
            return Ok(());
        }
        self.active = false;
        self.out.write_all(terminal_restore_sequence().as_bytes())?;
        self.out.flush()
    }

    pub fn invalidate(&mut self) {
        self.caret_shown = None;
        self.prev.clear();
    }

    /// The terminal reported (or might have) a size change: drop the diff base, force one more
    /// full frame, clear screen and scrollback and re-assert alt screen / no autowrap / no region.
    pub fn resize(&mut self) -> io::Result<()> {
        self.prev.clear();
        self.force_next = 1;
        if self.active {
            self.out
                .write_all(b"\x1b[2J\x1b[3J\x1b[H\x1b[?1049h\x1b[?7l\x1b[r")?;
            self.out.flush()?;
        }
        Ok(())
    }

    /// User-triggered "reload terminal" (ctrl+l): leave and re-enter the alt screen, clear
    /// scrollback and force the next two frames full.
    pub fn reset(&mut self) -> io::Result<()> {
        self.prev.clear();
        self.cols = 0; // the next draw() sees a size change and clears
        self.rows = 0;
        self.force_next = 2;
        if self.active {
            write!(
                self.out,
                "\x1b[0m\x1b[r\x1b[?1049l{}{ENTER}\x1b[3J\x1b[H",
                set_title(&self.title)
            )?;
            self.out.flush()?;
        }
        Ok(())
    }

    /// `lines` are already fitted to `cols` cells.
    pub fn draw(&mut self, lines: &[String], cols: u32, rows: u32) -> io::Result<()> {
        let mut buf = String::new();
        let mut full = false;
        if cols != self.cols || rows != self.rows {
            self.cols = cols;
            self.rows = rows;
            full = true;
            buf.push_str("\x1b[2J\x1b[H");
        } else if self.force_next > 0 {
            full = true;
        }
        if full {
            self.prev.clear();
            self.force_next = self.force_next.saturating_sub(1);
        }
        for i in 0..rows as usize {
            let line = lines.get(i).map_or("", String::as_str);
            if !full && self.prev.get(i).is_some_and(|p| p == line) {
                continue;
            }
            let mut enc = encode(line, self.color_mode);
            if let Some(link) = &self.linker {
                enc = link(&enc);
            }
            buf.push_str(&format!("\x1b[{};1H{enc}\x1b[K", i + 1));
        }
        self.prev = lines.iter().take(rows as usize).cloned().collect();
        let caret = self.caret.filter(|(x, y)| *x < cols && *y < rows);
        // Writing rows moves the cursor, so it is re-parked after every repaint.
        match caret {
            Some((x, y)) if !buf.is_empty() || caret != self.caret_shown => {
                buf.push_str(&format!("\x1b[{};{}H\x1b[?25h", y + 1, x + 1));
            }
            None if self.caret_shown.is_some() => buf.push_str("\x1b[?25l"),
            _ => {}
        }
        self.caret_shown = caret;
        if buf.is_empty() {
            return Ok(());
        }
        write!(self.out, "{SYNC_START}{buf}{SYNC_END}")?;
        self.out.flush()
    }

    pub fn into_inner(self) -> W {
        self.out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn screen() -> Screen<Vec<u8>> {
        Screen::new(Vec::new(), ColorMode::TrueColor)
    }
    fn take(s: &mut Screen<Vec<u8>>) -> String {
        String::from_utf8(std::mem::take(&mut s.out)).unwrap()
    }

    // Translated from Node test/screen-leave.test.js.
    #[test]
    fn caret_is_parked_after_each_repaint_and_hidden_without_one() {
        let mut s = screen();
        s.set_caret(Some((4, 2)));
        s.draw(&["a".into(), "b".into(), "c".into()], 10, 3)
            .unwrap();
        assert!(take(&mut s).ends_with("\x1b[3;5H\x1b[?25h\x1b[?2026l"));
        s.draw(&["a".into(), "b".into(), "c".into()], 10, 3)
            .unwrap();
        assert_eq!(
            take(&mut s),
            "",
            "unchanged frame and caret: nothing written"
        );
        s.set_caret(None);
        s.draw(&["a".into(), "b".into(), "c".into()], 10, 3)
            .unwrap();
        assert!(take(&mut s).contains("\x1b[?25l"));
    }

    #[test]
    fn leave_turns_bracketed_paste_off_once() {
        let mut s = screen();
        s.enter().unwrap();
        take(&mut s);
        s.leave().unwrap();
        assert!(take(&mut s).contains(BRACKETED_PASTE_OFF));
        s.leave().unwrap();
        assert_eq!(take(&mut s), "");
    }

    #[test]
    fn clears_always_home_the_cursor() {
        let mut s = screen();
        s.enter().unwrap();
        assert!(take(&mut s).contains("\x1b[2J\x1b[H"));
        s.draw(&["aaa".into()], 3, 1).unwrap();
        assert!(take(&mut s).contains("\x1b[2J\x1b[H"));
        s.resize().unwrap();
        assert!(take(&mut s).contains("\x1b[2J\x1b[3J\x1b[H"));
    }

    #[test]
    fn only_changed_lines_are_rewritten() {
        let mut s = screen();
        let a = vec!["one".to_string(), "two".to_string()];
        s.draw(&a, 10, 2).unwrap();
        take(&mut s);
        s.draw(&["one".into(), "TWO".into()], 10, 2).unwrap();
        let out = take(&mut s);
        assert!(!out.contains("one") && out.contains("\x1b[2;1HTWO\x1b[K"));
        assert!(out.starts_with(SYNC_START) && out.ends_with(SYNC_END));
        s.draw(&["one".into(), "TWO".into()], 10, 2).unwrap();
        assert_eq!(take(&mut s), "");
    }

    #[test]
    fn draw_never_emits_foreign_escapes_even_across_wrap_boundaries() {
        let mut s = screen();
        // an OSC 52 split over two rows, plus injected controls in the middle of a row
        let lines = vec![
            "\x1b[31mhi\x1b]52;c;".to_string(),
            "ZXZpbA==\x07\x1b[6n\u{9b}6n\r\x08x".to_string(),
        ];
        s.draw(&lines, 40, 2).unwrap();
        let out = take(&mut s);
        let body = out
            .replace(SYNC_START, "")
            .replace(SYNC_END, "")
            .replace("\x1b[2J\x1b[H", "")
            .replace("\x1b[38;5;1m", "")
            .replace("\x1b[0m", "")
            .replace("\x1b[K", "")
            .replace("\x1b[1;1H", "")
            .replace("\x1b[2;1H", "");
        assert!(!body.chars().any(|c| c.is_control()), "{out:?}");
    }

    #[test]
    fn title_and_size_helpers() {
        assert_eq!(set_title("a\nb\x07c"), "\x1b]2;a b c\x07");
        assert_eq!(set_title("a\u{9c}b\u{9d}c\x7f\x1b"), "\x1b]2;a b c  \x07");
        assert_eq!(adoptable_size(10, 10, (80, 24)), None);
        assert_eq!(adoptable_size(80, 24, (80, 24)), None);
        assert_eq!(adoptable_size(120, 40, (80, 24)), Some((120, 40)));
    }
}
