//! Clipboard copy (the `copyToClipboard` / `clipboard.js` part of the Node port).
//!
//! A [`ClipboardSink`] is one way to get text onto the clipboard; [`copy_to_clipboard`] tries
//! sinks in order. [`default_chain`] picks the OS tool first (clip.exe, pbcopy, wl-copy, xclip)
//! and OSC 52 last, which asks the terminal to do it and also works over ssh.
//!
//! Differences from Node: Node only uses clip.exe (Windows) or OSC 52; macOS/Linux tools are
//! new. Node's `copyToClipboard` always returns `true`; here the result says whether any sink
//! accepted the text (OSC 52 on a non-TTY writes nothing and reports failure).

use crate::Platform;
use std::io::{self, IsTerminal, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::time::Duration;
use synthesis_config::Env;

pub trait ClipboardSink {
    fn copy(&self, text: &str) -> io::Result<()>;
}

/// Best effort, nothing here can fail the app. Returns whether a sink accepted the text.
pub fn copy_to_clipboard(text: &str, sinks: &[&dyn ClipboardSink]) -> bool {
    sinks.iter().any(|s| s.copy(text).is_ok())
}

pub fn base64(bytes: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for c in bytes.chunks(3) {
        let n = (u32::from(c[0]) << 16)
            | (u32::from(*c.get(1).unwrap_or(&0)) << 8)
            | u32::from(*c.get(2).unwrap_or(&0));
        for k in 0..4 {
            if k <= c.len() {
                out.push(T[(n >> (18 - 6 * k) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// OSC 52 "set clipboard": `ESC ] 52 ; c ; <base64> BEL`.
pub fn osc52(text: &str) -> String {
    format!("\x1b]52;c;{}\x07", base64(text.as_bytes()))
}

/// Writes OSC 52 to a terminal; refuses when `is_tty` is false (output is piped).
pub struct Osc52<W: Write> {
    out: Mutex<W>,
    is_tty: bool,
}

impl<W: Write> Osc52<W> {
    pub fn new(out: W, is_tty: bool) -> Self {
        Self {
            out: Mutex::new(out),
            is_tty,
        }
    }
}

impl Osc52<io::Stdout> {
    pub fn stdout() -> Self {
        Self::new(io::stdout(), io::stdout().is_terminal())
    }
}

impl<W: Write> ClipboardSink for Osc52<W> {
    fn copy(&self, text: &str) -> io::Result<()> {
        if !self.is_tty {
            return Err(io::Error::new(
                io::ErrorKind::NotConnected,
                "output is not a terminal",
            ));
        }
        let mut out = self.out.lock().unwrap_or_else(|p| p.into_inner());
        out.write_all(osc52(text).as_bytes())?;
        out.flush()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Encoding {
    Utf8,
    /// What clip.exe reads: UTF-16LE with a BOM (UTF-8 garbles non-ASCII text).
    Utf16LeBom,
}

/// A copy tool still running after this is killed (it may hold the selection for xclip, but
/// the data was already handed over).
const COPY_TIMEOUT: Duration = Duration::from_secs(5);

/// An OS clipboard tool fed on stdin.
#[derive(Clone, Debug)]
pub struct PipeClipboard {
    pub program: PathBuf,
    pub args: Vec<String>,
    pub encoding: Encoding,
}

impl PipeClipboard {
    /// `%SystemRoot%\System32\clip.exe` by absolute path (no PATH lookup).
    pub fn clip_exe() -> Self {
        Self {
            program: synthesis_platform::system32("clip.exe"),
            args: vec![],
            encoding: Encoding::Utf16LeBom,
        }
    }

    pub fn pbcopy() -> Self {
        Self {
            program: "/usr/bin/pbcopy".into(),
            args: vec![],
            encoding: Encoding::Utf8,
        }
    }

    pub fn wl_copy() -> Self {
        Self {
            program: "wl-copy".into(),
            args: vec![],
            encoding: Encoding::Utf8,
        }
    }

    pub fn xclip() -> Self {
        Self {
            program: "xclip".into(),
            args: vec!["-selection".into(), "clipboard".into()],
            encoding: Encoding::Utf8,
        }
    }

    pub fn payload(&self, text: &str) -> Vec<u8> {
        match self.encoding {
            Encoding::Utf8 => text.as_bytes().to_vec(),
            Encoding::Utf16LeBom => {
                let mut v = vec![0xff, 0xfe];
                v.extend(text.encode_utf16().flat_map(u16::to_le_bytes));
                v
            }
        }
    }
}

impl ClipboardSink for PipeClipboard {
    /// Spawns the tool, hands it the text and returns without waiting for it (a thread reaps it).
    fn copy(&self, text: &str) -> io::Result<()> {
        let mut cmd = Command::new(&self.program);
        cmd.args(&self.args)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        #[cfg(windows)]
        std::os::windows::process::CommandExt::creation_flags(&mut cmd, 0x0800_0000); // CREATE_NO_WINDOW
        let mut child = cmd.spawn()?;
        let mut stdin = child.stdin.take().expect("stdin was piped");
        let data = self.payload(text);
        std::thread::spawn(move || {
            let _ = stdin.write_all(&data);
            drop(stdin); // EOF: the tool only finishes (and pbcopy only commits) after this
            let deadline = std::time::Instant::now() + COPY_TIMEOUT;
            while matches!(child.try_wait(), Ok(None)) {
                if std::time::Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
        });
        Ok(())
    }
}

/// OS tool(s) for `platform`, then OSC 52 on stdout. Linux uses wl-copy under Wayland
/// (`WAYLAND_DISPLAY`) and xclip under X11 (`DISPLAY`), both read from the injected `env`.
pub fn default_chain(platform: Platform, env: &Env) -> Vec<Box<dyn ClipboardSink>> {
    let mut chain: Vec<Box<dyn ClipboardSink>> = Vec::new();
    match platform {
        Platform::Windows => chain.push(Box::new(PipeClipboard::clip_exe())),
        Platform::MacOs => chain.push(Box::new(PipeClipboard::pbcopy())),
        Platform::Linux => {
            if env.get_nonempty("WAYLAND_DISPLAY").is_some() {
                chain.push(Box::new(PipeClipboard::wl_copy()));
            }
            if env.get_nonempty("DISPLAY").is_some() {
                chain.push(Box::new(PipeClipboard::xclip()));
            }
        }
        Platform::Other => {}
    }
    chain.push(Box::new(Osc52::stdout()));
    chain
}

/// One way to read the system clipboard. The text is DATA: callers sanitise it
/// ([`crate::keys::clean_paste`]) before it goes anywhere.
pub trait ClipboardSource {
    fn paste(&self) -> io::Result<String>;
}

/// Most bytes read from a paste tool; the rest is dropped.
pub const PASTE_MAX_BYTES: usize = 1 << 20;
pub const PASTE_TIMEOUT: Duration = Duration::from_secs(2);

/// An OS tool that prints the clipboard on stdout. Absolute path, argument vector, no shell.
#[derive(Clone, Debug)]
pub struct PasteCommand {
    pub program: PathBuf,
    pub args: Vec<String>,
}

impl PasteCommand {
    fn new(program: PathBuf, args: &[&str]) -> Self {
        Self {
            program,
            args: args.iter().map(|a| (*a).to_string()).collect(),
        }
    }
}

impl ClipboardSource for PasteCommand {
    /// Reads at most [`PASTE_MAX_BYTES`] within [`PASTE_TIMEOUT`]; the tool is killed on timeout
    /// or when the cap is hit, and a failing exit status is an error.
    fn paste(&self) -> io::Result<String> {
        let mut cmd = Command::new(&self.program);
        cmd.args(&self.args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        #[cfg(windows)]
        std::os::windows::process::CommandExt::creation_flags(&mut cmd, 0x0800_0000); // CREATE_NO_WINDOW
        let name = self.program.display().to_string();
        let mut child = cmd
            .spawn()
            .map_err(|e| io::Error::new(e.kind(), format!("{name}: {e}")))?;
        let out = child.stdout.take().expect("stdout was piped");
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            let r = out.take(PASTE_MAX_BYTES as u64 + 1).read_to_end(&mut buf);
            let _ = tx.send(r.map(|_| buf));
        });
        let got = rx.recv_timeout(PASTE_TIMEOUT);
        let capped = matches!(&got, Ok(Ok(b)) if b.len() > PASTE_MAX_BYTES);
        let status = if got.is_err() || capped {
            let _ = child.kill();
            None
        } else {
            child.wait().ok()
        };
        let _ = child.wait(); // reap
        match got {
            Err(_) => Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!("{name} did not answer in {}s", PASTE_TIMEOUT.as_secs()),
            )),
            Ok(Err(e)) => Err(e),
            Ok(Ok(mut b)) => {
                if !capped && status.is_some_and(|s| !s.success()) {
                    return Err(io::Error::other(format!("{name} failed (no text copied?)")));
                }
                b.truncate(PASTE_MAX_BYTES);
                Ok(String::from_utf8_lossy(&b).into_owned())
            }
        }
    }
}

/// `name` in a standard system directory (never PATH or the cwd).
fn find_tool(name: &str) -> Option<PathBuf> {
    [
        "/usr/bin",
        "/usr/local/bin",
        "/bin",
        "/opt/homebrew/bin",
        "/snap/bin",
        "/run/current-system/sw/bin",
    ]
    .iter()
    .map(|d| Path::new(d).join(name))
    .find(|p| p.is_file())
}

/// Paste tools for `platform`, in the order to try them. Linux: wl-paste under Wayland
/// (`WAYLAND_DISPLAY`), then xclip / xsel under X11 (`DISPLAY`), both read from the injected
/// `env`; only tools found in standard directories are listed.
pub fn paste_chain(platform: Platform, env: &Env) -> Vec<Box<dyn ClipboardSource>> {
    let mut chain: Vec<Box<dyn ClipboardSource>> = Vec::new();
    match platform {
        Platform::MacOs => chain.push(Box::new(PasteCommand::new("/usr/bin/pbpaste".into(), &[]))),
        Platform::Windows => {
            // UTF-8 stdout; -Raw keeps the newlines as one string
            let script = "[Console]::OutputEncoding=[Text.Encoding]::UTF8; Get-Clipboard -Raw";
            let args = ["-NoProfile", "-NonInteractive", "-Command", script];
            chain.push(Box::new(PasteCommand::new(
                synthesis_platform::system32(r"WindowsPowerShell\v1.0\powershell.exe"),
                &args,
            )));
            if let Some(pf) = env.get_nonempty("ProgramFiles") {
                let pwsh = Path::new(pf).join(r"PowerShell\7\pwsh.exe");
                if pwsh.is_file() {
                    chain.push(Box::new(PasteCommand::new(pwsh, &args)));
                }
            }
        }
        Platform::Linux => {
            let mut add = |tool: &str, args: &[&str]| {
                if let Some(p) = find_tool(tool) {
                    chain.push(Box::new(PasteCommand::new(p, args)));
                }
            };
            if env.get_nonempty("WAYLAND_DISPLAY").is_some() {
                add("wl-paste", &["-n"]);
            }
            if env.get_nonempty("DISPLAY").is_some() {
                add("xclip", &["-selection", "clipboard", "-o"]);
                add("xsel", &["-b", "-o"]);
            }
        }
        Platform::Other => {}
    }
    chain
}

/// First source that answers wins; otherwise the last error (or "no clipboard tool").
pub fn read_clipboard(sources: &[Box<dyn ClipboardSource>]) -> io::Result<String> {
    let mut last = io::Error::new(io::ErrorKind::NotFound, "no clipboard tool found");
    for s in sources {
        match s.paste() {
            Ok(t) => return Ok(t),
            Err(e) => last = e,
        }
    }
    Err(last)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_padding() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"foob"), "Zm9vYg==");
    }

    #[test]
    fn utf16_payload_has_bom() {
        assert_eq!(
            PipeClipboard::clip_exe().payload("aé"),
            [0xff, 0xfe, b'a', 0, 0xe9, 0]
        );
    }

    #[test]
    fn linux_tool_follows_injected_env() {
        let n = |pairs: &[(&str, &str)]| {
            default_chain(Platform::Linux, &Env::from_pairs(pairs.iter().copied())).len()
        };
        assert_eq!(n(&[]), 1);
        assert_eq!(n(&[("WAYLAND_DISPLAY", "wayland-0")]), 2);
        assert_eq!(n(&[("WAYLAND_DISPLAY", "w"), ("DISPLAY", ":0")]), 3);
    }

    struct Fake(io::Result<&'static str>);
    impl ClipboardSource for Fake {
        fn paste(&self) -> io::Result<String> {
            match &self.0 {
                Ok(t) => Ok((*t).to_string()),
                Err(e) => Err(io::Error::new(e.kind(), e.to_string())),
            }
        }
    }

    #[test]
    fn first_working_source_wins_else_last_error() {
        let bad = |m: &'static str| -> Box<dyn ClipboardSource> {
            Box::new(Fake(Err(io::Error::other(m))))
        };
        let ok: Box<dyn ClipboardSource> = Box::new(Fake(Ok("hi")));
        assert_eq!(read_clipboard(&[bad("a"), ok]).unwrap(), "hi");
        assert_eq!(
            read_clipboard(&[bad("a"), bad("b")])
                .unwrap_err()
                .to_string(),
            "b"
        );
        assert!(
            read_clipboard(&[])
                .unwrap_err()
                .to_string()
                .contains("no clipboard tool")
        );
    }

    #[test]
    fn linux_paste_tools_follow_injected_env() {
        let n = |pairs: &[(&str, &str)]| {
            paste_chain(Platform::Linux, &Env::from_pairs(pairs.iter().copied())).len()
        };
        assert_eq!(n(&[]), 0, "no display, no tool");
        assert_eq!(
            paste_chain(Platform::MacOs, &Env::from_pairs([("A", "b")])).len(),
            1
        );
    }

    /// Run through `sh <file>`, never exec the file itself: executing a script that was just
    /// written races with other test threads forking (ETXTBSY, "Text file busy").
    #[cfg(unix)]
    fn script(dir: &Path, body: &str) -> PasteCommand {
        let p = dir.join("tool.sh");
        std::fs::write(&p, format!("{body}\n")).unwrap();
        PasteCommand {
            program: "/bin/sh".into(),
            args: vec![p.display().to_string()],
        }
    }

    #[cfg(unix)]
    #[test]
    fn paste_command_reads_caps_fails_and_times_out() {
        let d = tempfile::tempdir().unwrap();
        assert_eq!(
            script(d.path(), "printf 'a\\nb\\303\\251'")
                .paste()
                .unwrap(),
            "a\nbé"
        );
        let err = script(d.path(), "exit 1").paste().unwrap_err();
        assert!(err.to_string().contains("failed"), "{err}");
        let big = script(d.path(), "head -c 5000000 /dev/zero | tr '\\0' x")
            .paste()
            .unwrap();
        assert_eq!(big.len(), PASTE_MAX_BYTES);
        let t0 = std::time::Instant::now();
        let err = script(d.path(), "sleep 30").paste().unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::TimedOut);
        assert!(t0.elapsed() < Duration::from_secs(10));
        let missing = PasteCommand {
            program: d.path().join("nope"),
            args: vec![],
        };
        assert!(missing.paste().is_err());
    }

    /// The tool must see EOF: a sink that reads to the end then writes a file proves it.
    #[cfg(unix)]
    #[test]
    fn pipe_clipboard_closes_stdin_so_tool_receives_text() {
        let d = tempfile::tempdir().unwrap();
        let out = d.path().join("out.txt");
        let sh = d.path().join("sink.sh");
        std::fs::write(&sh, format!("cat > '{}'\n", out.display())).unwrap();
        let sink = PipeClipboard {
            program: "/bin/sh".into(),
            args: vec![sh.display().to_string()],
            encoding: Encoding::Utf8,
        };
        sink.copy("héllo\nworld").unwrap();
        let t0 = std::time::Instant::now();
        while !std::fs::read_to_string(&out).is_ok_and(|s| s == "héllo\nworld") {
            assert!(t0.elapsed() < Duration::from_secs(5), "tool never got EOF");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// Real pbcopy round trip; the user's clipboard is restored afterwards.
    #[cfg(target_os = "macos")]
    #[test]
    fn pbcopy_really_receives_text() {
        let paste = || {
            PasteCommand::new("/usr/bin/pbpaste".into(), &[])
                .paste()
                .ok()
        };
        let saved = paste();
        let text = format!("synthesis-clip-test-{}", std::process::id());
        PipeClipboard::pbcopy().copy(&text).unwrap();
        let t0 = std::time::Instant::now();
        let mut got = paste();
        while got.as_deref() != Some(text.as_str()) && t0.elapsed() < Duration::from_secs(5) {
            std::thread::sleep(Duration::from_millis(50));
            got = paste();
        }
        if let Some(s) = saved {
            let _ = PipeClipboard::pbcopy().copy(&s);
        }
        assert_eq!(got.as_deref(), Some(text.as_str()));
    }

    #[test]
    fn osc52_sink_is_last_fallback() {
        let chain = default_chain(Platform::Other, &Env::from_pairs([("A", "b")]));
        assert_eq!(chain.len(), 1, "no tool: only OSC 52 remains");
    }

    #[cfg(windows)]
    #[test]
    fn clip_exe_is_absolute_system32() {
        assert!(
            PipeClipboard::clip_exe()
                .program
                .ends_with("System32\\clip.exe")
        );
    }
}
