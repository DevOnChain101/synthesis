//! A real shell on a PTY for the Terminal tab and the plain-terminal mode (docs/specs/desktop.md
//! section 2.2). `alacritty_terminal::tty` opens the PTY and starts the shell in its own session;
//! dropping the handle sends the shell SIGHUP, which it forwards to its jobs.
//! Unix only for now: the ConPTY reader/writer of Windows are not cloneable, a Windows pass is TODO.

use std::io;
use std::path::Path;

use futures::channel::mpsc::UnboundedReceiver;

#[cfg(unix)]
mod imp {
    use super::*;
    use alacritty_terminal::event::{OnResize, WindowSize};
    use alacritty_terminal::tty::{self, Options, Pty};
    use futures::channel::mpsc::unbounded;
    use std::fs::File;
    use std::io::{Read, Write};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Duration;

    pub struct PtyShell {
        pty: Pty,
        writer: File,
        stop: Arc<AtomicBool>,
    }

    impl PtyShell {
        /// `shell` is started directly (not through `login`, which ignores the hangup on close).
        pub fn spawn(
            shell: &str,
            cwd: &Path,
            cols: u16,
            rows: u16,
        ) -> io::Result<(Self, UnboundedReceiver<Vec<u8>>)> {
            let opts = Options {
                shell: Some(tty::Shell::new(shell.to_string(), vec![])),
                working_directory: Some(cwd.to_path_buf()),
                env: [("TERM", "xterm-256color"), ("COLORTERM", "truecolor")]
                    .into_iter()
                    .map(|(k, v)| (k.to_string(), v.to_string()))
                    .collect(),
                ..Options::default()
            };
            let size = WindowSize {
                num_lines: rows,
                num_cols: cols,
                cell_width: 8,
                cell_height: 16,
            };
            let pty = tty::new(&opts, size, 0)?;
            let writer = pty.file().try_clone()?;
            let mut reader = pty.file().try_clone()?;
            let (tx, rx) = unbounded();
            let stop = Arc::new(AtomicBool::new(false));
            let stop2 = stop.clone();
            std::thread::spawn(move || {
                let mut buf = [0u8; 16 * 1024];
                while !stop2.load(Ordering::Relaxed) {
                    match reader.read(&mut buf) {
                        Ok(0) => break,
                        Ok(n) => {
                            if tx.unbounded_send(buf[..n].to_vec()).is_err() {
                                break;
                            }
                        }
                        // the master is non-blocking: poll
                        Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                            std::thread::sleep(Duration::from_millis(4));
                        }
                        Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                        Err(_) => break, // EIO: the shell exited
                    }
                }
            });
            Ok((Self { pty, writer, stop }, rx))
        }

        pub fn write(&mut self, bytes: &[u8]) {
            let mut left = bytes;
            while !left.is_empty() {
                match self.writer.write(left) {
                    Ok(n) => left = &left[n..],
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(2));
                    }
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                    Err(_) => return,
                }
            }
        }

        pub fn resize(&mut self, cols: u16, rows: u16) {
            self.pty.on_resize(WindowSize {
                num_lines: rows,
                num_cols: cols,
                cell_width: 8,
                cell_height: 16,
            });
        }
    }

    impl Drop for PtyShell {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Relaxed);
            // `Pty::drop` hangs the shell up and then waits for it; a shell that ignores the hangup
            // would freeze the window. Kill the shell's whole process group first (it leads its
            // own session), so the wait returns at once.
            let pid = self.pty.child().id();
            let _ = std::process::Command::new("kill")
                .args(["-KILL", "--", &format!("-{pid}")])
                .stderr(std::process::Stdio::null())
                .status();
        }
    }
}

#[cfg(not(unix))]
mod imp {
    use super::*;

    pub struct PtyShell;

    impl PtyShell {
        pub fn spawn(
            _: &str,
            _: &Path,
            _: u16,
            _: u16,
        ) -> io::Result<(Self, UnboundedReceiver<Vec<u8>>)> {
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "the shell panel needs a Unix PTY; ConPTY support is not built yet",
            ))
        }
        pub fn write(&mut self, _: &[u8]) {}
        pub fn resize(&mut self, _: u16, _: u16) {}
    }
}

pub use imp::PtyShell;

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn a_shell_echoes_and_follows_a_resize() {
        let dir = std::env::temp_dir();
        let (mut sh, mut rx) = PtyShell::spawn("/bin/sh", &dir, 80, 24).unwrap();
        sh.write(b"echo synth-$((20+22))\n");
        sh.resize(100, 30);
        let mut text = String::new();
        let t0 = std::time::Instant::now();
        while !text.contains("synth-42\r\n") && t0.elapsed().as_secs() < 10 {
            if let Ok(b) = rx.try_recv() {
                text.push_str(&String::from_utf8_lossy(&b));
            } else {
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
        }
        assert!(text.contains("synth-42\r\n"), "{text:?}");
    }
}
