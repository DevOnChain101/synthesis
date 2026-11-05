use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use cap_std::ambient_authority;
#[cfg(unix)]
use cap_std::fs::OpenOptionsExt;
use cap_std::fs::{Dir, Metadata, OpenOptions};
use serde_json::Value;
use synthesis_core::{ToolContext, contains};

use crate::ToolOutput;

/// Largest file the tools load into memory (Read/Edit/Grep).
pub(crate) const MAX_FILE_BYTES: u64 = 10 * 1024 * 1024;

/// Non-empty string field (Node treats "" like a missing value).
pub(crate) fn str_field<'a>(input: &'a Value, key: &str) -> Option<&'a str> {
    input.get(key)?.as_str().filter(|s| !s.is_empty())
}

/// `Number.isInteger` on a JSON number.
pub(crate) fn int_field(input: &Value, key: &str) -> Option<i64> {
    let f = input.get(key)?.as_f64()?;
    (f.is_finite() && f.fract() == 0.0).then_some(f as i64)
}

/// All tool paths go through `WorkspaceContext::resolve` (no implicit process cwd).
pub(crate) fn resolve(ctx: &ToolContext, tool: &str, p: &str) -> Result<PathBuf, String> {
    ctx.workspace.resolve(p).map_err(|e| format!("{tool}: {e}"))
}

/// File IO scoped to a workspace root through a directory handle (F12). `resolve` is a
/// check-then-use API; every open below goes through `cap-std`, which walks the path one
/// component at a time under the root handle and refuses any symlink (or `..`) that would
/// leave it, so a directory swapped for a symlink after `resolve` cannot redirect IO.
// shortcut: the root itself is opened by its stored realpath (ambient), so replacing the root's
// own ancestors after session creation is not covered; Glob/Grep directory walks use the
// `ignore` crate on plain paths (they do not follow links, but the walk base is not re-checked);
// upgrade by walking with `Dir::read_dir` if that matters.
pub(crate) struct Root {
    dir: Dir,
    skip: usize,
}

impl Root {
    /// Handle for whichever of cwd / additional dirs contains `abs` (a `resolve` result).
    pub(crate) fn open(ctx: &ToolContext, abs: &Path) -> io::Result<Self> {
        let ws = &ctx.workspace;
        let root = std::iter::once(&ws.cwd)
            .chain(&ws.additional_dirs)
            .find(|r| contains(r, abs, ws.path_policy))
            .ok_or_else(|| io::Error::new(io::ErrorKind::PermissionDenied, "outside workspace"))?;
        Ok(Self {
            dir: Dir::open_ambient_dir(root, ambient_authority())?,
            skip: root.components().count(),
        })
    }

    fn rel(&self, abs: &Path) -> PathBuf {
        let r: PathBuf = abs.components().skip(self.skip).collect();
        if r.as_os_str().is_empty() {
            ".".into()
        } else {
            r
        }
    }

    /// `stat` (follows links inside the root only).
    pub(crate) fn metadata(&self, abs: &Path) -> io::Result<Metadata> {
        self.dir.metadata(self.rel(abs))
    }

    fn open_file(&self, abs: &Path, write: bool) -> io::Result<cap_std::fs::File> {
        let mut o = OpenOptions::new();
        o.read(!write).write(write).create(write);
        // A FIFO/device must fail or open at once, never block the worker.
        #[cfg(unix)]
        o.custom_flags(libc::O_NONBLOCK);
        let f = self.dir.open_with(self.rel(abs), &o)?;
        let md = f.metadata()?;
        if !md.is_file() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "not a regular file",
            ));
        }
        // A hard link can point at a file outside the root and no path check sees it.
        // shortcut: Windows link counts are not checked (cap-std has no stable accessor), so
        // there a hard link is covered by the sandbox only; upgrade with GetFileInformationByHandle.
        #[cfg(unix)]
        if write {
            let n = cap_std::fs::MetadataExt::nlink(&md);
            if n > 1 {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    format!(
                        "{} has {n} hard links; it may be a path to a file outside the workspace; remove the link or edit the original",
                        abs.display()
                    ),
                ));
            }
        }
        Ok(f)
    }

    /// Reads a regular file; `FileTooLarge` when it exceeds `cap` (checked while reading, so a
    /// file that grows after `stat` still cannot exhaust memory).
    pub(crate) fn read_capped(&self, abs: &Path, cap: u64) -> io::Result<Vec<u8>> {
        let mut buf = Vec::new();
        self.open_file(abs, false)?
            .take(cap + 1)
            .read_to_end(&mut buf)?;
        if buf.len() as u64 > cap {
            return Err(io::Error::new(
                io::ErrorKind::FileTooLarge,
                format!("file exceeds the {} MiB read limit", cap >> 20),
            ));
        }
        Ok(buf)
    }

    /// Creates parent dirs, then writes a regular file (truncate happens after the type check).
    pub(crate) fn write(&self, abs: &Path, content: &str) -> io::Result<()> {
        if let Some(parent) = self.rel(abs).parent().filter(|p| !p.as_os_str().is_empty()) {
            self.dir.create_dir_all(parent)?;
        }
        let mut f = self.open_file(abs, true)?;
        f.set_len(0)?;
        f.write_all(content.as_bytes())
    }
}

/// Root handle or the tool's error output.
pub(crate) fn open_root(ctx: &ToolContext, tool: &str, abs: &Path) -> Result<Root, ToolOutput> {
    Root::open(ctx, abs).map_err(|e| {
        ToolOutput::error(format!(
            "{tool}: cannot access {}: {}",
            abs.display(),
            io_msg(&e, abs)
        ))
    })
}

pub(crate) fn mtime(md: &Metadata) -> io::Result<SystemTime> {
    md.modified().map(cap_std::time::SystemTime::into_std)
}

/// Node-style text for the codes the model sees most (soft-error patterns key on `ENOENT`).
pub(crate) fn io_msg(e: &io::Error, abs: &Path) -> String {
    match e.kind() {
        io::ErrorKind::NotFound => {
            format!(
                "ENOENT: no such file or directory, stat '{}'",
                abs.display()
            )
        }
        io::ErrorKind::PermissionDenied => {
            format!("EACCES: permission denied, stat '{}'", abs.display())
        }
        _ => e.to_string(),
    }
}

pub(crate) fn ledger_get(ctx: &ToolContext, abs: &Path) -> Option<SystemTime> {
    let l = ctx.read_ledger.lock().unwrap_or_else(|p| p.into_inner());
    l.get(abs).copied()
}

pub(crate) fn ledger_set(ctx: &ToolContext, abs: &Path, mtime: io::Result<SystemTime>) {
    if let Ok(t) = mtime {
        let mut l = ctx.read_ledger.lock().unwrap_or_else(|p| p.into_inner());
        l.insert(abs.to_path_buf(), t);
    }
}

/// True when the ledger holds exactly this mtime (Read happened and nothing changed since).
pub(crate) fn is_fresh(ctx: &ToolContext, abs: &Path, mtime: io::Result<SystemTime>) -> bool {
    matches!((ledger_get(ctx, abs), mtime), (Some(k), Ok(m)) if k == m)
}

pub(crate) fn is_binary(buf: &[u8]) -> bool {
    buf[..buf.len().min(8000)].contains(&0)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use serde_json::json;
    use std::collections::{BTreeMap, HashMap};
    use std::fs;
    use std::os::unix::fs::symlink;
    use std::sync::{Arc, Mutex};
    use synthesis_core::{PermissionMode, WorkspaceContext};

    fn ctx(root: &Path) -> ToolContext {
        let ws = WorkspaceContext::open("w", root, root, &[], root.join("art"), "pg").unwrap();
        ToolContext {
            session_id: "s".into(),
            task_id: "t".into(),
            turn_id: "u".into(),
            tool_call_id: "c".into(),
            workspace: Arc::new(ws),
            env: Arc::new(BTreeMap::new()),
            read_ledger: Arc::new(Mutex::new(HashMap::new())),
            permission_mode: PermissionMode::Default,
            skip_secrets: false,
            sub_agent: false,
            approval_token: None,
            deadline: None,
            cancel: tokio_util::sync::CancellationToken::new(),
        }
    }

    /// base/ws (workspace, with ws/sub/), base/outside/
    fn fx() -> (tempfile::TempDir, PathBuf, ToolContext) {
        let d = tempfile::tempdir().unwrap();
        let base = fs::canonicalize(d.path()).unwrap();
        fs::create_dir_all(base.join("ws/sub")).unwrap();
        fs::create_dir_all(base.join("outside")).unwrap();
        fs::write(base.join("ws/sub/f.txt"), "inside").unwrap();
        fs::write(base.join("outside/f.txt"), "SECRET").unwrap();
        let c = ctx(&base.join("ws"));
        (d, base, c)
    }

    /// Swaps `ws/sub` for a symlink to `outside`: the state an attacker leaves between
    /// `resolve` (which passed) and the actual IO.
    fn swap_sub_for_link(base: &Path) {
        fs::rename(base.join("ws/sub"), base.join("ws/sub.orig")).unwrap();
        symlink(base.join("outside"), base.join("ws/sub")).unwrap();
    }

    #[test]
    fn write_after_resolve_cannot_follow_swapped_symlink() {
        let (_d, base, c) = fx();
        let abs = c.workspace.resolve("sub/new/deep.txt").unwrap();
        swap_sub_for_link(&base);
        // Control: the old resolve-then-std::fs flow escapes (this is the bug being fixed).
        fs::create_dir_all(abs.parent().unwrap()).unwrap();
        fs::write(&abs, "pwn").unwrap();
        assert!(base.join("outside/new/deep.txt").exists());
        fs::remove_dir_all(base.join("outside/new")).unwrap();

        let root = Root::open(&c, &abs).unwrap();
        assert!(root.write(&abs, "pwn").is_err());
        assert!(!base.join("outside/new").exists());
    }

    #[test]
    fn read_after_resolve_cannot_follow_swapped_symlink() {
        let (_d, base, c) = fx();
        let abs = c.workspace.resolve("sub/f.txt").unwrap();
        swap_sub_for_link(&base);
        assert_eq!(fs::read(&abs).unwrap(), b"SECRET"); // control
        let root = Root::open(&c, &abs).unwrap();
        assert!(root.read_capped(&abs, MAX_FILE_BYTES).is_err());
        assert!(root.metadata(&abs).is_err());
    }

    #[test]
    fn final_component_symlink_to_outside_is_refused() {
        let (_d, base, c) = fx();
        let abs = c.workspace.resolve("link.txt").unwrap(); // does not exist yet
        symlink(base.join("outside/f.txt"), base.join("ws/link.txt")).unwrap();
        let root = Root::open(&c, &abs).unwrap();
        assert!(root.write(&abs, "pwn").is_err());
        assert_eq!(fs::read(base.join("outside/f.txt")).unwrap(), b"SECRET");
    }

    #[test]
    fn read_rejects_fifo_without_blocking_and_caps_size() {
        let (_d, base, c) = fx();
        let fifo = base.join("ws/pipe");
        assert!(
            std::process::Command::new("mkfifo")
                .arg(&fifo)
                .status()
                .unwrap()
                .success()
        );
        let root = Root::open(&c, &fifo).unwrap();
        let e = root.read_capped(&fifo, MAX_FILE_BYTES).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::InvalidInput);
        assert!(root.write(&fifo, "x").is_err());

        let big = base.join("ws/big.bin");
        fs::write(&big, vec![b'a'; 1025]).unwrap();
        let e = root.read_capped(&big, 1024).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::FileTooLarge);
        assert_eq!(root.read_capped(&big, 1025).unwrap().len(), 1025);
    }

    #[test]
    fn write_refuses_a_hard_linked_file() {
        let (_d, base, c) = fx();
        fs::hard_link(base.join("outside/f.txt"), base.join("ws/hl.txt")).unwrap();
        fs::write(base.join("ws/plain.txt"), "a").unwrap();
        let abs = c.workspace.resolve("hl.txt").unwrap();
        let root = Root::open(&c, &abs).unwrap();
        let e = root.write(&abs, "HARD").unwrap_err();
        assert!(e.to_string().contains("2 hard links"), "{e}");
        assert_eq!(fs::read(base.join("outside/f.txt")).unwrap(), b"SECRET");
        let plain = c.workspace.resolve("plain.txt").unwrap();
        root.write(&plain, "b").unwrap();
    }

    fn out(o: &crate::ToolOutput) -> String {
        format!("{:?}", o.content)
    }

    #[test]
    fn directory_walks_skip_secret_files_unless_the_call_names_them() {
        let (_d, base, c) = fx();
        fs::write(base.join("ws/.env"), "TOKEN=hunter2").unwrap();
        fs::write(base.join("ws/key.pem"), "hunter2").unwrap();
        fs::write(base.join("ws/ok.txt"), "hunter2").unwrap();
        let g = |v: Value| out(&crate::grep::run(&v, &c));
        let found = g(json!({"pattern": "hunter2", "output_mode": "files_with_matches"}));
        assert!(
            found.contains("ok.txt") && !found.contains(".env") && !found.contains("key.pem"),
            "{found}"
        );
        let by_path = g(json!({"pattern": "hunter2", "path": ".", "glob": "*"}));
        assert!(!by_path.contains(".env"), "{by_path}");
        // naming it with `glob` is what the permission layer asks about; then it is searched
        let named =
            g(json!({"pattern": "hunter2", "glob": ".env", "output_mode": "files_with_matches"}));
        assert!(named.contains(".env"), "{named}");
        let list = out(&crate::glob::run(&json!({"pattern": "**/*"}), &c));
        assert!(
            list.contains("ok.txt") && !list.contains("key.pem"),
            "{list}"
        );
        let list = out(&crate::glob::run(&json!({"pattern": "*.pem"}), &c));
        assert!(list.contains("key.pem"), "{list}");
    }
}
