//! One personal shell: the PTY, the `Term` the view draws, a reader thread and a bounded write queue.
//! The UI thread never waits on the shell: input is `try_send` into a queue that one writer thread drains, the
//! reader thread parses into the `Term` under a lock the view holds only to copy the visible cells, and it
//! wakes the view at most once per repaint (coalesced; the view caps repaints at 60 per second).

use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, SyncSender, TrySendError};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use alacritty_terminal::event::{Event, EventListener};
use alacritty_terminal::grid::{Dimensions, Scroll};
use alacritty_terminal::index::{Column, Point, Side};
use alacritty_terminal::selection::{Selection, SelectionType};
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::color::Colors;
use alacritty_terminal::term::{
    Config, Osc52, Term, TermMode, point_to_viewport, viewport_to_point,
};
use alacritty_terminal::vte::ansi::{Color, CursorShape, Processor};
use portable_pty::{Child, CommandBuilder, MasterPty, PtySize, native_pty_system};
use synthesis_cli_ui_render::theme as t;

use super::launch::Launch;

type Rgb = t::Rgb;
type Parser = Processor;

/// Scrollback lines (spec 1.7) and the size limits shared with the engine's PTYs.
pub const SCROLLBACK: usize = 5000;
const MAX_COLS: u16 = synthesis_terminal::MAX_COLS;
const MAX_ROWS: u16 = synthesis_terminal::MAX_ROWS;
/// The write queue: 16 chunks of at most 64 KiB.
const QUEUE: usize = 16;
pub const CHUNK: usize = 64 << 10;

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Terminal events of a personal shell. A title is sanitised (spec 1.10). OSC 52 clipboard store and load,
/// colour and size queries and the bell are ignored (a program must not read or write the person's
/// clipboard); the only thing written back is `Reply`'s two-query allow-list (spec 1.12).
pub struct Listener {
    reply: synthesis_terminal::Reply,
    title: Arc<Mutex<String>>,
}

impl EventListener for Listener {
    fn send_event(&self, event: Event) {
        match event {
            Event::Title(raw) => *lock(&self.title) = super::sanitize_title(&raw),
            Event::ResetTitle => lock(&self.title).clear(),
            Event::ClipboardStore(..)
            | Event::ClipboardLoad(..)
            | Event::ColorRequest(..)
            | Event::TextAreaSizeRequest(..)
            | Event::Bell => {}
            other => self.reply.send_event(other),
        }
    }
}

/// The `Term` configuration of a personal shell: OSC 52 off in both directions.
pub fn config() -> Config {
    Config {
        scrolling_history: SCROLLBACK,
        osc52: Osc52::Disabled,
        ..Config::default()
    }
}

struct Dims(usize, usize);

impl Dimensions for Dims {
    fn total_lines(&self) -> usize {
        self.1
    }
    fn screen_lines(&self) -> usize {
        self.1
    }
    fn columns(&self) -> usize {
        self.0
    }
}

/// One cell as drawn: colours resolved (inverse, hidden and dim applied), the character, the bits the painter needs.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Cell {
    pub ch: char,
    pub fg: Rgb,
    pub bg: Rgb,
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
    pub strike: bool,
    pub wide: bool,
    /// The cell belongs to the selection (exactly the selected characters).
    pub selected: bool,
    /// An OSC 8 hyperlink.
    pub link: bool,
}

pub const DEFAULT_FG: Rgb = t::SILVER;
pub const DEFAULT_BG: Rgb = t::BG;

/// The visible screen, copied out under the lock.
pub struct Snap {
    pub cols: usize,
    pub rows: usize,
    pub cells: Vec<Cell>,
    /// Column, row and shape of the cursor when it shows.
    pub cursor: Option<(usize, usize, CursorShape)>,
    pub mode: TermMode,
    /// Lines the view is scrolled back, and the lines of history.
    pub offset: usize,
    pub history: usize,
}

/// The 16 ANSI colours on the TUI palette, then the xterm 6x6x6 cube and the grey ramp.
pub fn ansi(i: usize) -> Rgb {
    const BASE: [Rgb; 16] = [
        t::ELEVATED,
        t::ERROR,
        t::SUCCESS,
        t::WARN,
        t::INFO,
        [176, 146, 196],
        t::CYAN,
        t::TEXT,
        t::MUTED,
        t::DIFF_REMOVE,
        t::DIFF_ADD,
        t::ACCENT_LIGHT,
        [160, 190, 220],
        [204, 176, 220],
        [164, 210, 214],
        t::PEARL,
    ];
    match i {
        0..=15 => BASE[i],
        16..=231 => {
            let n = i - 16;
            let lv = |v: usize| if v == 0 { 0 } else { (55 + 40 * v) as u8 };
            [lv(n / 36), lv(n / 6 % 6), lv(n % 6)]
        }
        _ => {
            let g = (8 + 10 * (i.min(255) - 232)) as u8;
            [g, g, g]
        }
    }
}

fn mix(a: Rgb, b: Rgb, k: f32) -> Rgb {
    crate::theme::mix(a, b, k)
}

/// A colour of the grid: palette overrides set by the program (OSC 4, 10, 11) win; bold brightens 0..=7.
pub fn resolve(c: Color, colors: &Colors, fg: bool, bold: bool) -> Rgb {
    let pick = |i: usize| -> Rgb {
        if let Some(o) = colors[i] {
            return [o.r, o.g, o.b];
        }
        match i {
            0..=7 if fg && bold => ansi(i + 8),
            0..=255 => ansi(i),
            256 | 267 => DEFAULT_FG,
            257 => DEFAULT_BG,
            258 => t::ACCENT,
            259..=266 => mix(ansi(i - 259), DEFAULT_BG, 0.4),
            _ => mix(DEFAULT_FG, DEFAULT_BG, 0.4),
        }
    };
    match c {
        Color::Spec(r) => [r.r, r.g, r.b],
        Color::Indexed(i) => pick(usize::from(i)),
        Color::Named(n) => pick(n as usize),
    }
}

/// The fg and bg of a cell after inverse, hidden and dim.
fn cell_colors(fg: Color, bg: Color, flags: Flags, colors: &Colors) -> (Rgb, Rgb) {
    let bold = flags.contains(Flags::BOLD);
    let (mut f, mut b) = (
        resolve(fg, colors, true, bold),
        resolve(bg, colors, false, false),
    );
    if flags.contains(Flags::INVERSE) {
        std::mem::swap(&mut f, &mut b);
    }
    if flags.contains(Flags::DIM) {
        f = mix(f, b, 0.4);
    }
    if flags.contains(Flags::HIDDEN) {
        f = b;
    }
    (f, b)
}

/// A shell on a PTY, or a scripted screen (`--shot --pty-script`, tests) with no process.
pub struct Session {
    term: Arc<Mutex<Term<Listener>>>,
    title: Arc<Mutex<String>>,
    writer: Option<SyncSender<Vec<u8>>>,
    master: Mutex<Option<Box<dyn MasterPty + Send>>>,
    child: Option<Arc<Mutex<Box<dyn Child + Send + Sync>>>>,
    exit: Arc<Mutex<Option<String>>>,
    /// Set by the reader after new output; cleared by the view when it repaints.
    pub dirty: Arc<AtomicBool>,
    size: Mutex<(u16, u16)>,
    pub label: String,
}

fn new_term(
    cols: u16,
    rows: u16,
    tx: SyncSender<Vec<u8>>,
) -> (Arc<Mutex<Term<Listener>>>, Arc<Mutex<String>>) {
    let title = Arc::new(Mutex::new(String::new()));
    let listener = Listener {
        reply: synthesis_terminal::Reply::new(tx),
        title: title.clone(),
    };
    let term = Term::new(config(), &Dims(cols.into(), rows.into()), listener);
    (Arc::new(Mutex::new(term)), title)
}

fn clamp(cols: u16, rows: u16) -> (u16, u16) {
    (cols.clamp(2, MAX_COLS), rows.clamp(1, MAX_ROWS))
}

impl Session {
    /// A screen fed with `bytes` and no process (a fixed byte script).
    pub fn scripted(cols: u16, rows: u16, bytes: &[u8], label: &str) -> Self {
        let (cols, rows) = clamp(cols, rows);
        let (tx, _) = mpsc::sync_channel(1);
        let (term, title) = new_term(cols, rows, tx);
        Parser::default().advance(&mut *lock(&term), bytes);
        Self {
            term,
            title,
            writer: None,
            master: Mutex::new(None),
            child: None,
            exit: Arc::new(Mutex::new(None)),
            dirty: Arc::new(AtomicBool::new(false)),
            size: Mutex::new((cols, rows)),
            label: label.into(),
        }
    }

    /// Starts the shell. `wake` is called (from the reader thread) after output, at most once until `dirty` is
    /// cleared again.
    pub fn spawn(
        l: &Launch,
        cols: u16,
        rows: u16,
        wake: Arc<dyn Fn() + Send + Sync>,
    ) -> Result<Self, String> {
        #[cfg(windows)]
        {
            // portable-pty loads `conpty.dll` by bare name: restrict the DLL search path first (process-wide, once)
            static DLLS: std::sync::OnceLock<Result<(), String>> = std::sync::OnceLock::new();
            DLLS.get_or_init(synthesis_sandbox_win::harden_dll_search)
                .clone()
                .map_err(|e| format!("cannot restrict the DLL search path: {e}"))?;
        }
        let (cols, rows) = clamp(cols, rows);
        let size = PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        };
        let pair = native_pty_system()
            .openpty(size)
            .map_err(|e| format!("cannot open a pseudo terminal: {e}"))?;
        let mut b = CommandBuilder::new(&l.program);
        b.env_clear(); // portable-pty starts from this process's environment
        b.args(&l.args);
        for (k, v) in &l.env {
            b.env(k, v);
        }
        if let Some(d) = &l.cwd {
            b.cwd(d);
        }
        let child = pair
            .slave
            .spawn_command(b)
            .map_err(|e| format!("cannot start {}: {e}", l.program.display()))?;
        drop(pair.slave); // else the reader never sees EOF
        let mut reader = pair
            .master
            .try_clone_reader()
            .map_err(|e| format!("pseudo terminal: {e}"))?;
        let mut out = pair
            .master
            .take_writer()
            .map_err(|e| format!("pseudo terminal: {e}"))?;
        let (tx, rx) = mpsc::sync_channel::<Vec<u8>>(QUEUE);
        let (term, title) = new_term(cols, rows, tx.clone());
        let child = Arc::new(Mutex::new(child));
        let exit = Arc::new(Mutex::new(None));
        let dirty = Arc::new(AtomicBool::new(false));
        let ping = {
            let (dirty, wake) = (dirty.clone(), wake);
            move || {
                if !dirty.swap(true, Ordering::AcqRel) {
                    wake();
                }
            }
        };
        std::thread::Builder::new()
            .name("synt-term-write".into())
            .spawn(move || {
                while let Ok(data) = rx.recv() {
                    if out.write_all(&data).and_then(|()| out.flush()).is_err() {
                        break;
                    }
                }
            })
            .map_err(|e| e.to_string())?;
        let (rt, rchild, rexit, rping) = (term.clone(), child.clone(), exit.clone(), ping);
        std::thread::Builder::new()
            .name("synt-term-read".into())
            .spawn(move || {
                let mut parser = Parser::default();
                let mut buf = vec![0u8; CHUNK];
                loop {
                    match reader.read(&mut buf) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            parser.advance(&mut *lock(&rt), &buf[..n]);
                            rping();
                        }
                    }
                }
                // the output ended: say how the shell ended (it may need a moment to be reaped)
                let end = Instant::now() + Duration::from_secs(20);
                let status = loop {
                    if let Ok(Some(s)) = lock(&rchild).try_wait() {
                        break Some(s.exit_code());
                    }
                    if Instant::now() >= end {
                        break None;
                    }
                    std::thread::sleep(Duration::from_millis(20));
                };
                *lock(&rexit) = Some(match status {
                    Some(0) => "The shell has exited.".to_string(),
                    None => {
                        "The shell's output ended, but it has not reported its exit.".to_string()
                    }
                    Some(c) => format!("The shell has exited with code {c}."),
                });
                rping();
            })
            .map_err(|e| e.to_string())?;
        Ok(Self {
            term,
            title,
            writer: Some(tx),
            master: Mutex::new(Some(pair.master)),
            child: Some(child),
            exit,
            dirty,
            size: Mutex::new((cols, rows)),
            label: l.label.clone(),
        })
    }

    /// A program other than the shell runs in the foreground (unix; Windows cannot tell).
    pub fn busy(&self) -> bool {
        #[cfg(unix)]
        if let (Some(m), Some(c)) = (lock(&self.master).as_ref(), &self.child) {
            let shell = lock(c).process_id().and_then(|p| i32::try_from(p).ok());
            return shell.is_some() && m.process_group_leader().is_some_and(|g| Some(g) != shell);
        }
        false
    }

    /// How the shell ended, once it has.
    pub fn exited(&self) -> Option<String> {
        lock(&self.exit).clone()
    }

    pub fn title(&self) -> String {
        lock(&self.title).clone()
    }

    /// Queues `data` for the shell without ever blocking. Fails when the shell has ended or does not read.
    pub fn write(&self, data: Vec<u8>) -> Result<(), String> {
        let tx = self.writer.as_ref().ok_or("this terminal has no shell")?;
        match tx.try_send(data) {
            Ok(()) => Ok(()),
            Err(TrySendError::Full(_)) => Err("the shell is not reading its input".into()),
            Err(TrySendError::Disconnected(_)) => Err("the shell has ended".into()),
        }
    }

    /// Typed or pasted bytes, in chunks the queue takes; the view goes back to the bottom.
    pub fn send(&self, data: &[u8]) -> Result<(), String> {
        self.with_term(|t| t.scroll_display(Scroll::Bottom));
        data.chunks(CHUNK).try_for_each(|c| self.write(c.to_vec()))
    }

    pub fn resize(&self, cols: u16, rows: u16) {
        let (cols, rows) = clamp(cols, rows);
        let mut size = lock(&self.size);
        if *size == (cols, rows) {
            return;
        }
        *size = (cols, rows);
        if let Some(m) = lock(&self.master).as_ref() {
            let _ = m.resize(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            });
        }
        lock(&self.term).resize(Dims(cols.into(), rows.into()));
    }

    pub fn with_term<R>(&self, f: impl FnOnce(&mut Term<Listener>) -> R) -> R {
        f(&mut lock(&self.term))
    }

    /// Scrolls the view by `lines` (positive: back into the history).
    pub fn scroll(&self, lines: i32) {
        self.with_term(|t| t.scroll_display(Scroll::Delta(lines)));
    }

    pub fn mode(&self) -> TermMode {
        *lock(&self.term).mode()
    }

    /// Copies the visible screen out under the lock.
    pub fn snapshot(&self) -> Snap {
        let t = lock(&self.term);
        let rc = t.renderable_content();
        let (cols, rows) = (t.columns(), t.screen_lines());
        let blank = Cell {
            ch: ' ',
            fg: DEFAULT_FG,
            bg: DEFAULT_BG,
            bold: false,
            italic: false,
            underline: false,
            strike: false,
            wide: false,
            selected: false,
            link: false,
        };
        let mut cells = vec![blank; cols * rows];
        for ic in rc.display_iter {
            let Some(p) = point_to_viewport(rc.display_offset, ic.point) else {
                continue;
            };
            if p.line >= rows || p.column.0 >= cols {
                continue;
            }
            let c = ic.cell;
            if c.flags.contains(Flags::WIDE_CHAR_SPACER) {
                continue;
            }
            let (fg, bg) = cell_colors(c.fg, c.bg, c.flags, rc.colors);
            cells[p.line * cols + p.column.0] = Cell {
                ch: if c.c == '\0' { ' ' } else { c.c },
                fg,
                bg,
                bold: c.flags.contains(Flags::BOLD),
                italic: c.flags.contains(Flags::ITALIC),
                underline: c.flags.intersects(Flags::ALL_UNDERLINES),
                strike: c.flags.contains(Flags::STRIKEOUT),
                wide: c.flags.contains(Flags::WIDE_CHAR),
                selected: rc.selection.is_some_and(|r| r.contains(ic.point)),
                link: c.hyperlink().is_some(),
            };
        }
        let cursor = (rc.mode.contains(TermMode::SHOW_CURSOR)
            && rc.cursor.shape != CursorShape::Hidden)
            .then(|| point_to_viewport(rc.display_offset, rc.cursor.point))
            .flatten()
            .filter(|p| p.line < rows && p.column.0 < cols)
            .map(|p| (p.column.0, p.line, rc.cursor.shape));
        Snap {
            cols,
            rows,
            cells,
            cursor,
            mode: rc.mode,
            offset: rc.display_offset,
            history: t.grid().history_size(),
        }
    }

    /// The URI of the OSC 8 link under a cell (viewport coordinates).
    pub fn link_at(&self, col: usize, row: usize) -> Option<String> {
        self.with_term(|t| {
            let p = viewport_to_point(t.grid().display_offset(), Point::new(row, Column(col)));
            (col < t.columns() && row < t.screen_lines())
                .then(|| t.grid()[p].hyperlink().map(|h| h.uri().to_string()))
                .flatten()
        })
    }

    // ---- selection (viewport cells; `right` is the right half of the cell) ----

    pub fn select_start(&self, ty: SelectionType, col: usize, row: usize, right: bool) {
        self.with_term(|t| {
            let p = viewport_to_point(t.grid().display_offset(), Point::new(row, Column(col)));
            t.selection = Some(Selection::new(
                ty,
                p,
                if right { Side::Right } else { Side::Left },
            ));
        });
    }

    pub fn select_to(&self, col: usize, row: usize, right: bool) {
        self.with_term(|t| {
            let p = viewport_to_point(t.grid().display_offset(), Point::new(row, Column(col)));
            if let Some(s) = &mut t.selection {
                s.update(p, if right { Side::Right } else { Side::Left });
            }
        });
    }

    pub fn select_all(&self) {
        self.with_term(|t| {
            let (top, bottom) = (t.topmost_line(), t.bottommost_line());
            let mut s = Selection::new(
                SelectionType::Simple,
                Point::new(top, Column(0)),
                Side::Left,
            );
            s.update(Point::new(bottom, t.last_column()), Side::Right);
            t.selection = Some(s);
        });
    }

    pub fn select_clear(&self) {
        self.with_term(|t| t.selection = None);
    }

    pub fn selected_text(&self) -> Option<String> {
        self.with_term(|t| t.selection_to_string())
            .filter(|s| !s.is_empty())
    }
}

impl Session {
    /// The foreground process group of the terminal (a job of the interactive shell has its own group, and
    /// `nohup`/background jobs stay in it after a hang-up). Unix only; read while the master is still ours.
    fn fg_group(&self) -> Option<u32> {
        #[cfg(unix)]
        {
            let g = lock(&self.master)
                .as_ref()
                .and_then(|m| m.process_group_leader())?;
            u32::try_from(g).ok().filter(|g| *g > 1)
        }
        #[cfg(not(unix))]
        None
    }

    /// Hang up the shell's group and the foreground group (the first half of closing; does not wait).
    pub fn hangup(&self) {
        if let Some(c) = &self.child {
            hup(c, self.fg_group());
        }
    }

    /// Kills both groups now (the second half of closing; bounded, no waiting for the reap).
    pub fn kill_now(&self) {
        if let Some(c) = &self.child {
            kill(c, self.fg_group());
        }
    }

    /// Whether the shell has not been reaped yet.
    pub fn alive(&self) -> bool {
        self.child
            .as_ref()
            .is_some_and(|c| !matches!(lock(c).try_wait(), Ok(Some(_))))
    }
}

impl Drop for Session {
    /// Hang up, give the shell 2 s, then kill its groups (the shell's pid is never signalled once reaped).
    /// On app quit this thread may not get to run: the pane calls [`Session::hangup`] and
    /// [`Session::kill_now`] itself then.
    fn drop(&mut self) {
        let Some(child) = self.child.clone() else {
            return;
        };
        let fg = self.fg_group();
        self.child = None;
        lock(&self.master).take();
        let _ = std::thread::Builder::new()
            .name("synt-term-reap".into())
            .spawn(move || terminate(&child, fg));
    }
}

type Child_ = Mutex<Box<dyn Child + Send + Sync>>;

#[cfg(unix)]
fn signal_groups(shell: Option<u32>, fg: Option<u32>, sig: rustix::process::Signal) {
    let mut gs = vec![shell, fg];
    gs.dedup();
    for g in gs.into_iter().flatten() {
        if let Some(p) = i32::try_from(g)
            .ok()
            .and_then(rustix::process::Pid::from_raw)
        {
            let _ = rustix::process::kill_process_group(p, sig);
        }
    }
}

/// SIGHUP to the shell's group and the foreground group, unless the shell was reaped (its pid may be
/// recycled). Under the child lock: no reap can slip in between the check and the signal.
fn hup(child: &Child_, fg: Option<u32>) {
    let mut c = lock(child);
    if matches!(c.try_wait(), Ok(Some(_))) {
        return;
    }
    #[cfg(unix)]
    signal_groups(c.process_id(), fg, rustix::process::Signal::HUP);
    let _ = fg;
}

/// The same with SIGKILL (`taskkill /T /F` on Windows), then `Child::kill`.
fn kill(child: &Child_, fg: Option<u32>) {
    let mut c = lock(child);
    if matches!(c.try_wait(), Ok(Some(_))) {
        return;
    }
    #[cfg(unix)]
    signal_groups(c.process_id(), fg, rustix::process::Signal::KILL);
    let _ = fg;
    #[cfg(windows)]
    if let Some(pid) = c.process_id() {
        let _ = std::process::Command::new(synthesis_platform::system32("taskkill.exe"))
            .args(["/T", "/F", "/PID"])
            .arg(pid.to_string())
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
    }
    let _ = c.kill();
}

fn terminate(child: &Child_, fg: Option<u32>) {
    hup(child, fg);
    let end = Instant::now() + Duration::from_secs(2);
    while Instant::now() < end {
        if matches!(lock(child).try_wait(), Ok(Some(_))) {
            return;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    kill(child, fg);
    for _ in 0..3000 {
        if matches!(lock(child).try_wait(), Ok(Some(_))) {
            return;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn screen(s: &Session) -> Vec<String> {
        let snap = s.snapshot();
        snap.cells
            .chunks(snap.cols)
            .map(|r| {
                r.iter()
                    .map(|c| c.ch)
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .collect()
    }

    #[test]
    fn a_script_draws_text_colour_and_the_cursor() {
        let s = Session::scripted(
            20,
            4,
            b"plain \x1b[1;31mred\x1b[0m\r\nsecond \xe2\x82\xac",
            "t",
        );
        let snap = s.snapshot();
        assert_eq!(&screen(&s)[..2], ["plain red", "second \u{20ac}"]);
        let red = &snap.cells[6];
        assert!(red.bold && red.fg == ansi(9), "{red:?}");
        assert_eq!(snap.cells[0].fg, DEFAULT_FG);
        assert_eq!(snap.cursor.map(|c| (c.0, c.1)), Some((8, 1)));
    }

    #[test]
    fn selection_covers_exactly_the_selected_characters() {
        let s = Session::scripted(12, 3, b"abcdef ghij\r\nklm", "t");
        // from the left half of column 2 to the right half of column 4: "cde"
        s.select_start(SelectionType::Simple, 2, 0, false);
        s.select_to(4, 0, true);
        let on: Vec<usize> = s
            .snapshot()
            .cells
            .iter()
            .enumerate()
            .filter(|(_, c)| c.selected)
            .map(|(i, _)| i)
            .collect();
        assert_eq!(on, [2, 3, 4]);
        assert_eq!(s.selected_text().as_deref(), Some("cde"));
        // from the right half of column 2: "de"
        s.select_start(SelectionType::Simple, 2, 0, true);
        s.select_to(4, 0, true);
        assert_eq!(s.selected_text().as_deref(), Some("de"));
        // across lines: the rest of line 0 and the start of line 1
        s.select_start(SelectionType::Simple, 7, 0, false);
        s.select_to(1, 1, true);
        assert_eq!(s.selected_text().as_deref(), Some("ghij\nkl"));
        // a word and a line
        s.select_start(SelectionType::Semantic, 8, 0, false);
        assert_eq!(s.selected_text().as_deref(), Some("ghij"));
        s.select_start(SelectionType::Lines, 1, 1, false);
        assert_eq!(s.selected_text().as_deref(), Some("klm\n"));
        s.select_clear();
        assert!(s.selected_text().is_none());
        assert!(s.snapshot().cells.iter().all(|c| !c.selected));
    }

    #[test]
    fn select_all_takes_the_scrollback_too() {
        let mut script = String::new();
        for i in 0..30 {
            script.push_str(&format!("line {i}\r\n"));
        }
        let s = Session::scripted(20, 5, script.as_bytes(), "t");
        s.select_all();
        let text = s.selected_text().unwrap();
        assert!(
            text.starts_with("line 0\n") && text.contains("line 29"),
            "{text:?}"
        );
    }

    #[test]
    fn scrolling_back_shows_the_history() {
        let mut script = String::new();
        for i in 0..30 {
            script.push_str(&format!("line {i}\r\n"));
        }
        let s = Session::scripted(20, 5, script.as_bytes(), "t");
        assert_eq!(screen(&s)[0], "line 26");
        s.scroll(10);
        assert_eq!(screen(&s)[0], "line 16");
        assert_eq!(s.snapshot().offset, 10);
        assert!(
            s.snapshot().cursor.is_none(),
            "the cursor is below the visible lines"
        );
    }

    #[test]
    fn modes_and_hyperlinks_are_read_from_the_term() {
        let s = Session::scripted(
            20,
            3,
            b"\x1b[?1h\x1b[?2004h\x1b]8;;https://example.com\x07link\x1b]8;;\x07 no",
            "t",
        );
        let m = s.mode();
        assert!(m.contains(TermMode::APP_CURSOR) && m.contains(TermMode::BRACKETED_PASTE));
        assert_eq!(s.link_at(1, 0).as_deref(), Some("https://example.com"));
        assert_eq!(s.link_at(6, 0), None);
        assert!(s.snapshot().cells[0].link && !s.snapshot().cells[6].link);
    }

    /// Records every event the `Term` raises, with the configuration of a personal shell.
    struct Rec(Arc<Mutex<Vec<String>>>);
    impl EventListener for Rec {
        fn send_event(&self, e: Event) {
            lock(&self.0).push(format!("{e:?}"));
        }
    }

    /// Red team: OSC 52 store and load never reach the clipboard or the shell, whatever the listener does.
    #[test]
    fn osc_52_is_ignored_both_ways() {
        let seen = Arc::new(Mutex::new(vec![]));
        let mut term = Term::new(config(), &Dims(20, 3), Rec(seen.clone()));
        let mut p = Parser::default();
        p.advance(
            &mut term,
            b"\x1b]52;c;aGVsbG8=\x07\x1b]52;c;?\x07\x1b]52;p;aGk=\x1b\\ok",
        );
        let seen = lock(&seen).join(",");
        assert!(
            !seen.contains("Clipboard") && !seen.contains("PtyWrite"),
            "{seen}"
        );
        // and the listener of a real session drops them too, and writes nothing back
        let (tx, rx) = mpsc::sync_channel(4);
        let (term, _) = new_term(20, 3, tx);
        lock(&term).selection = None;
        Parser::default().advance(&mut *lock(&term), b"\x1b]52;c;?\x07\x1b]52;c;aGk=\x07");
        assert!(rx.try_recv().is_err());
        let l = Listener {
            reply: synthesis_terminal::Reply::new(mpsc::sync_channel(4).0),
            title: Default::default(),
        };
        l.send_event(Event::ClipboardLoad(
            alacritty_terminal::term::ClipboardType::Clipboard,
            Arc::new(|s| s.to_string()),
        ));
        l.send_event(Event::ClipboardStore(
            alacritty_terminal::term::ClipboardType::Clipboard,
            "x".into(),
        ));
    }

    /// Red team: a title with escapes and bidi controls is sanitised on its way into the tab.
    #[test]
    fn a_title_is_sanitised_and_replies_stay_on_the_allow_list() {
        let (tx, rx) = mpsc::sync_channel(8);
        let (term, title) = new_term(20, 3, tx);
        let mut p = Parser::default();
        p.advance(
            &mut *lock(&term),
            "\x1b]0;evil \u{202e}txt.exe\x07".as_bytes(),
        );
        assert_eq!(*lock(&title), "evil txt.exe");
        p.advance(&mut *lock(&term), b"\x1b]2;\x1b[31m\x9b\x07");
        assert!(!lock(&title).contains(['\x1b', '\u{9b}']));
        // a cursor position report is answered; a text-area size request and a status report are not
        p.advance(&mut *lock(&term), b"\x1b[6n\x1b[18t\x1b[5n");
        let got: Vec<Vec<u8>> = rx.try_iter().collect();
        assert_eq!(got, [b"\x1b[1;1R".to_vec()]);
    }

    #[test]
    fn size_is_clamped_to_the_engine_limits() {
        assert_eq!(clamp(0, 0), (2, 1));
        assert_eq!(clamp(9999, 9999), (MAX_COLS, MAX_ROWS));
        let s = Session::scripted(80, 24, b"", "t");
        s.resize(40, 10);
        assert_eq!((s.snapshot().cols, s.snapshot().rows), (40, 10));
    }

    #[cfg(unix)]
    #[test]
    fn a_real_shell_echoes_resizes_and_ends() {
        let (tx, rx) = mpsc::channel();
        let wake: Arc<dyn Fn() + Send + Sync> = Arc::new(move || {
            let _ = tx.send(());
        });
        let env = synthesis_config::Env::from_pairs([
            ("PATH", "/usr/bin:/bin"),
            ("HOME", "/tmp"),
            ("SYNT_HOME", "/nope"),
            ("API_TOKEN", "x"),
        ]);
        let mut l = super::super::launch::plan(
            &super::super::launch::Where::Here(std::env::temp_dir()),
            &env,
            false,
            None,
            None,
        )
        .unwrap();
        l.program = "/bin/sh".into();
        l.args = vec!["-i".into()];
        let s = Session::spawn(&l, 60, 8, wake).unwrap();
        s.send(b"echo \"[$SYNT_HOME|$API_TOKEN|$TERM]\"; exit 3\r")
            .unwrap();
        let end = Instant::now() + Duration::from_secs(40);
        while s.exited().is_none() && Instant::now() < end {
            let _ = rx.recv_timeout(Duration::from_millis(100));
            s.dirty.store(false, Ordering::Release);
        }
        let all = screen(&s).join("\n");
        assert!(all.contains("[||xterm-256color]"), "{all}");
        assert_eq!(
            s.exited().as_deref(),
            Some("The shell has exited with code 3."),
            "{all}"
        );
        s.resize(30, 5);
    }

    /// A job of the interactive shell has its own process group: closing the session still ends it.
    #[cfg(unix)]
    #[test]
    fn closing_kills_the_foreground_job_group_too() {
        let tmp = tempfile::tempdir().unwrap();
        let env = synthesis_config::Env::from_pairs([("PATH", "/usr/bin:/bin"), ("HOME", "/tmp")]);
        let mut l = super::super::launch::plan(
            &super::super::launch::Where::Here(tmp.path().into()),
            &env,
            false,
            None,
            None,
        )
        .unwrap();
        l.program = "/bin/sh".into();
        l.args = vec!["-i".into()];
        let wake: Arc<dyn Fn() + Send + Sync> = Arc::new(|| {});
        let s = Session::spawn(&l, 60, 8, wake).unwrap();
        let pidfile = tmp.path().join("pid");
        // a foreground job (its own group) that ignores SIGHUP and records its pid
        s.send(
            format!(
                "sh -c 'trap \"\" HUP; echo $$ > {}; sleep 300'\r",
                pidfile.display()
            )
            .as_bytes(),
        )
        .unwrap();
        let end = Instant::now() + Duration::from_secs(30);
        while std::fs::read_to_string(&pidfile).map_or(true, |t| t.trim().is_empty())
            && Instant::now() < end
        {
            std::thread::sleep(Duration::from_millis(50));
        }
        let pid: i32 = std::fs::read_to_string(&pidfile)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        assert!(s.busy());
        s.hangup();
        s.kill_now();
        let p = rustix::process::Pid::from_raw(pid).unwrap();
        let end = Instant::now() + Duration::from_secs(20);
        // the job is gone (reaped by init or us): signal 0 fails with ESRCH
        while rustix::process::test_kill_process(p).is_ok() && Instant::now() < end {
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(
            rustix::process::test_kill_process(p).is_err(),
            "the job survived the close"
        );
    }
}
