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

/// Every guard that is alive, so code that must end the process with `process::exit` (which runs
/// no destructors: the `synt stop` hard kill, a second Ctrl-C) can still leave a usable terminal.
static ACTIVE: Mutex<Vec<std::sync::Weak<Inner>>> = Mutex::new(Vec::new());

/// How long the exit paths wait for the restore (a writer stuck mid-write or a stopped output
/// must not keep a process that is being killed alive).
pub const EXIT_RESTORE_WAIT: std::time::Duration = std::time::Duration::from_millis(500);

/// [`restore_active`] for code that is about to `process::exit`: runs it on a helper thread and
/// waits at most `wait`; if it is stuck, switches raw mode off directly (no lock, no write) and
/// returns anyway. Returns false when the helper did not finish.
pub fn restore_active_bounded(wait: std::time::Duration) -> bool {
    run_bounded(restore_active, wait)
}

fn run_bounded(f: impl FnOnce() + Send + 'static, wait: std::time::Duration) -> bool {
    let (tx, rx) = std::sync::mpsc::channel();
    let spawned = std::thread::Builder::new().spawn(move || {
        f();
        let _ = tx.send(());
    });
    if spawned.is_ok() && rx.recv_timeout(wait).is_ok() {
        return true;
    }
    let _ = crossterm::terminal::disable_raw_mode();
    false
}

/// Restores the terminal of every live [`TerminalGuard`] (cooked mode, main screen, cursor);
/// guards restore only once, so calling it twice or before a drop is harmless.
pub fn restore_active() {
    let all: Vec<_> = {
        let mut a = ACTIVE.lock().unwrap_or_else(|p| p.into_inner());
        a.retain(|w| w.strong_count() > 0);
        a.iter().filter_map(std::sync::Weak::upgrade).collect()
    };
    for i in all {
        i.restore();
    }
}

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

        ACTIVE
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .push(Arc::downgrade(&inner));
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

    /// `restore_active` is global: tests that make guards must not overlap.
    static SERIAL: Mutex<()> = Mutex::new(());
    fn serial() -> std::sync::MutexGuard<'static, ()> {
        SERIAL.lock().unwrap_or_else(|p| p.into_inner())
    }

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
        let _s = serial();
        let out = Shared::default();
        let g = TerminalGuard::enter(out.clone(), GuardOptions { raw_mode: false }).unwrap();
        assert_eq!(text(&out), "");
        g.restore();
        drop(g);
        assert_eq!(text(&out), terminal_restore_sequence());
    }

    #[test]
    fn restore_active_restores_a_live_guard_once() {
        let _s = serial();
        let out = Shared::default();
        let g = TerminalGuard::enter(out.clone(), GuardOptions { raw_mode: false }).unwrap();
        restore_active(); // what the process-exit paths call: no destructor runs there
        assert_eq!(text(&out), terminal_restore_sequence());
        drop(g);
        restore_active();
        assert_eq!(text(&out), terminal_restore_sequence(), "written once");
    }

    #[test]
    fn the_exit_restore_is_bounded_when_the_writer_is_stuck() {
        let _s = serial();
        let (tx, rx) = mpsc::channel();
        let buf = Arc::default();
        // a writer that never returns in time, as a terminal stopped with ^S
        let g =
            TerminalGuard::enter(StuckWrite(tx, buf), GuardOptions { raw_mode: false }).unwrap();
        let t0 = std::time::Instant::now();
        let inner = Arc::clone(&g.inner);
        let finished = run_bounded(
            move || inner.restore(),
            std::time::Duration::from_millis(100),
        );
        assert!(!finished && t0.elapsed() < std::time::Duration::from_secs(2));
        rx.recv().unwrap(); // the writer really was entered
    }

    struct StuckWrite(mpsc::Sender<()>, Arc<Mutex<Vec<u8>>>);
    impl Write for StuckWrite {
        fn write(&mut self, b: &[u8]) -> io::Result<usize> {
            self.0.send(()).unwrap();
            std::thread::sleep(std::time::Duration::from_secs(3));
            self.1.lock().unwrap().extend_from_slice(b);
            Ok(b.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
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
        let _s = serial();
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
        let _s = serial();
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
