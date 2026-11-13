//! The file and image methods of `synt engine --stdio` (protocol 3, docs/specs/desktop.md section 8). They run
//! in the engine, on its own machine (inside WSL, the distro's Linux): the client names a path RELATIVE to the
//! session folder (never absolute), every path is validated here, and what comes back is bounded and made
//! printable. Paths in answers are display text for the client, never something it opens.
//!
//! - `fs.list` / `fs.git_*`: [`synthesis_view::project`]'s scans (plain walk when the folder is not trusted,
//!   git through its hardened wrapper only when it is);
//! - `fs.read`: a text preview with the refusals of the window's own preview (outside the folder, links out,
//!   secret files, not regular, binary) plus hard links, `.git`, no blocking on a pipe, 256 KB;
//! - `image.put`: a pasted or dropped image in bounded base64 chunks, sniffed, held in memory until `send`
//!   names it by the id the engine issued.

use std::path::{Component, Path};
use std::time::{Duration, Instant};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use serde_json::{Value, json};
use synthesis_config::Env;
use synthesis_harness::{ImageAttachment, sanitize_for_display as clean};
use synthesis_view::project::{self, ProjectData};

/// Rows of the tree per answer.
pub const PAGE_ROWS: usize = 200;
const MAX_REL: usize = 1024;
pub const PREVIEW_BYTES: usize = 256 * 1024;
pub const PREVIEW_LINES: usize = 2000;
/// One image, the images waiting for a `send`, their bytes (the window's own limits), and one chunk's base64.
pub const MAX_IMAGE: usize = 5 << 20;
pub const MAX_IMAGES: usize = 5;
pub const MAX_IMAGE_BYTES: usize = 6 << 20;
pub const MAX_CHUNK_B64: usize = 512 << 10;
/// An upload that stays silent this long is dropped.
const UPLOAD_IDLE: Duration = Duration::from_secs(60);

/// A path of the session folder as the client names it: relative, plain components only (no `..`, no `.`, no
/// drive or root), no control characters, and nothing in `.git`. `allow_empty`: the folder itself.
pub fn check_rel(p: &str, allow_empty: bool) -> Result<(), String> {
    if p.is_empty() {
        return if allow_empty {
            Ok(())
        } else {
            Err("not a path inside the project".into())
        };
    }
    let path = Path::new(p);
    if p.len() > MAX_REL
        || p.chars().any(char::is_control)
        || path.is_absolute()
        || path
            .components()
            .any(|c| !matches!(c, Component::Normal(n) if !n.eq_ignore_ascii_case(".git")))
    {
        return Err("not a path inside the project".into());
    }
    Ok(())
}

/// The tree of the session folder: git (hardened) when `trusted`, else a plain walk. The client never decides.
pub fn scan(root: &Path, trusted: bool, env: &Env) -> ProjectData {
    if trusted {
        project::scan(root, env)
    } else {
        project::scan_plain(root)
    }
}

/// The paths (`a/b/c`) of the rows, in row order.
fn row_paths(d: &ProjectData) -> Vec<String> {
    let mut stack: Vec<&str> = vec![];
    d.rows
        .iter()
        .map(|r| {
            stack.truncate(usize::from(r.depth));
            let p = stack
                .iter()
                .copied()
                .chain([r.name.as_str()])
                .collect::<Vec<_>>()
                .join("/");
            if r.dir {
                stack.push(&r.name);
            }
            p
        })
        .collect()
}

/// `[depth, name, dir, status]` rows of the scan under `dir` ("" is all of it), from `offset`, at most
/// [`PAGE_ROWS`].
pub fn list_of(d: &ProjectData, dir: &str, offset: usize, trusted: bool) -> Result<Value, String> {
    check_rel(dir, true)?;
    let paths = row_paths(d);
    let prefix = format!("{dir}/");
    let picked: Vec<usize> = if dir.is_empty() {
        (0..d.rows.len()).collect()
    } else {
        let at = (0..d.rows.len())
            .find(|i| d.rows[*i].dir && paths[*i] == dir)
            .ok_or("no such folder")?;
        (at + 1..d.rows.len())
            .take_while(|i| paths[*i].starts_with(&prefix))
            .collect()
    };
    let page: Vec<Value> = picked
        .iter()
        .skip(offset)
        .take(PAGE_ROWS)
        .map(|i| {
            let r = &d.rows[*i];
            json!([r.depth, clean(&r.name), r.dir, r.status.map(String::from)])
        })
        .collect();
    let end = offset.saturating_add(page.len());
    Ok(json!({
        "rows": page, "total": picked.len(),
        "next": (end < picked.len()).then_some(end),
        "truncated": d.truncated, "note": d.note.as_deref().map(clean), "trusted": trusted,
    }))
}

/// The changed files (`[path, letter]`) of a trusted folder, through the hardened git wrapper.
pub fn git_status(root: &Path, trusted: bool, env: &Env) -> Result<Value, String> {
    let d = git_scan(root, trusted, env)?;
    let files: Vec<Value> = row_paths(&d)
        .into_iter()
        .zip(&d.rows)
        .filter(|(_, r)| !r.dir && r.status.is_some())
        .map(|(p, r)| json!([clean(&p), r.status.map(String::from)]))
        .collect();
    Ok(json!({"files": files, "truncated": d.truncated, "note": d.note.as_deref().map(clean)}))
}

/// The diff of the tracked changes (bounded lines) and `[added, deleted, path]` of every changed file.
pub fn git_diff(root: &Path, trusted: bool, env: &Env) -> Result<Value, String> {
    let d = git_scan(root, trusted, env)?;
    let stats: Vec<Value> = d
        .stats
        .iter()
        .map(|(a, b, p)| json!([clean(a), clean(b), clean(p)]))
        .collect();
    Ok(
        json!({"diff": d.diff, "stats": stats, "truncated": d.truncated, "note": d.note.as_deref().map(clean)}),
    )
}

/// git runs only in a trusted folder: a repository's config can name programs.
fn git_scan(root: &Path, trusted: bool, env: &Env) -> Result<ProjectData, String> {
    if !trusted {
        return Err("the folder is not trusted: git is not run here".into());
    }
    Ok(project::scan(root, env))
}

/// A text preview of `rel` under `root` (the session folder). The same refusals as the window's own preview
/// plus: `.git` and secret names judged on where the file REALLY is as well as on the name given, a file with
/// other hard links (a link to something outside cannot be told from a file inside), a file on a filesystem
/// whose link counts cannot be trusted, and a file that is not opened as a plain file (a pipe never blocks the
/// engine, and the file never becomes a controlling terminal). The file is opened by its canonical relative
/// path beneath a handle of the folder with no link followed (Linux: `openat2` `RESOLVE_BENEATH |
/// RESOLVE_NO_SYMLINKS`; elsewhere cap-std), so a parent or the file swapped for a link meanwhile is refused;
/// it must be the very file that was looked at before (device and inode), and on Linux the path of the open
/// descriptor is checked once more. Reads at most [`PREVIEW_BYTES`], shows at most [`PREVIEW_LINES`] lines,
/// made printable.
#[cfg(not(unix))]
pub fn read(_root: &Path, _rel: &str) -> Result<Value, String> {
    // the race-free open is not available: an engine on Windows serves no previews
    Err("file previews are served by an engine on Linux or macOS only".into())
}

#[cfg(unix)]
pub fn read(root: &Path, rel: &str) -> Result<Value, String> {
    use std::io::Read as _;
    use std::os::unix::fs::MetadataExt as _;
    use synthesis_core::{PathPolicy, contains};
    check_rel(rel, false)?;
    let p = Path::new(rel);
    fn gone<E>(_: E) -> String {
        "cannot read that file".to_string()
    }
    let real_root = synthesis_core::canonicalize(root).map_err(gone)?;
    let real = synthesis_core::canonicalize(root.join(p)).map_err(gone)?;
    let inner = real
        .strip_prefix(&real_root)
        .ok()
        .filter(|_| contains(&real_root, &real, PathPolicy::NATIVE))
        .ok_or("the file is outside the project folder")?;
    // where the file really is, not only the name the client gave: a link inside the project that points at
    // `.git/config` or `.env` is refused too
    name_checks(inner)?;
    if [&real, p]
        .into_iter()
        .any(synthesis_core::secrets::is_secret_path)
    {
        return Err("a secret file (keys, .env, credentials) is not shown".into());
    }
    // the file as it is now: a regular file, not a link; what is opened must be this very file
    let before = std::fs::symlink_metadata(&real).map_err(gone)?;
    if !before.is_file() {
        return Err("not a regular file".into());
    }
    let file = open_beneath(&real_root, inner).map_err(gone)?;
    let meta = file.metadata().map_err(gone)?;
    if !meta.is_file() {
        return Err("not a regular file".into());
    }
    if (before.dev(), before.ino()) != (meta.dev(), meta.ino()) {
        return Err("the file changed while it was opened".into());
    }
    if meta.nlink() > 1 {
        return Err("a file with other hard links is not shown".into());
    }
    #[cfg(target_os = "linux")]
    {
        let fs = rustix::fs::fstatfs(&file).map_err(gone)?;
        #[allow(clippy::cast_sign_loss, clippy::unnecessary_cast)]
        let f_type = fs.f_type as u64;
        if links_unreliable(f_type) {
            return Err("this file system (a Windows drive, a network or FUSE mount) cannot tell whether a file has other hard links: not shown".into());
        }
        // what the descriptor really is, once more with the checks of the name
        let now = std::fs::read_link(format!(
            "/proc/self/fd/{}",
            rustix::fd::AsRawFd::as_raw_fd(&file)
        ))
        .map_err(gone)?;
        let now = now
            .strip_prefix(&real_root)
            .map_err(|_| "the file changed while it was opened".to_string())?;
        name_checks(now)?;
        if synthesis_core::secrets::is_secret_path(now) {
            return Err("a secret file (keys, .env, credentials) is not shown".into());
        }
    }
    let mut buf = Vec::new();
    file.take(PREVIEW_BYTES as u64 + 1)
        .read_to_end(&mut buf)
        .map_err(gone)?;
    let cut_bytes = buf.len() > PREVIEW_BYTES;
    buf.truncate(PREVIEW_BYTES);
    if buf[..buf.len().min(8192)].contains(&0) {
        return Err(format!("binary file ({} bytes)", meta.len()));
    }
    let text = String::from_utf8_lossy(&buf);
    let mut lines: Vec<String> = text.lines().map(clean).collect();
    let cut_lines = lines.len() > PREVIEW_LINES;
    lines.truncate(PREVIEW_LINES);
    Ok(json!({"lines": lines, "size": meta.len(), "cut": cut_bytes || cut_lines}))
}

/// No `.git` component (any case) in a path relative to the folder.
#[cfg(unix)]
fn name_checks(inner: &Path) -> Result<(), String> {
    if inner
        .components()
        .any(|c| c.as_os_str().eq_ignore_ascii_case(".git"))
    {
        return Err("a file inside .git is not shown".into());
    }
    Ok(())
}

/// Filesystems whose hard-link counts mean nothing: 9p (WSL's Windows drives), CIFS/SMB, FUSE (drvfs-like and
/// virtiofs mounts). Everything else (ext4, xfs, btrfs, tmpfs, overlayfs, NFS, ...) counts links correctly,
/// so a project on a Windows drive is refused here and a Linux one is not.
#[cfg(target_os = "linux")]
fn links_unreliable(f_type: u64) -> bool {
    matches!(
        f_type & 0xFFFF_FFFF,
        0x0102_1997 // 9p
            | 0xFF53_4D42 // cifs
            | 0xFE53_4D42 // smb2
            | 0x517B // smb
            | 0x6573_5546 // fuse (virtiofs, drvfs-like)
    )
}

/// Opens `rel` (canonical, relative) beneath `root` with no link followed, non-blocking, not a terminal.
#[cfg(target_os = "linux")]
fn open_beneath(root: &Path, rel: &Path) -> std::io::Result<std::fs::File> {
    use rustix::fs::{Mode, OFlags, ResolveFlags, open, openat2};
    let dir = open(
        root,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
        Mode::empty(),
    )?;
    let fd = openat2(
        &dir,
        rel,
        OFlags::RDONLY | OFlags::NONBLOCK | OFlags::NOCTTY | OFlags::CLOEXEC,
        Mode::empty(),
        ResolveFlags::BENEATH | ResolveFlags::NO_SYMLINKS | ResolveFlags::NO_MAGICLINKS,
    )?;
    Ok(std::fs::File::from(fd))
}

#[cfg(all(unix, not(target_os = "linux")))]
fn open_beneath(root: &Path, rel: &Path) -> std::io::Result<std::fs::File> {
    let dir = cap_std::fs::Dir::open_ambient_dir(root, cap_std::ambient_authority())?;
    let mut open = cap_std::fs::OpenOptions::new();
    open.read(true);
    cap_std::fs::OpenOptionsExt::custom_flags(&mut open, libc::O_NONBLOCK | libc::O_NOCTTY);
    Ok(dir.open_with(rel, &open)?.into_std())
}

// ---- images

/// The kinds accepted, by their first bytes (the declared type must agree).
fn sniff(b: &[u8]) -> Option<&'static str> {
    if b.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("image/png")
    } else if b.starts_with(&[0xFF, 0xD8, 0xFF]) {
        Some("image/jpeg")
    } else if b.starts_with(b"GIF87a") || b.starts_with(b"GIF89a") {
        Some("image/gif")
    } else if b.len() >= 12 && &b[..4] == b"RIFF" && &b[8..12] == b"WEBP" {
        Some("image/webp")
    } else {
        None
    }
}

/// Largest side and area an image may have (a model rejects or mis-reads more, and decoders pay for it).
pub const MAX_SIDE: u32 = 8000;
pub const MAX_PIXELS: u64 = 40_000_000;

/// `(width, height)` read from the header of a sniffed image; `None` for a truncated or malformed one.
fn dims(b: &[u8], mime: &str) -> Option<(u32, u32)> {
    let be32 = |i: usize| {
        b.get(i..i + 4)
            .map(|x| u32::from_be_bytes([x[0], x[1], x[2], x[3]]))
    };
    let le16 = |i: usize| {
        b.get(i..i + 2)
            .map(|x| u32::from(u16::from_le_bytes([x[0], x[1]])))
    };
    match mime {
        // the first chunk must be a 13-byte IHDR
        "image/png" => {
            (be32(8)? == 13 && b.get(12..16)? == b"IHDR").then_some((be32(16)?, be32(20)?))
        }
        "image/gif" => Some((le16(6)?, le16(8)?)),
        "image/jpeg" => {
            // walk the segments to the first start-of-frame marker
            let mut i = 2;
            loop {
                if *b.get(i)? != 0xFF {
                    return None;
                }
                let m = *b.get(i + 1)?;
                if m == 0xFF {
                    i += 1;
                    continue;
                }
                let len = usize::from(u16::from_be_bytes([*b.get(i + 2)?, *b.get(i + 3)?]));
                if (0xC0..=0xCF).contains(&m) && !matches!(m, 0xC4 | 0xC8 | 0xCC) {
                    let h = u32::from(u16::from_be_bytes([*b.get(i + 5)?, *b.get(i + 6)?]));
                    let w = u32::from(u16::from_be_bytes([*b.get(i + 7)?, *b.get(i + 8)?]));
                    return Some((w, h));
                }
                if len < 2 || matches!(m, 0xD8..=0xDA) {
                    return None;
                }
                i += 2 + len;
            }
        }
        "image/webp" => {
            let le24 = |i: usize| {
                b.get(i..i + 3)
                    .map(|x| u32::from(x[0]) | u32::from(x[1]) << 8 | u32::from(x[2]) << 16)
            };
            match b.get(12..16)? {
                b"VP8X" => Some((le24(24)? + 1, le24(27)? + 1)),
                b"VP8 " => Some((le16(26)? & 0x3FFF, le16(28)? & 0x3FFF)),
                b"VP8L" if *b.get(20)? == 0x2F => {
                    let v =
                        u32::from_le_bytes([*b.get(21)?, *b.get(22)?, *b.get(23)?, *b.get(24)?]);
                    Some(((v & 0x3FFF) + 1, ((v >> 14) & 0x3FFF) + 1))
                }
                _ => None,
            }
        }
        _ => None,
    }
}

struct Upload {
    id: String,
    mime: &'static str,
    size: usize,
    bytes: Vec<u8>,
    next: u32,
    at: Instant,
}

/// What one `image.put` carries.
pub struct Put<'a> {
    pub upload: Option<&'a str>,
    pub seq: u32,
    pub mime: Option<&'a str>,
    pub size: Option<u64>,
    pub data: &'a str,
    pub last: bool,
}

/// The images of this process: at most one upload open, and the finished ones waiting for a `send`.
#[derive(Default)]
pub struct Images {
    up: Option<Upload>,
    ready: Vec<(String, ImageAttachment, usize)>,
}

impl Images {
    /// Forgets everything (a restart ends the session the images were for).
    pub fn clear(&mut self) {
        *self = Self::default();
    }

    fn held(&self) -> usize {
        self.ready.iter().map(|r| r.2).sum()
    }

    pub fn put(&mut self, p: &Put, now: Instant) -> Result<Value, String> {
        if self
            .up
            .as_ref()
            .is_some_and(|u| now.duration_since(u.at) > UPLOAD_IDLE)
        {
            self.up = None;
        }
        let up = match (p.upload, self.up.take()) {
            (None, None) => self.begin(p, now)?,
            (None, Some(_)) => return Err("another image is being sent".into()),
            // any mistake in the middle of an upload ends it (it was already taken out)
            (Some(id), Some(u)) if u.id == id && u.next == p.seq && p.seq > 0 => u,
            (Some(_), _) => return Err("no such upload".into()),
        };
        self.add(up, p, now)
    }

    fn begin(&self, p: &Put, now: Instant) -> Result<Upload, String> {
        let mime = match p.mime {
            Some("image/png") => "image/png",
            Some("image/jpeg") => "image/jpeg",
            Some("image/gif") => "image/gif",
            Some("image/webp") => "image/webp",
            _ => return Err("not an image type that can be attached".into()),
        };
        let size = usize::try_from(p.size.ok_or("the size is missing")?).unwrap_or(usize::MAX);
        if p.seq != 0 || size == 0 || size > MAX_IMAGE {
            return Err(format!("an image must be 1 byte to {} MB", MAX_IMAGE >> 20));
        }
        if self.ready.len() >= MAX_IMAGES {
            return Err(format!("at most {MAX_IMAGES} images per message"));
        }
        if self.held() + size > MAX_IMAGE_BYTES {
            return Err(format!(
                "a message holds at most {} MB of images",
                MAX_IMAGE_BYTES >> 20
            ));
        }
        Ok(Upload {
            id: format!("img-{}", uuid::Uuid::new_v4().simple()),
            mime,
            size,
            bytes: Vec::with_capacity(size.min(1 << 20)),
            next: 0,
            at: now,
        })
    }

    fn add(&mut self, mut u: Upload, p: &Put, now: Instant) -> Result<Value, String> {
        if p.data.len() > MAX_CHUNK_B64 {
            return Err("the chunk is too long".into());
        }
        let raw = STANDARD
            .decode(p.data)
            .map_err(|_| "the chunk is not base64".to_string())?;
        if u.bytes.len() + raw.len() > u.size {
            return Err("more bytes than the declared size".into());
        }
        u.bytes.extend_from_slice(&raw);
        u.next += 1;
        u.at = now;
        if !p.last {
            let id = u.id.clone();
            self.up = Some(u);
            return Ok(json!({"upload": id}));
        }
        if u.bytes.len() != u.size {
            return Err("fewer bytes than the declared size".into());
        }
        if sniff(&u.bytes) != Some(u.mime) {
            return Err("the bytes are not the declared image type".into());
        }
        let (w, h) = dims(&u.bytes, u.mime).ok_or("the image header is truncated or malformed")?;
        if w == 0
            || h == 0
            || w > MAX_SIDE
            || h > MAX_SIDE
            || u64::from(w) * u64::from(h) > MAX_PIXELS
        {
            return Err(format!(
                "an image may be at most {MAX_SIDE} pixels a side and {} megapixels",
                MAX_PIXELS / 1_000_000
            ));
        }
        let b64 = STANDARD.encode(&u.bytes);
        let attach = ImageAttachment {
            media_type: u.mime.into(),
            data_base64: b64,
        };
        let out = json!({"id": u.id, "bytes": u.size});
        self.ready.push((u.id, attach, u.size));
        Ok(out)
    }

    /// The images `ids` names, for a `send` (nothing is removed yet: see [`Images::remove`]).
    pub fn peek(&self, ids: &[String]) -> Result<Vec<ImageAttachment>, String> {
        if ids.len() > MAX_IMAGES {
            return Err(format!("at most {MAX_IMAGES} images per message"));
        }
        ids.iter()
            .enumerate()
            .map(|(i, id)| {
                let dup = ids[..i].contains(id);
                self.ready
                    .iter()
                    .find(|r| r.0 == *id && !dup)
                    .map(|r| r.1.clone())
                    .ok_or_else(|| "no such image".to_string())
            })
            .collect()
    }

    /// Drops the images `ids` (sent, or removed by the person); unknown ids are ignored.
    pub fn remove(&mut self, ids: &[String]) {
        self.ready.retain(|r| !ids.contains(&r.0));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn png_of(w: u32, h: u32, n: usize) -> Vec<u8> {
        let mut v = b"\x89PNG\r\n\x1a\n\0\0\0\x0dIHDR".to_vec();
        v.extend(w.to_be_bytes());
        v.extend(h.to_be_bytes());
        v.resize(n.max(v.len()), 7);
        v
    }

    fn png(n: usize) -> Vec<u8> {
        png_of(10, 10, n)
    }

    #[test]
    fn image_dimensions_are_read_from_the_header_and_capped() {
        let mut im = Images::default();
        assert!(put_all(&mut im, "image/png", &png_of(8000, 5000, 100), 100).is_ok());
        for (w, h) in [(8001, 10), (10, 8001), (7000, 7000), (0, 5)] {
            let e = put_all(&mut im, "image/png", &png_of(w, h, 100), 100).unwrap_err();
            assert!(e.contains("pixels"), "{w}x{h}: {e}");
        }
        // truncated or malformed headers: PNG without IHDR, a short PNG, a GIF cut before its size, a JPEG without a frame
        let mut bad = png_of(10, 10, 100);
        bad[12..16].copy_from_slice(b"JUNK");
        for (mime, bytes) in [
            ("image/png", bad),
            ("image/png", png(12)[..12].to_vec()),
            ("image/gif", b"GIF89a\x01".to_vec()),
            ("image/jpeg", vec![0xFF, 0xD8, 0xFF, 0xE0, 0, 4, 0, 0]),
            ("image/webp", b"RIFF\0\0\0\0WEBPVP8 ".to_vec()),
        ] {
            let e = put_all(&mut im, mime, &bytes, 100).unwrap_err();
            assert!(e.contains("malformed"), "{mime}: {e}");
        }
        // the other formats' headers parse
        let gif: Vec<u8> = b"GIF89a".iter().copied().chain([20, 0, 30, 0]).collect();
        assert!(put_all(&mut im, "image/gif", &gif, 100).is_ok());
        let jpeg = vec![
            0xFF, 0xD8, 0xFF, 0xC0, 0, 11, 8, 0, 40, 0, 50, 3, 1, 0x22, 0,
        ];
        assert_eq!(dims(&jpeg, "image/jpeg"), Some((50, 40)));
        let mut webp = b"RIFF\0\0\0\0WEBPVP8X\0\0\0\0\0\0\0\0".to_vec();
        webp.extend([99, 0, 0, 49, 0, 0]);
        assert_eq!(dims(&webp, "image/webp"), Some((100, 50)));
    }

    fn put_all(im: &mut Images, mime: &str, bytes: &[u8], chunk: usize) -> Result<Value, String> {
        let mut id: Option<String> = None;
        let parts: Vec<&[u8]> = bytes.chunks(chunk).collect();
        let mut last = Ok(Value::Null);
        for (i, c) in parts.iter().enumerate() {
            let data = STANDARD.encode(c);
            last = im.put(
                &Put {
                    upload: id.as_deref(),
                    seq: i as u32,
                    mime: (i == 0).then_some(mime),
                    size: (i == 0).then_some(bytes.len() as u64),
                    data: &data,
                    last: i + 1 == parts.len(),
                },
                Instant::now(),
            );
            if let Ok(v) = &last {
                id = v["upload"].as_str().map(String::from);
            } else {
                break;
            }
        }
        last
    }

    #[test]
    fn relative_paths_only() {
        for bad in [
            "/etc/passwd",
            "../x",
            "a/../b",
            "./a",
            "a\nb",
            "a\0b",
            ".git/config",
            "a/.GIT/x",
            &"a/".repeat(600),
        ] {
            assert!(check_rel(bad, false).is_err(), "{bad:?}");
        }
        #[cfg(windows)]
        assert!(check_rel("C:\\x", false).is_err() && check_rel("\\\\h\\s", false).is_err());
        assert!(check_rel("", false).is_err() && check_rel("", true).is_ok());
        assert!(check_rel("src/a.rs", false).is_ok() && check_rel(".github/x.yml", false).is_ok());
    }

    #[test]
    fn the_tree_is_paged_and_a_folder_is_a_row_of_it() {
        let d = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(d.path().join("src/deep")).unwrap();
        for i in 0..250 {
            std::fs::write(d.path().join(format!("f{i:03}")), "x").unwrap();
        }
        std::fs::write(d.path().join("src/a.rs"), "x").unwrap();
        std::fs::write(d.path().join("src/deep/b.rs"), "x").unwrap();
        let env = Env::from_pairs::<&str, &str>([]);
        let p0 = list_of(&scan(d.path(), false, &env), "", 0, false).unwrap();
        assert_eq!(p0["rows"].as_array().unwrap().len(), PAGE_ROWS);
        assert_eq!(p0["next"], PAGE_ROWS);
        let p1 = list_of(&scan(d.path(), false, &env), "", PAGE_ROWS, false).unwrap();
        assert!(p1["next"].is_null() && p1["total"] == p0["total"]);
        let sub = list_of(&scan(d.path(), false, &env), "src", 0, false).unwrap();
        let names: Vec<String> = sub["rows"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r[1].as_str().unwrap().into())
            .collect();
        assert_eq!(names, ["a.rs", "deep", "b.rs"]);
        // a folder that is not in the tree, or a hostile one, is refused without touching the disk
        assert!(list_of(&scan(d.path(), false, &env), "nope", 0, false).is_err());
        assert!(list_of(&scan(d.path(), false, &env), "../..", 0, false).is_err());
        assert!(
            list_of(&scan(d.path(), false, &env), "f001", 0, false).is_err(),
            "a file is not a folder"
        );
        // git is never run for a folder that is not trusted
        assert!(
            git_diff(d.path(), false, &env).is_err() && git_status(d.path(), false, &env).is_err()
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_preview_refuses_what_must_not_be_shown() {
        let d = tempfile::tempdir().unwrap();
        let w = d.path().join("w");
        std::fs::create_dir_all(w.join("sub")).unwrap();
        std::fs::create_dir_all(w.join(".git")).unwrap();
        std::fs::write(w.join("a.txt"), "hello\nbye\x1b[31m\n").unwrap();
        std::fs::write(w.join(".env"), "K=1").unwrap();
        std::fs::write(w.join(".git/config"), "[core]").unwrap();
        std::fs::write(w.join("bin"), [0u8, 1, 2]).unwrap();
        std::fs::write(w.join("big.txt"), "x\n".repeat(PREVIEW_BYTES)).unwrap();
        std::fs::write(d.path().join("secret.txt"), "outside").unwrap();
        let ok = read(&w, "a.txt").unwrap();
        assert_eq!(ok["lines"][0], "hello");
        assert!(
            !ok["lines"].to_string().contains('\u{1b}'),
            "made printable"
        );
        for bad in [
            ".env",
            ".git/config",
            "bin",
            "../secret.txt",
            "/etc/hosts",
            "sub",
            "missing",
        ] {
            assert!(read(&w, bad).is_err(), "{bad}");
        }
        let big = read(&w, "big.txt").unwrap();
        assert_eq!(
            (big["cut"].clone(), big["lines"].as_array().unwrap().len()),
            (true.into(), PREVIEW_LINES)
        );
        // no error carries a path of this machine
        let e = read(&w, "missing").unwrap_err();
        assert!(!e.contains(&*d.path().to_string_lossy()), "{e}");
    }

    #[cfg(unix)]
    #[test]
    fn links_in_and_out_of_the_folder_are_not_followed() {
        let d = tempfile::tempdir().unwrap();
        let w = d.path().join("w");
        std::fs::create_dir_all(&w).unwrap();
        std::fs::write(d.path().join("outside.txt"), "secret").unwrap();
        std::fs::write(w.join("real.txt"), "in").unwrap();
        std::os::unix::fs::symlink(d.path().join("outside.txt"), w.join("out.txt")).unwrap();
        std::os::unix::fs::symlink("/etc", w.join("etc")).unwrap();
        std::fs::hard_link(d.path().join("outside.txt"), w.join("hard.txt")).unwrap();
        assert!(read(&w, "real.txt").is_ok());
        // a link INSIDE the project to `.git` internals or a secret file is refused by where it really points
        std::fs::create_dir_all(w.join(".git")).unwrap();
        std::fs::write(w.join(".git/config"), "[remote]").unwrap();
        std::fs::write(w.join(".env"), "K=1").unwrap();
        std::os::unix::fs::symlink(".git/config", w.join("cfg.txt")).unwrap();
        std::os::unix::fs::symlink(".env", w.join("e.txt")).unwrap();
        std::os::unix::fs::symlink("real.txt", w.join("ok.txt")).unwrap();
        assert!(read(&w, "cfg.txt").unwrap_err().contains(".git"));
        assert!(read(&w, "e.txt").unwrap_err().contains("secret"));
        assert!(
            read(&w, "ok.txt").is_ok(),
            "a harmless link inside the folder reads"
        );
        assert!(read(&w, "out.txt").unwrap_err().contains("outside"));
        assert!(read(&w, "etc/hosts").unwrap_err().contains("outside"));
        assert!(read(&w, "hard.txt").unwrap_err().contains("hard links"));
        // a pipe in the tree does not block the engine
        // (no unsafe code in this crate: the shell makes the pipe)
        let ok = std::process::Command::new("mkfifo")
            .arg(w.join("pipe"))
            .status()
            .is_ok_and(|s| s.success());
        assert!(ok);
        assert!(read(&w, "pipe").unwrap_err().contains("regular"));
    }

    #[cfg(unix)]
    #[test]
    fn a_file_or_a_parent_swapped_for_a_link_during_the_read_never_yields_git_or_secret_bytes() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering};
        let d = tempfile::tempdir().unwrap();
        let w = d.path().join("w");
        std::fs::create_dir_all(w.join(".git")).unwrap();
        std::fs::create_dir_all(w.join("d")).unwrap();
        std::fs::write(w.join(".git/config"), "SECRET-GIT").unwrap();
        std::fs::write(w.join(".env"), "SECRET-ENV").unwrap();
        std::fs::write(w.join("ok.txt"), "fine").unwrap();
        std::fs::write(w.join("d/config"), "fine-d").unwrap();
        std::fs::write(w.join("stable.txt"), "stable").unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let swapper = {
            let (w, stop) = (w.clone(), stop.clone());
            std::thread::spawn(move || {
                while !stop.load(Ordering::SeqCst) {
                    // the file becomes a link to .git/config (and back), the folder a link to .git (and back)
                    let _ = std::fs::remove_file(w.join("ok.txt"));
                    let _ = std::os::unix::fs::symlink(".git/config", w.join("ok.txt"));
                    let _ = std::fs::remove_file(w.join("ok.txt"));
                    let _ = std::fs::write(w.join("ok.txt"), "fine");
                    let _ = std::fs::rename(w.join("d"), w.join("d.real"));
                    let _ = std::os::unix::fs::symlink(".git", w.join("d"));
                    let _ = std::fs::remove_file(w.join("d"));
                    let _ = std::fs::rename(w.join("d.real"), w.join("d"));
                }
            })
        };
        let t0 = Instant::now();
        let (mut ok, mut refused, mut stable) = (0, 0, 0);
        while t0.elapsed() < Duration::from_millis(1500) {
            // a file nobody touches must always read: a reader that refuses everything must not pass
            assert_eq!(read(&w, "stable.txt").unwrap()["lines"][0], "stable");
            stable += 1;
            for rel in ["ok.txt", "d/config"] {
                match read(&w, rel) {
                    Ok(v) => {
                        assert!(!v.to_string().contains("SECRET"), "{rel}: {v}");
                        ok += 1;
                    }
                    Err(_) => refused += 1,
                }
            }
        }
        stop.store(true, Ordering::SeqCst);
        swapper.join().unwrap();
        assert!(ok + refused > 0 && stable > 0);
        // quiet phase: with the swapper stopped (and the files back as they were) the very same reads work
        let _ = std::fs::remove_file(w.join("ok.txt"));
        std::fs::write(w.join("ok.txt"), "fine").unwrap();
        if std::fs::symlink_metadata(w.join("d")).is_ok_and(|m| m.file_type().is_symlink()) {
            std::fs::remove_file(w.join("d")).unwrap();
        }
        if !w.join("d").exists() && w.join("d.real").exists() {
            std::fs::rename(w.join("d.real"), w.join("d")).unwrap();
        }
        assert_eq!(read(&w, "ok.txt").unwrap()["lines"][0], "fine");
        assert_eq!(read(&w, "d/config").unwrap()["lines"][0], "fine-d");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn only_filesystems_that_count_links_wrongly_are_refused() {
        for bad in [
            0x0102_1997u64,
            0xFF53_4D42,
            0xFE53_4D42,
            0x517B,
            0x6573_5546,
        ] {
            assert!(links_unreliable(bad), "{bad:#x}");
        }
        // ext4, xfs, btrfs, tmpfs, overlayfs, nfs
        for ok in [
            0xEF53u64,
            0x5846_5342,
            0x9123_683E,
            0x0102_1994,
            0x794C_7630,
            0x6969,
        ] {
            assert!(!links_unreliable(ok), "{ok:#x}");
        }
    }

    #[test]
    fn an_image_is_chunked_sniffed_and_named_by_the_engine() {
        let mut im = Images::default();
        let bytes = png(10_000);
        let r = put_all(&mut im, "image/png", &bytes, 3000).unwrap();
        let id = r["id"].as_str().unwrap().to_string();
        let got = im.peek(std::slice::from_ref(&id)).unwrap();
        assert_eq!(STANDARD.decode(&got[0].data_base64).unwrap(), bytes);
        assert_eq!(got[0].media_type, "image/png");
        assert!(im.peek(&[id.clone(), id.clone()]).is_err(), "twice is no");
        im.remove(std::slice::from_ref(&id));
        assert!(im.peek(&[id]).is_err());
    }

    #[test]
    fn hostile_images_are_refused_and_leave_nothing_open() {
        let mut im = Images::default();
        // the declared type must be what the bytes are
        assert!(put_all(&mut im, "image/png", b"GIF89a....", 100).is_err());
        assert!(put_all(&mut im, "image/svg+xml", &png(20), 100).is_err());
        assert!(put_all(&mut im, "text/html", b"<script>", 100).is_err());
        assert!(put_all(&mut im, "image/png", &png(MAX_IMAGE + 1), MAX_CHUNK_B64 / 2).is_err());
        // more than declared, fewer than declared, not base64, a chunk too long, a bad sequence
        let put = |im: &mut Images, p: Put| im.put(&p, Instant::now());
        fn first(data: &str, size: u64) -> Put<'_> {
            Put {
                upload: None,
                seq: 0,
                mime: Some("image/png"),
                size: Some(size),
                data,
                last: false,
            }
        }
        let b = STANDARD.encode(png(30));
        assert!(put(&mut im, first(&b, 10)).is_err());
        assert!(put(&mut im, first("***", 10)).is_err());
        assert!(
            put(
                &mut im,
                first(&"A".repeat(MAX_CHUNK_B64 + 4), MAX_IMAGE as u64)
            )
            .is_err()
        );
        let id = put(&mut im, first(&b, 90)).unwrap()["upload"]
            .as_str()
            .unwrap()
            .to_string();
        let more = |seq, last| Put {
            upload: Some(&id),
            seq,
            mime: None,
            size: None,
            data: &b,
            last,
        };
        assert!(
            put(&mut im, more(5, false)).is_err(),
            "a skipped chunk ends it"
        );
        assert!(
            put(&mut im, more(1, true)).is_err(),
            "and nothing of it stays"
        );
        // short upload
        let id = put(&mut im, first(&b, 90)).unwrap()["upload"]
            .as_str()
            .unwrap()
            .to_string();
        let end = Put {
            upload: Some(&id),
            seq: 1,
            mime: None,
            size: None,
            data: &b,
            last: true,
        };
        assert!(put(&mut im, end).unwrap_err().contains("fewer"));
        assert!(im.ready.is_empty());
        // an unknown id cannot be sent
        assert!(im.peek(&["img-x".into()]).is_err());
    }

    #[test]
    fn the_count_and_the_bytes_of_waiting_images_are_bounded() {
        let mut im = Images::default();
        for _ in 0..MAX_IMAGES {
            put_all(&mut im, "image/png", &png(100), 100).unwrap();
        }
        assert!(
            put_all(&mut im, "image/png", &png(100), 100)
                .unwrap_err()
                .contains("at most")
        );
        let mut im = Images::default();
        put_all(&mut im, "image/png", &png(MAX_IMAGE), 300_000).unwrap();
        assert!(
            put_all(&mut im, "image/png", &png(MAX_IMAGE), 300_000)
                .unwrap_err()
                .contains("MB")
        );
        // a second upload cannot start while one is open, and an idle one is dropped
        let mut im = Images::default();
        let b = STANDARD.encode(png(30));
        let p = Put {
            upload: None,
            seq: 0,
            mime: Some("image/png"),
            size: Some(90),
            data: &b,
            last: false,
        };
        let t0 = Instant::now();
        im.put(&p, t0).unwrap();
        assert!(im.put(&p, t0).unwrap_err().contains("another"));
        assert!(
            im.put(&p, t0 + UPLOAD_IDLE + Duration::from_secs(1))
                .is_ok()
        );
    }
}

#[cfg(all(test, not(unix)))]
mod windows_tests {
    #[test]
    fn an_engine_without_the_race_free_open_serves_no_previews() {
        assert!(super::read(std::path::Path::new("."), "a.txt").is_err());
    }
}
