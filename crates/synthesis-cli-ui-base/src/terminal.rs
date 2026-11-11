//! Raw mode and restore-on-exit guard.
//!
//! Decision: crossterm is used only for raw mode and terminal size. Its `Event` type is already
//! decoded (and on Windows read from the console API), so it cannot reproduce the byte-level
//! behaviour the golden cases pin (CSI-u, split chunks, Option combos, paste detection). The
//! caller reads stdin bytes itself and feeds [`crate::keys::KeyParser`].
//! Unverified: on Windows raw stdin bytes need `ENABLE_VIRTUAL_TERMINAL_INPUT`, which crossterm
//! does not set; ConPTY and legacy conhost have not been run.
//!
//! [`TerminalGuard`] fixes Node defect D6 (no restore on SIGHUP, uncaught errors skipped it):
//! the terminal is restored on drop, on a panic in the creating thread (before the panic message
//! prints, so it stays visible) and, on Unix, on SIGHUP/SIGTERM, after which the signal's default
//! action runs so the exit status is still "killed by signal".

use crate::screen::terminal_restore_sequence;
use std::io::{self, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

pub use crossterm::terminal::size;

struct Inner {
    out: Mutex<Box<dyn Write + Send>>,
    raw: bool,
    done: AtomicBool,
}

impl Inner {
    fn restore(&self) {
        // Lock before claiming: a second caller (the signal thread vs the normal exit) must wait
        // for the first one's bytes to be written, or the process can exit with them still unsent.
        let mut out = self.out.lock().unwrap_or_else(|p| p.into_inner());
        if self.done.swap(true, Ordering::SeqCst) {
            return;
        }
        let _ = out.write_all(terminal_restore_sequence().as_bytes());
        let _ = out.flush();
        if self.raw {
            let _ = crossterm::terminal::disable_raw_mode();
        }
    }
}

pub struct GuardOptions {
    /// Switch the terminal to raw mode (needs a TTY; off for pipes and tests).
    pub raw_mode: bool,
}

pub struct TerminalGuard {
    inner: Arc<Inner>,
    #[cfg(unix)]
    signals: signal_hook::iterator::backend::Handle,
}

impl TerminalGuard {
    /// Enables raw mode (if asked) and arms the restore paths. `out` receives the restore
    /// sequence; pass the same terminal the [`crate::screen::Screen`] draws on.
    pub fn enter(out: impl Write + Send + 'static, opts: GuardOptions) -> io::Result<Self> {
        let inner = Arc::new(Inner {
            out: Mutex::new(Box::new(out)),
            raw: opts.raw_mode,
            done: AtomicBool::new(false),
        });

        let (weak, owner, prev) = (
            Arc::downgrade(&inner),
            std::thread::current().id(),
            std::panic::take_hook(),
        );
        std::panic::set_hook(Box::new(move |info| {
            if let Some(i) = weak
                .upgrade()
                .filter(|_| std::thread::current().id() == owner || cfg!(panic = "abort"))
            {
                i.restore();
            }
            prev(info);
        }));

        // Handlers first, raw mode second: a SIGHUP/SIGTERM that lands between the two must find
        // a handler (restore is a no-op on a terminal that is not raw yet), never the default
        // action on a terminal already switched to raw mode.
        #[cfg(unix)]
        let signals = {
            use signal_hook::consts::{SIGHUP, SIGTERM};
            let mut sigs = signal_hook::iterator::Signals::new([SIGHUP, SIGTERM])?;
            let handle = sigs.handle();
            let weak = Arc::downgrade(&inner);
            std::thread::spawn(move || {
                if let Some(sig) = sigs.forever().next() {
                    if let Some(i) = weak.upgrade() {
                        i.restore();
                    }
                    let _ = signal_hook::low_level::emulate_default_handler(sig);
                }
            });
            handle
        };

        if opts.raw_mode
            && let Err(e) = crossterm::terminal::enable_raw_mode()
        {
            #[cfg(unix)]
            signals.close();
            return Err(e);
        }

        Ok(Self {
            inner,
            #[cfg(unix)]
            signals,
        })
    }

    /// Restore now; later calls and the drop do nothing more.
    pub fn restore(&self) {
        self.inner.restore();
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        self.inner.restore();
        #[cfg(unix)]
        self.signals.close();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    #[derive(Clone, Default)]
    struct Shared(Arc<Mutex<Vec<u8>>>);
    impl Write for Shared {
        fn write(&mut self, b: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(b);
            Ok(b.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    fn text(s: &Shared) -> String {
        String::from_utf8(s.0.lock().unwrap().clone()).unwrap()
    }

    #[test]
    fn restores_once_on_drop() {
        let out = Shared::default();
        let g = TerminalGuard::enter(out.clone(), GuardOptions { raw_mode: false }).unwrap();
        assert_eq!(text(&out), "");
        g.restore();
        drop(g);
        assert_eq!(text(&out), terminal_restore_sequence());
    }

    /// Blocks inside `write` until released, to hold a restore half done.
    struct Slow(mpsc::Sender<()>, Arc<Mutex<Vec<u8>>>);
    impl Write for Slow {
        fn write(&mut self, b: &[u8]) -> io::Result<usize> {
            self.0.send(()).unwrap();
            std::thread::sleep(std::time::Duration::from_millis(200));
            self.1.lock().unwrap().extend_from_slice(b);
            Ok(b.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn a_second_restore_waits_for_the_first_to_finish_writing() {
        let (tx, rx) = mpsc::channel();
        let buf = Arc::default();
        let g = TerminalGuard::enter(Slow(tx, Arc::clone(&buf)), GuardOptions { raw_mode: false })
            .unwrap();
        let inner = Arc::clone(&g.inner);
        let t = std::thread::spawn(move || inner.restore());
        rx.recv().unwrap(); // the first restore is inside its write
        g.restore();
        assert!(
            !buf.lock().unwrap().is_empty(),
            "returned before the bytes were written"
        );
        t.join().unwrap();
    }

    #[test]
    fn restores_when_unwinding() {
        let out = Shared::default();
        let o = out.clone();
        let r = std::panic::catch_unwind(move || {
            let _g = TerminalGuard::enter(o, GuardOptions { raw_mode: false }).unwrap();
            panic!("boom");
        });
        assert!(r.is_err());
        assert_eq!(text(&out), terminal_restore_sequence());
    }
}
