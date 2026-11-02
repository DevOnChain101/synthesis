//! An image from the clipboard, for Alt+V.
//!
//! WSL: the image is on the WINDOWS clipboard. WSLg shows it to `wl-paste` only as a huge
//! `image/bmp` whose conversion fails, so the Windows route is tried first: `powershell.exe`
//! (found by absolute path under the Windows drive mount, never through PATH, which
//! `appendWindowsPath=false` may not contain) runs a script that writes the image as base64 PNG
//! to stdout. Linux tools (`wl-paste` / `xclip` asking for `image/png`) are the fallback.
//! WSL is recognised from the kernel, not from environment variables. Everything that comes back
//! is untrusted bytes: bounded in size and time, PNG magic and pixel size checked.
//! Windows itself uses the same script through `System32`. Not implemented elsewhere.
//!
//! Probing, argv and decoding are pure functions and the process run is behind [`Exec`], so all of
//! it is tested without WSL.

use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// Longest wait for the helper process; it is killed after this.
pub const TIMEOUT: Duration = Duration::from_secs(5);
/// Largest decoded PNG. The session store refuses a message over 8 MiB, and an image travels as
/// base64 inside it: 4 MB of PNG is 5.4 MB of base64, which leaves room for the text and for a
/// second small image (the per-message total is checked where images are attached).
pub const MAX_PNG: usize = 4 << 20;
/// Largest stdout taken from the helper: the base64 of [`MAX_PNG`].
pub const MAX_STDOUT: usize = MAX_PNG / 3 * 4 + 16;
/// Largest width and height in pixels.
pub const MAX_DIM: u32 = 8000;
const PNG_MAGIC: [u8; 8] = [0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];
/// Exit code of the script when the clipboard holds no image.
const EXIT_NO_IMAGE: i32 = 2;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClipImage {
    pub png: Vec<u8>,
    pub width: u32,
    pub height: u32,
    /// base64 of `png`, made on the worker thread (not on the UI thread that attaches it).
    pub base64: String,
}

/// A one-line, user-facing reason.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ImageError {
    /// The clipboard holds no image.
    NoImage(String),
    Failed(String),
}

impl std::fmt::Display for ImageError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let (Self::NoImage(m) | Self::Failed(m)) = self;
        f.write_str(m)
    }
}

pub trait ImageSource: Send + Sync {
    fn read(&self) -> Result<ClipImage, ImageError>;
}

// ---------------------------------------------------------------- WSL probing (pure)

/// WSL from the kernel's own words (`/proc/sys/kernel/osrelease`, `/proc/version`): both WSL1
/// (`...-Microsoft`) and WSL2 (`...-microsoft-standard-WSL2`) contain "microsoft". `read` returns a
/// file's text.
pub fn is_wsl(read: &dyn Fn(&str) -> Option<String>) -> bool {
    ["/proc/sys/kernel/osrelease", "/proc/version"]
        .iter()
        .filter_map(|p| read(p))
        .any(|t| t.to_lowercase().contains("microsoft"))
}

/// Where the Windows C: drive is mounted, from `/proc/mounts` (`C: /mnt/c drvfs ...`, or the
/// `9p` / `C:\` forms).
pub fn windows_c_mount(proc_mounts: &str) -> Option<PathBuf> {
    proc_mounts.lines().find_map(|l| {
        let mut f = l.split_whitespace();
        let (dev, mnt, fstype) = (f.next()?, f.next()?, f.next()?);
        // bytes, not `dev[..2]`: a non-ASCII device name must not panic on a char boundary
        let d = dev.as_bytes();
        let drive = d.len() >= 2 && d[0].eq_ignore_ascii_case(&b'c') && d[1] == b':';
        let rest_ok = d.len() == 2 || matches!(d.get(2), Some(b'\\' | b'/'));
        // /proc/mounts escapes spaces as \040
        (drive && rest_ok && matches!(fstype, "drvfs" | "9p"))
            .then(|| PathBuf::from(mnt.replace("\\040", " ")))
    })
}

const PS_REL: &str = "Windows/System32/WindowsPowerShell/v1.0/powershell.exe";

/// `powershell.exe` candidates in the order to try: under the real C: mount, then `/mnt/c`.
pub fn powershell_candidates(proc_mounts: &str) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = windows_c_mount(proc_mounts)
        .into_iter()
        .chain([PathBuf::from("/mnt/c")])
        .map(|m| m.join(PS_REL))
        .collect();
    v.dedup();
    v
}

/// The first candidate that is a file.
pub fn find_powershell(proc_mounts: &str, is_file: &dyn Fn(&Path) -> bool) -> Option<PathBuf> {
    powershell_candidates(proc_mounts)
        .into_iter()
        .find(|p| is_file(p))
}

/// Say it once when the Windows route cannot be used.
pub const INTEROP_OFF: &str =
    "WSL interop is off: cannot read the Windows clipboard; enable [interop] in /etc/wsl.conf";
pub const NO_WINDOWS_IMAGE: &str =
    "no image on the Windows clipboard (a file copied in Explorer is not an image)";

// ---------------------------------------------------------------- the script and its output

/// One argv element for `-Command`, no shell. `Clipboard.GetImage()` needs `-STA`; it also works
/// in Windows PowerShell 5.1 (`Get-Clipboard -Format Image` does not).
pub const PS_SCRIPT: &str = "Add-Type -AssemblyName System.Windows.Forms; Add-Type -AssemblyName System.Drawing; \
$img = [System.Windows.Forms.Clipboard]::GetImage(); if ($null -eq $img) { exit 2 }; \
$ms = New-Object System.IO.MemoryStream; $img.Save($ms, [System.Drawing.Imaging.ImageFormat]::Png); \
[Console]::Out.Write([Convert]::ToBase64String($ms.ToArray()))";

pub fn ps_args() -> Vec<String> {
    [
        "-NoProfile",
        "-NonInteractive",
        "-STA",
        "-Command",
        PS_SCRIPT,
    ]
    .iter()
    .map(|s| (*s).to_string())
    .collect()
}

/// Standard base64, whitespace ignored; `None` on any other byte or bad padding.
pub fn decode_base64(input: &[u8]) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(input.len() / 4 * 3);
    let (mut acc, mut bits, mut pad) = (0u32, 0u32, 0usize);
    for &b in input.iter().filter(|b| !b.is_ascii_whitespace()) {
        let v = match b {
            b'A'..=b'Z' => b - b'A',
            b'a'..=b'z' => b - b'a' + 26,
            b'0'..=b'9' => b - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            b'=' => {
                pad += 1;
                continue;
            }
            _ => return None,
        };
        if pad > 0 {
            return None; // data after padding
        }
        acc = (acc << 6) | u32::from(v);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
            acc &= (1 << bits) - 1;
        }
    }
    (pad <= 2).then_some(out)
}

/// PNG magic, IHDR present, pixel size in `1..=MAX_DIM`, bytes <= `MAX_PNG`.
pub fn validate_png(png: Vec<u8>) -> Result<ClipImage, ImageError> {
    let bad = |m: &str| {
        Err(ImageError::Failed(format!(
            "the clipboard image was refused: {m}"
        )))
    };
    if png.len() > MAX_PNG {
        return bad("larger than 4 MB");
    }
    if png.len() < 24 || png[..8] != PNG_MAGIC || &png[12..16] != b"IHDR" {
        return bad("not a PNG");
    }
    let dim = |i: usize| u32::from_be_bytes([png[i], png[i + 1], png[i + 2], png[i + 3]]);
    let (width, height) = (dim(16), dim(20));
    if width == 0 || height == 0 || width > MAX_DIM || height > MAX_DIM {
        return bad(&format!(
            "{width}x{height} pixels (the limit is {MAX_DIM}x{MAX_DIM})"
        ));
    }
    let base64 = crate::clipboard::base64(&png);
    Ok(ClipImage {
        png,
        width,
        height,
        base64,
    })
}

/// Stdout of the script: base64 of a PNG.
pub fn png_from_base64(stdout: &[u8]) -> Result<ClipImage, ImageError> {
    let png = decode_base64(stdout).ok_or_else(|| {
        ImageError::Failed("the clipboard image was refused: not valid base64".into())
    })?;
    validate_png(png)
}

// ---------------------------------------------------------------- running the helper

pub struct Output {
    pub code: Option<i32>,
    pub stdout: Vec<u8>,
    /// More than the limit was written: the rest was dropped and the process killed.
    pub truncated: bool,
    pub timed_out: bool,
}

/// Runs a program without a shell; a seam for tests.
pub trait Exec: Send + Sync {
    fn run(&self, program: &Path, args: &[String]) -> io::Result<Output>;
}

/// The real thing: stdin closed, stderr dropped, stdout read up to `max` bytes, killed after
/// `timeout`.
pub struct RealExec {
    pub timeout: Duration,
    pub max: usize,
}

impl Default for RealExec {
    fn default() -> Self {
        Self {
            timeout: TIMEOUT,
            max: MAX_STDOUT,
        }
    }
}

/// Kills the child AND what it started (powershell.exe starts helpers; a script can fork): unix
/// kills the process group (the child leads its own), Windows uses `taskkill /T`. Then reaps it.
pub fn kill_tree(child: &mut std::process::Child) {
    let pid = child.id();
    #[cfg(unix)]
    {
        let group = format!("-{pid}");
        for kill in ["/bin/kill", "/usr/bin/kill"] {
            let ok = Command::new(kill)
                .args(["-KILL", "--", &group])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .is_ok_and(|s| s.success());
            if ok {
                break;
            }
        }
    }
    #[cfg(windows)]
    {
        let _ = Command::new(synthesis_platform::system32("taskkill.exe"))
            .args(["/T", "/F", "/PID", &pid.to_string()])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
    let _ = child.kill(); // the child itself, if the group kill was not possible
    let _ = child.wait();
}

/// How long to wait for the reader thread once the child is gone (its pipe is closed by then).
const READER_GRACE: Duration = Duration::from_millis(500);

impl Exec for RealExec {
    fn run(&self, program: &Path, args: &[String]) -> io::Result<Output> {
        self.run_with(program, args, None, None)
    }
}

impl RealExec {
    /// [`Exec::run`] in `cwd`, and with exactly `env` as the environment when given (everything
    /// else cleared): for helpers that must not inherit the process environment.
    pub fn run_with(
        &self,
        program: &Path,
        args: &[String],
        cwd: Option<&Path>,
        env: Option<&[(String, String)]>,
    ) -> io::Result<Output> {
        let mut cmd = Command::new(program);
        if let Some(d) = cwd {
            cmd.current_dir(d);
        }
        if let Some(e) = env {
            cmd.env_clear().envs(e.iter().map(|(k, v)| (k, v)));
        }
        cmd.args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        #[cfg(unix)]
        std::os::unix::process::CommandExt::process_group(&mut cmd, 0);
        #[cfg(windows)]
        std::os::windows::process::CommandExt::creation_flags(&mut cmd, 0x0800_0000);
        let mut child = cmd.spawn()?;
        let mut out = child.stdout.take().expect("stdout was piped");
        let max = self.max;
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            let _ = (&mut out).take(max as u64 + 1).read_to_end(&mut buf);
            let truncated = buf.len() > max;
            buf.truncate(max);
            let _ = tx.send((buf, truncated));
        });
        let deadline = Instant::now() + self.timeout;
        let mut got: Option<(Vec<u8>, bool)> = None;
        let (mut code, mut timed_out) = (None, false);
        loop {
            if got.is_none() {
                got = rx.try_recv().ok();
            }
            match child.try_wait() {
                Ok(Some(s)) => {
                    code = s.code();
                    break;
                }
                Ok(None) => {}
                Err(e) => {
                    kill_tree(&mut child);
                    return Err(e);
                }
            }
            // the limit was hit: more output is coming that nobody reads, so stop the child;
            // a reader that finished normally only means the output is complete (wait for the
            // exit code)
            if got.as_ref().is_some_and(|g| g.1) {
                kill_tree(&mut child);
                break;
            }
            if Instant::now() >= deadline {
                timed_out = true;
                kill_tree(&mut child);
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let (stdout, truncated) = match got {
            Some(g) => g,
            None => rx.recv_timeout(READER_GRACE).unwrap_or_default(),
        };
        Ok(Output {
            code,
            stdout,
            truncated,
            timed_out,
        })
    }
}

/// Reads the image through the Windows PowerShell script.
fn via_powershell(exec: &dyn Exec, exe: &Path) -> Result<ClipImage, ImageError> {
    let o = exec
        .run(exe, &ps_args())
        .map_err(|e| ImageError::Failed(format!("cannot run powershell.exe: {e}")))?;
    if o.timed_out {
        return Err(ImageError::Failed(
            "powershell.exe did not answer within 5 s; stopped".into(),
        ));
    }
    if o.truncated {
        return Err(ImageError::Failed(
            "the clipboard image was refused: larger than 4 MB".into(),
        ));
    }
    match o.code {
        Some(0) => png_from_base64(&o.stdout),
        Some(EXIT_NO_IMAGE) => Err(ImageError::NoImage(NO_WINDOWS_IMAGE.into())),
        c => Err(ImageError::Failed(format!(
            "powershell.exe failed (exit {c:?})"
        ))),
    }
}

/// WSL: Windows clipboard first, then Linux tools asking for `image/png`.
pub struct WslImages {
    /// `None`: interop is off or the file is missing.
    pub powershell: Option<PathBuf>,
    /// `(program, args)`, absolute paths from fixed directories.
    pub linux: Vec<(PathBuf, Vec<String>)>,
    pub exec: Box<dyn Exec>,
}

impl ImageSource for WslImages {
    fn read(&self) -> Result<ClipImage, ImageError> {
        let first: Option<ImageError>;
        match &self.powershell {
            Some(exe) => match via_powershell(&*self.exec, exe) {
                Ok(i) => return Ok(i),
                // an image-less Windows clipboard is an answer, not a reason to ask Linux
                Err(e @ ImageError::NoImage(_)) if self.linux.is_empty() => return Err(e),
                Err(e) => first = Some(e),
            },
            None => first = Some(ImageError::Failed(INTEROP_OFF.into())),
        }
        for (prog, args) in &self.linux {
            if let Ok(o) = self.exec.run(prog, args)
                && o.code == Some(0)
                && !o.truncated
                && !o.timed_out
                && let Ok(i) = validate_png(o.stdout)
            {
                return Ok(i);
            }
        }
        Err(first.unwrap_or_else(|| ImageError::NoImage("no image on the clipboard".into())))
    }
}

/// Windows: the same script through `System32`.
pub struct WindowsImages {
    pub powershell: PathBuf,
    pub exec: Box<dyn Exec>,
}

impl ImageSource for WindowsImages {
    fn read(&self) -> Result<ClipImage, ImageError> {
        via_powershell(&*self.exec, &self.powershell)
    }
}

/// The image source of this machine, or `None` (with the reason from [`unavailable`]).
pub fn default_source(platform: crate::Platform) -> Option<Box<dyn ImageSource>> {
    match platform {
        crate::Platform::Windows => Some(Box::new(WindowsImages {
            powershell: synthesis_platform::system32(r"WindowsPowerShell\v1.0\powershell.exe"),
            exec: Box::new(RealExec::default()),
        })),
        crate::Platform::Linux => {
            let read = |p: &str| std::fs::read_to_string(p).ok();
            if !is_wsl(&read) {
                return None;
            }
            let mounts = read("/proc/mounts").unwrap_or_default();
            let linux = [
                ("wl-paste", vec!["--no-newline", "--type", "image/png"]),
                (
                    "xclip",
                    vec!["-selection", "clipboard", "-t", "image/png", "-o"],
                ),
            ]
            .into_iter()
            .filter_map(|(n, a)| {
                crate::clipboard::find_tool(n)
                    .map(|p| (p, a.into_iter().map(String::from).collect()))
            })
            .collect();
            Some(Box::new(WslImages {
                powershell: find_powershell(&mounts, &|p| p.is_file()),
                linux,
                exec: Box::new(RealExec::default()),
            }))
        }
        _ => None,
    }
}

/// Why Alt+V has nothing to read the clipboard with.
pub fn unavailable() -> &'static str {
    "image paste works on WSL and Windows in this build; attach a file with @path instead"
}

#[cfg(test)]
mod tests {
    use super::*;

    fn png(w: u32, h: u32) -> Vec<u8> {
        let mut v = PNG_MAGIC.to_vec();
        v.extend([0, 0, 0, 13]);
        v.extend(b"IHDR");
        v.extend(w.to_be_bytes());
        v.extend(h.to_be_bytes());
        v.extend([8, 6, 0, 0, 0]);
        v
    }

    fn b64(b: &[u8]) -> String {
        crate::clipboard::base64(b)
    }

    #[test]
    fn wsl_is_read_from_the_kernel_strings() {
        let r = |a: Option<&'static str>, b: Option<&'static str>| {
            is_wsl(&|p| match p {
                "/proc/sys/kernel/osrelease" => a.map(String::from),
                "/proc/version" => b.map(String::from),
                _ => None,
            })
        };
        assert!(r(Some("5.15.153.1-microsoft-standard-WSL2\n"), None));
        assert!(r(Some("4.4.0-19041-Microsoft"), None));
        assert!(r(
            None,
            Some("Linux version 5.15.0 (root@x) Microsoft-standard #1 SMP")
        ));
        // "wsl" alone is not enough: only the word the kernel of both WSL versions carries
        assert!(!r(
            Some("6.1.0-custom-wsl-lookalike"),
            Some("Linux version 6.1 wsl2")
        ));
        assert!(!r(
            Some("6.8.0-45-generic"),
            Some("Linux version 6.8.0-45-generic (buildd@lcy02)")
        ));
        assert!(!r(None, None));
    }

    #[test]
    fn powershell_is_found_under_the_real_mount_then_mnt_c_by_absolute_path() {
        let mounts =
            "none / ext4 rw 0 0\nC: /mnt/win-c drvfs rw,noatime 0 0\nD: /mnt/d drvfs rw 0 0\n";
        assert_eq!(windows_c_mount(mounts), Some("/mnt/win-c".into()));
        assert_eq!(
            windows_c_mount("C:\\ /mnt/c 9p rw 0 0"),
            Some("/mnt/c".into())
        );
        assert_eq!(
            windows_c_mount("c: /mnt/my\\040c drvfs rw 0 0"),
            Some("/mnt/my c".into())
        );
        assert_eq!(
            windows_c_mount("D: /mnt/d drvfs rw 0 0\nnone / ext4 rw 0 0"),
            None
        );
        assert_eq!(
            windows_c_mount("C: /mnt/c ext4 rw 0 0"),
            None,
            "only drvfs/9p are Windows drives"
        );
        // non-ASCII device names: no panic on a char boundary, and no match
        for l in [
            "aé: /mnt/x drvfs rw 0 0",
            "é:\\ /mnt/x 9p rw 0 0",
            "Cé /mnt/x drvfs rw 0 0",
            "日本 /x 9p rw 0 0",
            "c /x 9p rw 0 0",
            "é",
            "C:é /mnt/x drvfs rw 0 0",
        ] {
            assert_eq!(windows_c_mount(l), None, "{l}");
        }
        let want = |m: &str| {
            PathBuf::from(m).join("Windows/System32/WindowsPowerShell/v1.0/powershell.exe")
        };
        assert_eq!(
            powershell_candidates(mounts),
            [want("/mnt/win-c"), want("/mnt/c")]
        );
        assert_eq!(
            powershell_candidates("none / ext4 rw 0 0"),
            [want("/mnt/c")]
        );
        assert_eq!(
            find_powershell(mounts, &|p| p == want("/mnt/c")),
            Some(want("/mnt/c"))
        );
        assert_eq!(
            find_powershell(mounts, &|_| false),
            None,
            "interop off or missing: no PATH lookup"
        );
        assert!(find_powershell(mounts, &|_| true).unwrap().is_absolute());
    }

    #[test]
    fn the_script_is_one_argv_element_without_a_shell() {
        let a = ps_args();
        assert_eq!(
            &a[..4],
            ["-NoProfile", "-NonInteractive", "-STA", "-Command"]
        );
        assert_eq!(a.len(), 5);
        assert!(a[4].contains("Clipboard]::GetImage()") && a[4].contains("exit 2"));
        assert!(!a[4].contains("Get-Clipboard"));
    }

    #[test]
    fn base64_decodes_and_the_png_is_checked() {
        for n in 0..6u8 {
            let data: Vec<u8> = (0..n).collect();
            assert_eq!(decode_base64(b64(&data).as_bytes()), Some(data));
        }
        assert_eq!(decode_base64(b"QQ==\r\n"), Some(b"A".to_vec()));
        assert_eq!(decode_base64(b"Q$=="), None);
        assert_eq!(decode_base64(b"QQ==QQ"), None, "data after padding");
        assert_eq!(decode_base64(b"Q==="), None);
        let ok = png_from_base64(b64(&png(640, 480)).as_bytes()).unwrap();
        assert_eq!((ok.width, ok.height), (640, 480));
        let refused = |r: Result<ClipImage, ImageError>, why: &str| {
            assert!(
                matches!(&r, Err(ImageError::Failed(m)) if m.contains(why)),
                "{r:?}"
            );
        };
        refused(png_from_base64(b"@@@"), "base64");
        refused(
            png_from_base64(b64(b"GIF89a-not-a-png-at-all-xxxx").as_bytes()),
            "not a PNG",
        );
        refused(validate_png(png(8001, 10)), "8001x10");
        refused(validate_png(png(10, 8001)), "pixels");
        refused(validate_png(png(0, 10)), "pixels");
        assert!(validate_png(png(8000, 8000)).is_ok());
        let mut big = png(10, 10);
        big.resize(MAX_PNG + 1, 0);
        refused(validate_png(big), "4 MB");
        let mut bad_ihdr = png(10, 10);
        bad_ihdr[12] = b'X';
        refused(validate_png(bad_ihdr), "not a PNG");
    }

    struct Fake(
        Vec<(Vec<u8>, Option<i32>, bool, bool)>,
        std::sync::Mutex<usize>,
    );
    impl Fake {
        fn new(v: Vec<(Vec<u8>, Option<i32>, bool, bool)>) -> Box<Self> {
            Box::new(Self(v, std::sync::Mutex::new(0)))
        }
    }
    impl Exec for Fake {
        fn run(&self, _: &Path, _: &[String]) -> io::Result<Output> {
            let mut i = self.1.lock().unwrap();
            let (stdout, code, truncated, timed_out) = self
                .0
                .get(*i)
                .cloned()
                .ok_or_else(|| io::Error::other("no more"))?;
            *i += 1;
            Ok(Output {
                code,
                stdout,
                truncated,
                timed_out,
            })
        }
    }
    fn wsl(exec: Box<dyn Exec>, ps: bool, linux: bool) -> WslImages {
        WslImages {
            powershell: ps
                .then(|| "/mnt/c/Windows/System32/WindowsPowerShell/v1.0/powershell.exe".into()),
            linux: if linux {
                vec![("/usr/bin/wl-paste".into(), vec![])]
            } else {
                vec![]
            },
            exec,
        }
    }

    #[test]
    fn the_windows_route_answers_first_and_every_refusal_is_one_line() {
        let good = b64(&png(3, 2)).into_bytes();
        let r = wsl(
            Fake::new(vec![(good.clone(), Some(0), false, false)]),
            true,
            true,
        )
        .read();
        assert_eq!(r.unwrap().png, png(3, 2));
        // exit 2: no image, and the Linux tools are not asked when there are none
        let e = wsl(
            Fake::new(vec![(vec![], Some(2), false, false)]),
            true,
            false,
        )
        .read()
        .unwrap_err();
        assert!(
            matches!(&e, ImageError::NoImage(m) if m.contains("Windows clipboard") && m.contains("Explorer"))
        );
        // timeout, too much output, garbage, other exit codes
        let one = |o: (Vec<u8>, Option<i32>, bool, bool)| {
            wsl(Fake::new(vec![o]), true, false)
                .read()
                .unwrap_err()
                .to_string()
        };
        assert!(one((vec![], None, false, true)).contains("5 s"));
        assert!(one((vec![b'A'; 10], Some(0), true, false)).contains("4 MB"));
        assert!(one((b"!!!".to_vec(), Some(0), false, false)).contains("base64"));
        assert!(
            one((b64(&png(9000, 9000)).into_bytes(), Some(0), false, false)).contains("9000x9000")
        );
        assert!(one((vec![], Some(1), false, false)).contains("exit"));
        for m in [
            one((vec![], None, false, true)),
            one((vec![], Some(1), false, false)),
        ] {
            assert!(!m.contains('\n'));
        }
    }

    #[test]
    fn without_powershell_the_notice_is_the_interop_one_and_linux_tools_are_the_fallback() {
        let e = wsl(Fake::new(vec![]), false, false).read().unwrap_err();
        assert_eq!(e, ImageError::Failed(INTEROP_OFF.into()));
        assert!(INTEROP_OFF.contains("[interop]") && INTEROP_OFF.contains("/etc/wsl.conf"));
        // the Linux tool answers with a PNG
        let r = wsl(
            Fake::new(vec![(png(5, 5), Some(0), false, false)]),
            false,
            true,
        )
        .read();
        assert_eq!(r.unwrap().width, 5);
        // a Linux tool that yields a bmp or fails: the earlier (interop) reason stays
        let e = wsl(
            Fake::new(vec![(b"BM....".to_vec(), Some(0), false, false)]),
            false,
            true,
        )
        .read()
        .unwrap_err();
        assert_eq!(e, ImageError::Failed(INTEROP_OFF.into()));
        // powershell failed, Linux tools asked, none has an image: the first reason
        let e = wsl(
            Fake::new(vec![
                (vec![], Some(2), false, false),
                (vec![], Some(1), false, false),
            ]),
            true,
            true,
        )
        .read()
        .unwrap_err();
        assert!(matches!(e, ImageError::NoImage(_)));
    }

    #[cfg(unix)]
    #[test]
    fn the_real_runner_kills_on_timeout_and_cuts_output_at_the_limit() {
        let sh = |script: &str| ["-c".to_string(), script.to_string()];
        let q = RealExec {
            timeout: Duration::from_millis(300),
            max: 100,
        };
        let t = Instant::now();
        let o = q.run(Path::new("/bin/sh"), &sh("sleep 20")).unwrap();
        assert!(o.timed_out && o.code.is_none() && t.elapsed() < Duration::from_secs(5));
        let o = q
            .run(
                Path::new("/bin/sh"),
                &sh("head -c 100000 /dev/zero | tr '\\0' A"),
            )
            .unwrap();
        assert!(o.truncated && o.stdout.len() == 100, "{}", o.stdout.len());
        let o = q
            .run(Path::new("/bin/sh"), &sh("printf hi; exit 2"))
            .unwrap();
        assert_eq!(
            (o.code, o.stdout.as_slice(), o.truncated),
            (Some(2), &b"hi"[..], false)
        );
        assert!(q.run(Path::new("/no/such/powershell.exe"), &[]).is_err());
    }

    #[cfg(unix)]
    fn alive(pid: &str) -> bool {
        Command::new("/bin/kill")
            .args(["-0", pid])
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
    }

    #[cfg(unix)]
    #[test]
    fn a_timeout_kills_the_whole_process_group_not_just_the_shell() {
        let dir = tempfile::tempdir().unwrap();
        let pidfile = dir.path().join("pid");
        let script = format!("sleep 30 & echo $! > {}; wait", pidfile.display());
        let q = RealExec {
            timeout: Duration::from_millis(500),
            max: 100,
        };
        let o = q.run(Path::new("/bin/sh"), &["-c".into(), script]).unwrap();
        assert!(o.timed_out);
        let pid = std::fs::read_to_string(&pidfile)
            .unwrap()
            .trim()
            .to_string();
        std::thread::sleep(Duration::from_millis(200));
        assert!(!alive(&pid), "the grandchild survived the timeout");
    }

    #[cfg(unix)]
    #[test]
    fn a_closed_stdout_is_not_a_reason_to_kill_the_exit_code_is_still_read() {
        let q = RealExec {
            timeout: Duration::from_secs(5),
            max: 100,
        };
        let o = q
            .run(
                Path::new("/bin/sh"),
                &["-c".into(), "echo hi; exec 1>&-; sleep 0.4; exit 7".into()],
            )
            .unwrap();
        assert_eq!(
            (o.code, o.stdout.as_slice(), o.timed_out),
            (Some(7), &b"hi\n"[..], false)
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_pipe_held_open_by_a_grandchild_cannot_stall_the_worker() {
        let q = RealExec {
            timeout: Duration::from_millis(500),
            max: 100,
        };
        let t = Instant::now();
        // the shell exits at once, a background sleep keeps the pipe open for 3 s
        let o = q
            .run(
                Path::new("/bin/sh"),
                &["-c".into(), "sleep 3 & exit 0".into()],
            )
            .unwrap();
        assert_eq!(o.code, Some(0));
        assert!(t.elapsed() < Duration::from_secs(2), "{:?}", t.elapsed());
    }
}
