//! The terminal behind two traits (`Read` for raw input bytes, `Write` for output) plus a size
//! function, so the whole app runs against a scripted TTY in tests. [`Tty::stdio`] is the real
//! one; [`ScriptedTty`] is the test double.

use std::io::{self, Read, Write};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};

use synthesis_config::Env;

pub struct Tty {
    pub input: Box<dyn Read + Send>,
    pub output: Box<dyn Write + Send>,
    pub size: Box<dyn Fn() -> (u32, u32) + Send>,
    /// A real terminal: raw mode on (needs a TTY). Off for pipes and scripted TTYs.
    pub raw_mode: bool,
    /// Watch SIGINT (an external interrupt) and SIGWINCH (resize wake-up).
    pub signals: bool,
}

/// `COLUMNS` / `LINES` (when set and plausible), else 80x24.
fn env_size(env: &Env) -> (u32, u32) {
    let get = |k: &str, d: u32| {
        env.get_nonempty(k)
            .and_then(|v| v.parse::<u32>().ok())
            .filter(|v| *v > 0)
            .unwrap_or(d)
    };
    (get("COLUMNS", 80), get("LINES", 24))
}

impl Tty {
    /// Process stdin/stdout. The size is the terminal's own; `COLUMNS`/`LINES` from `env` only
    /// when the terminal cannot report one.
    pub fn stdio(env: &Env) -> Self {
        let env = env.clone();
        Self {
            input: Box::new(io::stdin()),
            output: Box::new(io::stdout()),
            size: Box::new(move || {
                synthesis_tui_base::terminal::size()
                    .map(|(c, r)| (u32::from(c), u32::from(r)))
                    .unwrap_or_else(|_| env_size(&env))
            }),
            raw_mode: true,
            signals: true,
        }
    }
}

/// Cloneable writer so the screen and the restore guard draw on the same terminal.
#[derive(Clone)]
pub(crate) struct SharedOut(Arc<Mutex<Box<dyn Write + Send>>>);

impl SharedOut {
    pub fn new(w: Box<dyn Write + Send>) -> Self {
        Self(Arc::new(Mutex::new(w)))
    }
}

impl Write for SharedOut {
    fn write(&mut self, b: &[u8]) -> io::Result<usize> {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).write(b)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).flush()
    }
}

// ---------------------------------------------------------------- scripted TTY (tests)

struct ChanReader(Receiver<Vec<u8>>, Vec<u8>);

impl Read for ChanReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.1.is_empty() {
            match self.0.recv() {
                Ok(v) => self.1 = v,
                Err(_) => return Ok(0), // handle dropped / closed: end of input
            }
        }
        let n = buf.len().min(self.1.len());
        buf[..n].copy_from_slice(&self.1[..n]);
        self.1.drain(..n);
        Ok(n)
    }
}

#[derive(Clone, Default)]
struct Sink(Arc<Mutex<Vec<u8>>>);

impl Write for Sink {
    fn write(&mut self, b: &[u8]) -> io::Result<usize> {
        self.0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .extend_from_slice(b);
        Ok(b.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// The test side of a scripted terminal: type bytes, resize, read what was drawn.
pub struct ScriptedTty {
    input: Mutex<Option<Sender<Vec<u8>>>>,
    out: Sink,
    size: Arc<Mutex<(u32, u32)>>,
}

impl ScriptedTty {
    pub fn new(cols: u32, rows: u32) -> (Self, Tty) {
        let (tx, rx) = mpsc::channel();
        let out = Sink::default();
        let size = Arc::new(Mutex::new((cols, rows)));
        let s2 = size.clone();
        let tty = Tty {
            input: Box::new(ChanReader(rx, Vec::new())),
            output: Box::new(out.clone()),
            size: Box::new(move || *s2.lock().unwrap_or_else(|e| e.into_inner())),
            raw_mode: false,
            signals: false,
        };
        (
            Self {
                input: Mutex::new(Some(tx)),
                out,
                size,
            },
            tty,
        )
    }

    /// Bytes as a terminal would deliver them (`"\r"` is enter, `"\x1b"` escape, ...).
    pub fn send(&self, bytes: &str) {
        if let Some(tx) = self
            .input
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
        {
            let _ = tx.send(bytes.as_bytes().to_vec());
        }
    }

    pub fn resize(&self, cols: u32, rows: u32) {
        *self.size.lock().unwrap_or_else(|e| e.into_inner()) = (cols, rows);
    }

    /// End of input (the terminal went away).
    pub fn close_input(&self) {
        self.input.lock().unwrap_or_else(|e| e.into_inner()).take();
    }

    /// Everything written so far, raw.
    pub fn raw(&self) -> String {
        String::from_utf8_lossy(&self.out.0.lock().unwrap_or_else(|e| e.into_inner())).into_owned()
    }

    /// The screen as the output left it: replays cursor-addressed line writes (the only thing
    /// `Screen` emits), colours stripped.
    pub fn screen(&self) -> Vec<String> {
        replay(&self.raw())
    }

    pub fn screen_text(&self) -> String {
        self.screen().join("\n")
    }
}

/// Minimal terminal model for `Screen`'s protocol: `ESC[2J` clears, `ESC[r;1H` addresses a row,
/// text fills it, `ESC[K` ends it. Everything else (SGR, modes, OSC) is skipped.
pub fn replay(raw: &str) -> Vec<String> {
    let mut rows: Vec<String> = Vec::new();
    let mut row = 0usize;
    // OSC 8 hyperlinks carry no cells: text snapshots do not see them
    let cs: Vec<char> = synthesis_tui_base::text::strip_osc8(raw).chars().collect();
    let mut i = 0;
    while i < cs.len() {
        if cs[i] == '\x1b' && cs.get(i + 1) == Some(&'[') {
            let mut j = i + 2;
            while j < cs.len() && !(('@'..='~').contains(&cs[j])) {
                j += 1;
            }
            let params: String = cs[i + 2..j.min(cs.len())].iter().collect();
            match cs.get(j) {
                Some('J') if params.starts_with('2') => rows.clear(),
                // `ESC[r;cH` with c > 1 only parks the cursor (the prompt caret): no write follows
                Some('H') if params.split(';').nth(1).is_some_and(|c| c != "1") => {}
                Some('H') => {
                    row = params
                        .split(';')
                        .next()
                        .and_then(|r| r.parse::<usize>().ok())
                        .unwrap_or(1)
                        .saturating_sub(1);
                    if rows.len() <= row {
                        rows.resize(row + 1, String::new());
                    }
                    rows[row].clear();
                }
                _ => {}
            }
            i = j + 1;
        } else if cs[i] == '\x1b' && cs.get(i + 1) == Some(&']') {
            while i < cs.len() && cs[i] != '\x07' {
                i += 1;
            }
            i += 1;
        } else {
            if rows.len() > row && cs[i] != '\n' && cs[i] != '\r' {
                rows[row].push(cs[i]);
            }
            i += 1;
        }
    }
    rows.iter().map(|r| r.trim_end().to_string()).collect()
}
