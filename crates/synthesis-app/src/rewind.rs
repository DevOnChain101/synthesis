//! `synt rewind SESSION`: put the files the agent changed with its file tools back to their state
//! at the checkpoint taken before its first such write to each (Bash moves, renames and deletes are
//! not tracked). A dry run unless `--apply`.
//!
//! Safety: every path in a checkpoint is untrusted input. Only files inside the session's
//! workspace roots (the parent directory is resolved, so a link cannot lead out; roots that are a
//! filesystem root or the home directory are refused unless `--allow-wide-root`); no directory on
//! the path may be a symlink or junction; a snapshot that is not the file's exact bytes
//! (secret-masked) is never written back; a snapshot is written back only if it still hashes to
//! the sha256 recorded when it was stored; a file is removed only if the checkpoint recorded that
//! it did not exist (`pre_state: absent`). Checkpoints of older builds carry neither hash nor
//! proof: they are refused unless `--force` (the desktop never forces). A file that is a symlink
//! or not a regular file is never touched; a restore goes through a temp file in the same
//! directory and a rename. A file whose current state differs from the one the run left is refused
//! unless `--force`; the dry run lists those. Every path is checked again (inside, no links, same
//! state) right before its own write, and one rewind per session runs at a time (lock file).
//! On unix the write itself goes through a handle of the parent directory (walked from the root with
//! no link followed; temp, `renameat`, `unlinkat` relative to it; hard links refused; the state hash
//! is taken from the opened file), so a directory swapped for a link after the checks is refused.
//! A crash can leave a `.synt-rewind-<uuid>` temp file next to the target (removed on every error path, not on a kill).
//! shortcut: no merge, `--force` overwrites later edits; on Windows the write is still by path (a
//! junction swapped in between the last check and the rename is not caught; upgrade: handle-relative
//! NT calls); a concurrent writer to the file itself can still change it between the hash and the rename; the hashes live in the same store as the checkpoint rows, so whoever can write
//! `synt.db` can forge both: they detect damage and a changed artifact file, not a forged row.

use std::path::{Component, Path, PathBuf};

use sha2::{Digest, Sha256};
use synthesis_config::Env;
use synthesis_session::{Checkpoint, Reader, file_state_hash};

use crate::homes;

#[derive(clap::Args)]
pub struct RewindArgs {
    /// Session id (from `session.started` or the `synt: session ...` line).
    pub session: String,
    /// Only this checkpoint (default: every checkpoint of the session).
    #[arg(long, value_name = "NAME")]
    pub checkpoint: Option<String>,
    /// Write the files back (default: show what would happen). Protected paths (.git and hooks, .env*,
    /// shell startup files, CI and package-manager config) are never written or removed, not even with
    /// --force: the dry run lists them as skipped and you revert them by hand.
    #[arg(long)]
    pub apply: bool,
    /// Also overwrite or remove files changed since the run (or whose end state was not recorded),
    /// and act on checkpoints of older builds that carry no snapshot hash or creation proof.
    #[arg(long)]
    pub force: bool,
    /// Allow a session whose workspace root is a filesystem root or your home directory.
    #[arg(long)]
    pub allow_wide_root: bool,
}

// Test seam: runs between the checks and the write (`"checked"`) and right after the parent handle is open
// (`"opened"`), where a hostile process would swap a directory.
#[cfg(test)]
type Hook = std::cell::RefCell<Option<Box<dyn Fn(&str)>>>;
#[cfg(test)]
thread_local! {
    static HOOK: Hook = const { std::cell::RefCell::new(None) };
}
#[cfg(test)]
fn hook(stage: &str) {
    HOOK.with(|h| {
        if let Some(f) = h.borrow().as_ref() {
            f(stage);
        }
    });
}

/// Unix: the write goes through a handle of the parent directory walked from the workspace root with no link
/// followed (temp file, `renameat` and `unlinkat` relative to it), the protected names are checked again (and a
/// protected folder renamed onto a checked name is recognised by inode, on the walked folder and each one above it), and the file's state hash is taken from the opened file, so a directory swapped
/// for a link after the checks cannot redirect it. `bytes`: restore these; `None`: remove.
#[cfg(unix)]
fn write_back(l: &Loaded, e: &Entry, bytes: Option<&[u8]>) -> Result<(), String> {
    use rustix::fs::{AtFlags, Mode, OFlags, openat, renameat, unlinkat};
    use std::io::{Read, Write};
    use std::os::unix::fs::MetadataExt;
    let fail = |why: &str| Err(format!("{}: {why}", e.shown));
    let io = |x: std::io::Error| format!("{}: {x}", e.shown);
    let (Some(name), Some(parent)) = (e.path.file_name(), e.path.parent()) else {
        return fail("not a file path");
    };
    let parent = synthesis_core::canonicalize(parent).map_err(io)?;
    let Some((root, rel)) = l.roots.iter().find_map(|r| {
        let r = synthesis_core::canonicalize(r).ok()?;
        let rel = parent.strip_prefix(&r).ok()?.to_path_buf();
        Some((r, rel))
    }) else {
        return fail("outside the session's workspace");
    };
    // the strictest (case-insensitive) policy, on the names the handle walk will use
    if synthesis_permissions::is_protected_path(std::slice::from_ref(&root), &parent.join(name)) {
        return fail(&protected_reason());
    }
    // identities of the protected folders next to the path, taken before the walk: one renamed onto a checked
    // name afterwards (so the name check above no longer describes it) is recognised by inode
    let mut protected = vec![];
    let mut cur = root.clone();
    for c in std::iter::once(None).chain(rel.components().map(Some)) {
        cur.extend(c);
        for ent in std::fs::read_dir(&cur).into_iter().flatten().flatten() {
            let p = ent.path();
            if synthesis_permissions::is_protected_path(std::slice::from_ref(&root), &p)
                && let Ok(m) = std::fs::symlink_metadata(&p)
                && m.is_dir()
            {
                protected.push((m.dev(), m.ino()));
            }
        }
    }
    #[cfg(test)]
    hook("checked");
    let dir = crate::engine_fs::open_dir_beneath(&root, &rel).map_err(io)?;
    // the walked folder and every folder above it, through the handle
    let mut up = dir.try_clone().map_err(io)?;
    for _ in 0..=rel.components().count() {
        let m = std::fs::File::from(up.try_clone().map_err(io)?)
            .metadata()
            .map_err(io)?;
        if protected.contains(&(m.dev(), m.ino())) {
            return fail(&protected_reason());
        }
        up = openat(
            &up,
            "..",
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(|x| io(x.into()))?;
    }
    #[cfg(test)]
    hook("opened");
    let flags = OFlags::NOFOLLOW | OFlags::CLOEXEC;
    let state = match openat(
        &dir,
        name,
        OFlags::RDONLY | OFlags::NONBLOCK | OFlags::NOCTTY | flags,
        Mode::empty(),
    ) {
        Err(rustix::io::Errno::NOENT) => None,
        Err(x) => return Err(io(x.into())), // a symlink in the last place (ELOOP) lands here
        Ok(fd) => Some(std::fs::File::from(fd)),
    };
    let now = match &state {
        None => "absent".to_string(),
        Some(f) => {
            let m = f.metadata().map_err(io)?;
            if !m.is_file() {
                return fail("not a regular file");
            }
            if m.nlink() > 1 {
                return fail("a hard link: not touched");
            }
            let mut b = vec![];
            (&*f).read_to_end(&mut b).map_err(io)?;
            sha_hex(&b)
        }
    };
    if now != e.state {
        return fail("changed while the rewind was running: not touched");
    }
    let Some(bytes) = bytes else {
        return unlinkat(&dir, name, AtFlags::empty()).map_err(|x| io(x.into()));
    };
    let tmp = format!(".synt-rewind-{}", uuid::Uuid::new_v4());
    let r = (|| {
        // private from the first byte; the original's mode is applied before any content is written
        let fd = openat(
            &dir,
            &tmp,
            OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | flags,
            Mode::from_raw_mode(0o600),
        )?;
        let mut f = std::fs::File::from(fd);
        if let Some(o) = &state {
            f.set_permissions(o.metadata()?.permissions())?;
        }
        f.write_all(bytes)?;
        f.sync_all()?;
        renameat(&dir, &tmp, &dir, name).map_err(std::io::Error::from)
    })();
    if r.is_err() {
        let _ = unlinkat(&dir, &tmp, AtFlags::empty());
    }
    r.map_err(io)
}

/// Not unix: path-based (shortcut: a junction swapped in after `path_problem`'s check is not caught; there is
/// no portable `openat` on Windows, upgrade with handle-relative NT calls).
#[cfg(not(unix))]
fn write_back(_l: &Loaded, e: &Entry, bytes: Option<&[u8]>) -> Result<(), String> {
    match bytes {
        Some(b) => replace_file(&e.path, b),
        None => std::fs::remove_file(&e.path),
    }
    .map_err(|x| format!("{}: {x}", e.shown))
}

/// Writes `bytes` next to `path` and renames over it, keeping the mode of the file it replaces.
#[cfg(not(unix))]
fn replace_file(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    let tmp = dir.join(format!(".synt-rewind-{}", uuid::Uuid::new_v4()));
    let r = (|| {
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp)?;
        if let Ok(m) = std::fs::metadata(path) {
            f.set_permissions(m.permissions())?;
        }
        f.write_all(bytes)?;
        f.sync_all()?;
        std::fs::rename(&tmp, path)
    })();
    if r.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    r
}

pub(crate) fn inside(path: &Path, roots: &[PathBuf]) -> bool {
    if !path.is_absolute() || path.components().any(|c| c == Component::ParentDir) {
        return false;
    }
    let Some(parent) = path
        .parent()
        .and_then(|p| synthesis_core::canonicalize(p).ok())
    else {
        return false;
    };
    roots.iter().any(|r| {
        synthesis_core::canonicalize(r)
            .map(|r| parent.starts_with(r))
            .unwrap_or(false)
    })
}

/// A name Windows treats as a device (`CON`, `NUL.txt`, `COM1` ...) or an alternate data stream
/// (`a.txt:stream`). Checked on every OS so the rule is tested everywhere; applied on Windows.
pub(crate) fn windows_unsafe_name(name: &str) -> bool {
    if name.contains(':') {
        return true;
    }
    let stem = name
        .split('.')
        .next()
        .unwrap_or("")
        .trim_end()
        .to_ascii_uppercase();
    matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || ["COM", "LPT"].iter().any(|p| {
            stem.strip_prefix(p)
                .is_some_and(|n| n.len() == 1 && n.as_bytes()[0].is_ascii_digit() && n != "0")
        })
}

/// Why a protected file is skipped, and what to do about it: a rewind never writes or deletes what the tools
/// refuse or ask about first (the checkpoint may be older than a rule), so the person reverts it by hand.
fn protected_reason() -> String {
    format!(
        "a protected path: a rewind never writes it, revert it by hand (git checkout, or your editor). Protected: {}",
        synthesis_permissions::PROTECTED_SUMMARY
    )
}

/// A regular file with more than one name (pnpm stores, `cp -l` trees): replacing it would not change the other names.
fn hard_linked(p: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        std::fs::symlink_metadata(p).is_ok_and(|m| m.is_file() && m.nlink() > 1)
    }
    #[cfg(not(unix))]
    {
        let _ = p;
        false
    }
}

fn is_link(p: &Path) -> bool {
    let Ok(m) = std::fs::symlink_metadata(p) else {
        return false;
    };
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        if m.file_attributes() & 0x400 != 0 {
            return true; // FILE_ATTRIBUTE_REPARSE_POINT: symlink or junction
        }
    }
    m.file_type().is_symlink()
}

/// Why `path` may not be written, apart from `inside`: a directory between the workspace root and
/// the file is a symlink or junction, or the final name is one Windows reads as something else.
fn path_problem(path: &Path, roots: &[PathBuf]) -> Option<&'static str> {
    let name = path.file_name()?.to_string_lossy();
    if cfg!(windows) && windows_unsafe_name(&name) {
        return Some("a reserved Windows name or alternate data stream");
    }
    let roots: Vec<PathBuf> = roots
        .iter()
        .filter_map(|r| synthesis_core::canonicalize(r).ok())
        .collect();
    let mut cur = path.parent()?;
    loop {
        if synthesis_core::canonicalize(cur).is_ok_and(|c| roots.contains(&c)) {
            return None;
        }
        if is_link(cur) {
            return Some("a directory on its path is a symlink or junction");
        }
        cur = cur.parent()?;
    }
}

/// What a rewind does to one file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step {
    /// Write the snapshot (artifact) back; `sha256` is what it must hash to (None: forced, an
    /// older checkpoint without one).
    Restore {
        artifact_id: String,
        sha256: Option<String>,
    },
    /// The checkpoint recorded that the file did not exist: remove what the run created.
    Remove,
    /// Left alone; `forceable`: `--force` would act on it (the CLI says so).
    Skip { reason: String, forceable: bool },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub path: PathBuf,
    /// `path` safe to print.
    pub shown: String,
    pub step: Step,
    /// Restore: bytes to write. Remove: bytes of the file now. Skip: 0.
    pub size: u64,
    /// `file_state_hash` of the path when it was planned; it must still be that when it is written.
    pub state: String,
}

/// A session's checkpoints and what a rewind may write.
pub struct Loaded {
    pub reader: Reader,
    pub home: PathBuf,
    pub session: String,
    pub roots: Vec<PathBuf>,
    pub cps: Vec<Checkpoint>,
}

/// `user_home`: the home directory, which (like a filesystem root) is refused as a workspace root
/// unless `allow_wide`.
pub fn load(
    synthesis_home: &Path,
    user_home: Option<&Path>,
    session: &str,
    allow_wide: bool,
) -> Result<Loaded, String> {
    let reader = Reader::open(synthesis_home).map_err(|e| e.to_string())?;
    let rec = reader
        .get_session(session)
        .map_err(|_| format!("no saved session {session:?}"))?;
    let roots: Vec<PathBuf> = std::iter::once(rec.workspace_root.clone())
        .chain(rec.additional_dirs.clone())
        .map(PathBuf::from)
        .collect();
    if !allow_wide {
        // fails closed: without a home directory to compare with, the check cannot be made
        let user = user_home
            .filter(|h| !h.as_os_str().is_empty())
            .and_then(|h| synthesis_core::canonicalize(h).ok())
            .ok_or("the home directory is not known (HOME or USERPROFILE is unset or unreadable), so a workspace root that contains it cannot be ruled out: refused (synt rewind --allow-wide-root)")?;
        for r in &roots {
            let c = synthesis_core::canonicalize(r).unwrap_or_else(|_| r.clone());
            // a filesystem root, the home directory, or any directory that contains it (/Users, /home)
            if c.parent().is_none() || user.starts_with(&c) {
                return Err(format!(
                    "the session's workspace root {} is a filesystem root, your home directory or a directory that contains it: a rewind there could touch almost anything (synt rewind --allow-wide-root)",
                    synthesis_harness::sanitize_for_display(&r.to_string_lossy())
                ));
            }
        }
    }
    let cps = reader.checkpoints(session).map_err(|e| e.to_string())?;
    Ok(Loaded {
        reader,
        home: synthesis_home.to_path_buf(),
        session: session.to_string(),
        roots,
        cps,
    })
}

fn sha_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

/// Decides, for every file of the matching checkpoints (the earliest checkpoint of a file wins),
/// what a rewind would do now. Writes nothing.
pub fn plan(l: &Loaded, checkpoint: Option<&str>, force: bool) -> Result<Vec<Entry>, String> {
    let cps: Vec<_> = l
        .cps
        .iter()
        .filter(|c| checkpoint.is_none_or(|n| c.name == n))
        .collect();
    if cps.is_empty() {
        return Err("this session has no matching checkpoint (a run takes one before the first write to each file)".into());
    }
    let mut seen = std::collections::HashSet::new();
    let mut out = vec![];
    for f in cps.iter().flat_map(|c| &c.files) {
        if !seen.insert(f.path.clone()) {
            continue;
        }
        let path = PathBuf::from(&f.path);
        let shown = synthesis_harness::sanitize_for_display(&f.path);
        let now = file_state_hash(&path);
        let skip = |reason: &str, forceable: bool| Step::Skip {
            reason: reason.into(),
            forceable,
        };
        let mut size = 0;
        let step = if !inside(&path, &l.roots) {
            skip("outside the session's workspace", false)
        } else if let Some(why) = path_problem(&path, &l.roots) {
            skip(why, false)
        } else if hard_linked(&path) {
            skip(
                "a hard link (also reachable under another name): not touched",
                false,
            )
        } else if f.lossy_sha256.is_some() {
            skip(
                "the snapshot is not the exact file (secrets were masked)",
                false,
            )
        } else if synthesis_permissions::is_protected_path(&l.roots, &path) {
            skip(&protected_reason(), false)
        } else if std::fs::symlink_metadata(&path).is_ok_and(|m| !m.is_file()) {
            skip("a symlink or not a regular file", false)
        } else if !force
            && f.post_sha256.as_deref() != Some(&now)
            && !(f.artifact_id.is_none() && now == "absent")
        {
            skip(
                if f.post_sha256.is_none() {
                    "changed since the run or not recorded"
                } else {
                    "changed since the run"
                },
                true,
            )
        } else if let Some(id) = &f.artifact_id {
            match l.reader.artifact_bytes(&l.session, id) {
                Err(_) => skip("the snapshot cannot be read", false),
                Ok(b)
                    if f.artifact_sha256
                        .as_ref()
                        .is_some_and(|h| *h != sha_hex(&b)) =>
                {
                    skip(
                        "the snapshot file no longer matches the hash recorded when it was stored",
                        false,
                    )
                }
                Ok(_) if f.artifact_sha256.is_none() && !force => skip(
                    "the checkpoint has no snapshot hash (older build): the snapshot cannot be verified",
                    true,
                ),
                Ok(b) => {
                    size = b.len() as u64;
                    Step::Restore {
                        artifact_id: id.clone(),
                        sha256: f.artifact_sha256.clone(),
                    }
                }
            }
        } else if let Some(state) = f.pre_state.as_deref().filter(|s| *s != "absent") {
            // recorded as existing but no snapshot: the record contradicts a deletion, even forced
            skip(
                &format!(
                    "the checkpoint recorded the file as present ({state}) but holds no snapshot: it will not be touched"
                ),
                false,
            )
        } else if f.pre_state.is_none() && !force {
            skip(
                "no proof the run created it (the checkpoint did not record that the file was absent): it will not be touched",
                true,
            )
        } else {
            size = std::fs::metadata(&path).map_or(0, |m| m.len());
            Step::Remove
        };
        out.push(Entry {
            path,
            shown,
            step,
            size,
            state: now,
        });
    }
    Ok(out)
}

/// Carries out one entry of [`plan`] (a `Skip` does nothing). Everything is checked again first:
/// inside the roots, no link on the path, the file still in the state it was planned in, and the
/// snapshot still its recorded hash. `Err` is `path: reason`.
pub fn apply_entry(l: &Loaded, e: &Entry) -> Result<(), String> {
    let fail = |why: &str| Err(format!("{}: {why}", e.shown));
    match &e.step {
        Step::Skip { .. } => return Ok(()),
        _ if !inside(&e.path, &l.roots) => return fail("outside the session's workspace"),
        _ if path_problem(&e.path, &l.roots).is_some() => {
            return fail("a directory on its path is a symlink or junction");
        }
        _ if synthesis_permissions::is_protected_path(&l.roots, &e.path) => {
            return fail(&protected_reason());
        }
        _ if file_state_hash(&e.path) != e.state => {
            return fail("changed while the rewind was running: not touched");
        }
        _ => {}
    }
    #[cfg(all(test, not(unix)))]
    hook("checked");
    match &e.step {
        Step::Restore {
            artifact_id,
            sha256,
        } => {
            let bytes = l
                .reader
                .artifact_bytes(&l.session, artifact_id)
                .map_err(|x| format!("{}: {x}", e.shown))?;
            if sha256.as_ref().is_some_and(|h| *h != sha_hex(&bytes)) {
                return fail("the snapshot no longer matches its recorded hash: not written");
            }
            write_back(l, e, Some(&bytes))
        }
        Step::Remove if e.state == "absent" => Ok(()),
        _ => write_back(l, e, None),
    }
}

/// One rewind of a session at a time: the lock is held until the returned file is dropped.
pub fn lock(l: &Loaded) -> Result<std::fs::File, String> {
    let dir = l.home.join("sessions").join(&l.session);
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let path = dir.join(".rewind.lock");
    // never create or open through a link someone planted there
    if is_link(&path) {
        return Err("the session's .rewind.lock is a symlink: refusing to follow it".into());
    }
    let mut o = std::fs::OpenOptions::new();
    o.create(true).truncate(false).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        o.custom_flags(libc::O_NOFOLLOW).mode(0o600);
    }
    let f = o.open(&path).map_err(|e| e.to_string())?;
    f.try_lock()
        .map_err(|_| "another rewind of this session is running".to_string())?;
    Ok(f)
}

/// An entry and how carrying it out went.
pub type Applied = (Entry, Result<(), String>);

/// Applies a plan the person confirmed, under the session lock. The plan is made again first: if
/// it is not the one they saw (a file changed, a checkpoint was added) nothing is written. A
/// failure on one file does not stop the others; each result is returned with its entry.
pub fn apply_confirmed(
    l: &Loaded,
    checkpoint: Option<&str>,
    seen: &[Entry],
) -> Result<Vec<Applied>, String> {
    let _lock = lock(l)?;
    let now = plan(l, checkpoint, false)?;
    let key = |v: &[Entry]| {
        v.iter()
            .map(|e| (e.path.clone(), e.step.clone()))
            .collect::<Vec<_>>()
    };
    if key(&now) != key(seen) {
        return Err("the files or checkpoints changed since the preview: nothing was written; run /rewind again".into());
    }
    Ok(now
        .into_iter()
        .map(|e| {
            let res = apply_entry(l, &e);
            (e, res)
        })
        .collect())
}

pub fn run_cmd(env: &Env, a: RewindArgs) -> Result<u8, String> {
    let h = homes(env)?;
    let user = (!h.user.as_os_str().is_empty()).then_some(h.user.as_path());
    let l = load(&h.synthesis, user, &a.session, a.allow_wide_root)?;
    // the lock comes before the plan, so the plan is the state the writes will find
    let _lock = a.apply.then(|| lock(&l)).transpose()?;
    let entries = plan(&l, a.checkpoint.as_deref(), a.force)?;
    let (mut problems, mut done, mut failed) = (0, 0, 0);
    for e in &entries {
        let shown = &e.shown;
        let verb = |apply: &str, dry: &str| {
            if a.apply {
                apply.to_string()
            } else {
                dry.to_string()
            }
        };
        let (what, note) = match &e.step {
            Step::Skip { reason, forceable } => {
                problems += 1;
                out!(
                    "skip {shown}: {reason}{}",
                    if *forceable {
                        " (--force to override)"
                    } else {
                        ""
                    }
                );
                continue;
            }
            Step::Restore { .. } => (verb("restore", "would restore"), ""),
            Step::Remove => (
                verb("remove", "would remove"),
                " (did not exist before the run)",
            ),
        };
        // applied first, printed after: a line never says "restore" for a write that failed
        match a.apply.then(|| apply_entry(&l, e)) {
            Some(Err(why)) => {
                problems += 1;
                failed += 1;
                out!("FAILED {why}");
            }
            r => {
                done += usize::from(r.is_some());
                out!("{what} {shown}{note}");
            }
        }
    }
    if !a.apply {
        out!("dry run: pass --apply to do it");
    } else if failed > 0 {
        out!(
            "rewind {}: {done} written, {failed} failed{}",
            if done > 0 { "PARTIAL" } else { "FAILED" },
            if done > 0 {
                " (the other files are already written)"
            } else {
                ""
            }
        );
    }
    Ok(u8::from(problems > 0))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use synthesis_core::{PermissionMode, WorkspaceContext};
    use synthesis_events::Budget;
    use synthesis_session::{NewSession, Store, StoreOptions};

    struct Rig {
        _tmp: tempfile::TempDir,
        base: PathBuf,
        ws: PathBuf,
        _store: Store,
        w: synthesis_session::SessionWriter,
    }

    fn rig_in(sub: &str) -> Rig {
        let tmp = tempfile::tempdir().unwrap();
        let base = synthesis_core::canonicalize(tmp.path()).unwrap();
        let ws = if sub.is_empty() {
            base.join("ws")
        } else {
            base.join(sub)
        };
        fs::create_dir_all(&ws).unwrap();
        fs::create_dir_all(base.join("userhome")).unwrap();
        let store = Store::open(&base.join("home"), StoreOptions::default()).unwrap();
        let ctx = WorkspaceContext::open("w1", &ws, &ws, &[], base.join("art"), "pg").unwrap();
        let w = store
            .create_session(NewSession {
                id: Some("s1".into()),
                workspace: &ctx,
                model_config_id: "m".into(),
                permission_mode: PermissionMode::AcceptEdits,
                permission_policy_id: "default".into(),
                budget: Budget::default(),
                parent: None,
                title: None,
            })
            .unwrap();
        Rig {
            _tmp: tmp,
            base,
            ws,
            _store: store,
            w,
        }
    }

    fn rig() -> Rig {
        rig_in("")
    }

    impl Rig {
        /// The run changed `name` to `after` (checkpoint taken before, end state recorded).
        fn run_wrote(&mut self, name: &str, after: &str) -> PathBuf {
            let p = self.ws.join(name);
            self.w
                .create_checkpoint(&format!("cp-{name}"), std::slice::from_ref(&p))
                .unwrap();
            fs::write(&p, after).unwrap();
            self.w.record_post_run_hash(&p).unwrap();
            p
        }
        fn loaded(&self) -> Loaded {
            load(
                &self.base.join("home"),
                Some(&self.base.join("userhome")),
                "s1",
                false,
            )
            .unwrap()
        }
        fn plan(&self) -> Vec<Entry> {
            plan(&self.loaded(), None, false).unwrap()
        }
        /// The snapshot artifact files on disk.
        fn artifacts(&self) -> Vec<PathBuf> {
            fs::read_dir(self.base.join("home/sessions/s1/artifacts"))
                .unwrap()
                .map(|e| e.unwrap().path())
                .filter(|p| !p.file_name().unwrap().to_string_lossy().starts_with('.'))
                .collect()
        }
    }

    fn skip_reason(e: &Entry) -> &str {
        match &e.step {
            Step::Skip { reason, .. } => reason,
            s => panic!("{s:?}"),
        }
    }

    #[test]
    fn the_dry_run_lists_restores_and_removals_with_sizes_and_writes_nothing() {
        let mut g = rig();
        fs::write(g.ws.join("a.txt"), "old!").unwrap();
        let a = g.run_wrote("a.txt", "new content");
        let b = g.run_wrote("b.txt", "created");
        let v = g.plan();
        assert_eq!(v.len(), 2);
        assert!(matches!(v[0].step, Step::Restore { .. }) && v[0].path == a);
        assert_eq!(v[0].size, 4, "the bytes that would be written");
        assert!(matches!(v[1].step, Step::Remove) && v[1].path == b);
        assert_eq!(v[1].size, 7);
        assert_eq!(fs::read_to_string(&a).unwrap(), "new content");
        assert!(b.exists());
    }

    #[test]
    fn a_protected_path_in_a_checkpoint_is_never_restored_or_removed() {
        let mut g = rig();
        fs::create_dir_all(g.ws.join(".git/hooks")).unwrap();
        fs::write(g.ws.join(".git/hooks/pre-commit"), "old").unwrap();
        let hook = g.run_wrote(".git/hooks/pre-commit", "new");
        let created = g.run_wrote(".github_ok.txt", "x");
        let env = g.run_wrote(".env", "K=1");
        let v = g.plan();
        assert_eq!(v.len(), 3);
        for e in v.iter().filter(|e| e.path != created) {
            assert!(skip_reason(e).contains("protected"), "{}", e.shown);
        }
        assert!(
            matches!(v[1].step, Step::Remove),
            "an ordinary file still rewinds"
        );
        // even a forged entry that asks for the write is refused when it is applied
        let mut forged = v[0].clone();
        forged.step = Step::Restore {
            artifact_id: "x".into(),
            sha256: None,
        };
        let err = apply_entry(&g.loaded(), &forged).unwrap_err();
        assert!(err.contains("protected"), "{err}");
        apply_confirmed(&g.loaded(), None, &v).unwrap();
        assert_eq!(fs::read_to_string(hook).unwrap(), "new");
        assert_eq!(fs::read_to_string(env).unwrap(), "K=1");
        assert!(!created.exists());
    }

    #[test]
    fn nothing_outside_the_roots_or_through_links_or_traversal_is_touched() {
        let mut g = rig();
        let a = g.run_wrote("a.txt", "x");
        // the checkpoint's file is outside the roots this rewind may write in
        let other = g.base.join("other");
        fs::create_dir_all(&other).unwrap();
        let mut l = g.loaded();
        l.roots = vec![other];
        let v = plan(&l, None, false).unwrap();
        assert_eq!(skip_reason(&v[0]), "outside the session's workspace");
        // the file became a symlink out of the workspace
        let outside = g.base.join("secret.txt");
        fs::write(&outside, "keep").unwrap();
        fs::remove_file(&a).unwrap();
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(&outside, &a).unwrap();
            let v = g.plan();
            assert_eq!(skip_reason(&v[0]), "a symlink or not a regular file");
            // a directory of the path became a link out: the parent resolves outside
            let mut g = rig();
            fs::create_dir_all(g.ws.join("d")).unwrap();
            g.run_wrote("d/f.txt", "x");
            fs::remove_dir_all(g.ws.join("d")).unwrap();
            let out_dir = g.base.join("elsewhere");
            fs::create_dir_all(&out_dir).unwrap();
            fs::write(out_dir.join("f.txt"), "x").unwrap();
            std::os::unix::fs::symlink(&out_dir, g.ws.join("d")).unwrap();
            let v = g.plan();
            assert_eq!(skip_reason(&v[0]), "outside the session's workspace");
            let done = apply_confirmed(&g.loaded(), None, &v).unwrap();
            assert!(matches!(done[0].0.step, Step::Skip { .. }) && done[0].1.is_ok());
            assert_eq!(fs::read_to_string(out_dir.join("f.txt")).unwrap(), "x");
            // a link that stays inside the workspace is refused too: no link on the path
            let mut g = rig();
            fs::create_dir_all(g.ws.join("real")).unwrap();
            g.run_wrote("real/f.txt", "x");
            fs::rename(g.ws.join("real"), g.ws.join("moved")).unwrap();
            std::os::unix::fs::symlink(g.ws.join("moved"), g.ws.join("real")).unwrap();
            let v = g.plan();
            assert_eq!(
                skip_reason(&v[0]),
                "a directory on its path is a symlink or junction"
            );
        }
        assert_eq!(fs::read_to_string(&outside).unwrap(), "keep");
        // `..` and relative paths never count as inside
        let roots = [g.ws.clone()];
        assert!(!inside(&g.ws.join("../ws/a.txt"), &roots));
        assert!(!inside(&g.ws.join("x/../../other"), &roots));
        assert!(!inside(Path::new("a.txt"), &roots));
        assert!(inside(&g.ws.join("a.txt"), &roots));
    }

    #[test]
    fn windows_reserved_names_and_streams_are_recognised() {
        for bad in ["CON", "nul.txt", "Com1", "LPT9.log", "a.txt:stream", "aux "] {
            assert!(windows_unsafe_name(bad), "{bad}");
        }
        for ok in ["console.rs", "com10", "COM0", "a.txt", "nulled"] {
            assert!(!windows_unsafe_name(ok), "{ok}");
        }
    }

    #[test]
    fn a_secret_file_and_a_file_changed_since_the_run_are_skipped() {
        let mut g = rig();
        fs::write(g.ws.join(".env"), "KEY=1").unwrap();
        g.run_wrote(".env", "KEY=2");
        let a = g.run_wrote("a.txt", "x");
        fs::write(&a, "edited later by the person").unwrap();
        let v = g.plan();
        assert!(skip_reason(&v[0]).contains("secrets were masked"), "{v:?}");
        assert_eq!(skip_reason(&v[1]), "changed since the run");
    }

    #[test]
    fn a_confirmed_plan_writes_only_if_it_is_still_the_plan_that_was_shown() {
        let mut g = rig();
        fs::write(g.ws.join("a.txt"), "old").unwrap();
        let a = g.run_wrote("a.txt", "new");
        let b = g.run_wrote("b.txt", "made");
        let seen = g.plan();
        // the person edits b.txt after the preview: nothing is written, not even a.txt
        fs::write(&b, "changed after preview").unwrap();
        let e = apply_confirmed(&g.loaded(), None, &seen).unwrap_err();
        assert!(e.contains("nothing was written"), "{e}");
        assert_eq!(fs::read_to_string(&a).unwrap(), "new");
        // a fresh preview applies
        let seen = g.plan();
        assert!(matches!(seen[1].step, Step::Skip { .. }));
        let done = apply_confirmed(&g.loaded(), None, &seen).unwrap();
        assert!(done.iter().all(|(_, r)| r.is_ok()));
        assert_eq!(fs::read_to_string(&a).unwrap(), "old");
        assert!(b.exists(), "a skipped file stays");
        // a second apply sees a different plan (a.txt is back to its checkpoint state)
        assert!(apply_confirmed(&g.loaded(), None, &seen).is_err());
    }

    #[test]
    fn a_snapshot_that_no_longer_hashes_to_its_record_is_never_written_back() {
        let mut g = rig();
        fs::write(g.ws.join("a.txt"), "old").unwrap();
        let a = g.run_wrote("a.txt", "new");
        let before = g.plan();
        assert!(matches!(
            &before[0].step,
            Step::Restore {
                sha256: Some(_),
                ..
            }
        ));
        // the artifact file is replaced after the preview: the write itself refuses
        let err = {
            fs::write(&g.artifacts()[0], "planted").unwrap();
            apply_entry(&g.loaded(), &before[0]).unwrap_err()
        };
        assert!(err.contains("no longer matches"), "{err}");
        assert_eq!(fs::read_to_string(&a).unwrap(), "new");
        // and the next dry run says so instead of offering it
        assert!(skip_reason(&g.plan()[0]).contains("no longer matches the hash"));
    }

    #[test]
    fn a_file_is_removed_only_with_proof_the_run_created_it_and_old_checkpoints_need_force() {
        let mut g = rig();
        fs::write(g.ws.join("a.txt"), "old").unwrap();
        g.run_wrote("a.txt", "new");
        g.run_wrote("b.txt", "made");
        let mut l = g.loaded();
        // a checkpoint of an older build: no pre_state, no snapshot hash
        for f in l.cps.iter_mut().flat_map(|c| c.files.iter_mut()) {
            f.pre_state = None;
            f.artifact_sha256 = None;
        }
        let v = plan(&l, None, false).unwrap();
        assert!(skip_reason(&v[0]).contains("no snapshot hash"));
        assert!(skip_reason(&v[1]).contains("no proof the run created it"));
        assert!(matches!(
            &v[1].step,
            Step::Skip {
                forceable: true,
                ..
            }
        ));
        let v = plan(&l, None, true).unwrap();
        assert!(matches!(&v[0].step, Step::Restore { sha256: None, .. }));
        assert!(matches!(v[1].step, Step::Remove));
        // the desktop never forces: apply_confirmed plans with force off, so the preview it was
        // shown (forced) differs and nothing is written
        assert!(apply_confirmed(&l, None, &v).is_err());
        assert!(g.ws.join("b.txt").exists());
    }

    #[test]
    fn a_file_that_changed_after_planning_is_not_touched_by_its_own_write() {
        let mut g = rig();
        fs::write(g.ws.join("a.txt"), "old").unwrap();
        let a = g.run_wrote("a.txt", "new");
        let b = g.run_wrote("b.txt", "made");
        let v = g.plan();
        let l = g.loaded();
        fs::write(&a, "someone typed here").unwrap();
        fs::write(&b, "and here").unwrap();
        for e in &v {
            let r = apply_entry(&l, e);
            assert!(
                r.unwrap_err()
                    .contains("changed while the rewind was running")
            );
        }
        assert_eq!(fs::read_to_string(&a).unwrap(), "someone typed here");
        assert!(b.exists());
    }

    #[test]
    fn a_wide_workspace_root_is_refused_unless_allowed_and_an_unknown_home_fails_closed() {
        let g = rig_in("fakehome");
        let home = g.base.join("home");
        let sub = g.ws.join("me");
        fs::create_dir_all(&sub).unwrap();
        // the root is the home directory
        let err = load(&home, Some(&g.ws), "s1", false).err().unwrap();
        assert!(err.contains("filesystem root"), "{err}");
        // the root contains the home directory (like /Users or /home)
        let err = load(&home, Some(&sub), "s1", false).err().unwrap();
        assert!(err.contains("contains it"), "{err}");
        assert!(load(&home, Some(&sub), "s1", true).is_ok());
        // the usual case: the workspace is below the home directory
        assert!(load(&home, Some(&g.base), "s1", false).is_ok());
        let other = g.base.join("elsewhere");
        fs::create_dir_all(&other).unwrap();
        assert!(load(&home, Some(&other), "s1", false).is_ok());
        // no home known (only SYNT_HOME set gives an empty path): refused, not skipped
        for none in [None, Some(Path::new(""))] {
            let err = load(&home, none, "s1", false).err().unwrap();
            assert!(err.contains("home directory is not known"), "{err}");
        }
        assert!(load(&home, None, "s1", true).is_ok());
    }

    #[test]
    fn force_does_not_override_a_recorded_pre_state() {
        let mut g = rig();
        g.run_wrote("b.txt", "made");
        let mut l = g.loaded();
        let f = &mut l.cps[0].files[0];
        f.pre_state = Some("deadbeef".into());
        for force in [false, true] {
            let v = plan(&l, None, force).unwrap();
            assert!(
                matches!(
                    &v[0].step,
                    Step::Skip {
                        forceable: false,
                        ..
                    }
                ),
                "{v:?}"
            );
        }
        l.cps[0].files[0].pre_state = None;
        assert!(matches!(
            plan(&l, None, true).unwrap()[0].step,
            Step::Remove
        ));
    }

    #[cfg(unix)]
    #[test]
    fn a_planted_symlink_as_the_lock_file_is_not_followed() {
        let g = rig();
        let l = g.loaded();
        let target = g.base.join("victim");
        let lockfile = g.base.join("home/sessions/s1/.rewind.lock");
        std::os::unix::fs::symlink(&target, &lockfile).unwrap();
        assert!(lock(&l).unwrap_err().contains("symlink"));
        assert!(!target.exists(), "the dangling target was not created");
    }

    #[cfg(unix)]
    #[test]
    fn a_restored_file_keeps_a_private_mode() {
        use std::os::unix::fs::PermissionsExt;
        let mut g = rig();
        let p = g.ws.join("k.txt");
        fs::write(&p, "old").unwrap();
        fs::set_permissions(&p, fs::Permissions::from_mode(0o600)).unwrap();
        g.run_wrote("k.txt", "new");
        let l = g.loaded();
        let v = plan(&l, None, false).unwrap();
        apply_entry(&l, &v[0]).unwrap();
        assert_eq!(fs::read_to_string(&p).unwrap(), "old");
        assert_eq!(
            fs::metadata(&p).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    #[test]
    fn two_rewinds_of_one_session_cannot_apply_at_once() {
        let g = rig();
        let l = g.loaded();
        let first = lock(&l).unwrap();
        assert!(lock(&l).unwrap_err().contains("another rewind"));
        drop(first);
        assert!(lock(&l).is_ok());
    }

    #[cfg(unix)]
    fn set_hook(f: impl Fn(&str) + 'static) {
        HOOK.with(|h| *h.borrow_mut() = Some(Box::new(f)));
    }

    #[cfg(unix)]
    fn entry(g: &Rig, rel: &str, state: &str) -> Entry {
        Entry {
            path: g.ws.join(rel),
            shown: rel.into(),
            step: Step::Remove,
            size: 0,
            state: state.into(),
        }
    }

    /// `ws/d/f.txt` written by the run ("new"), the snapshot "old", and an outside folder holding a file
    /// with the same bytes ("new") that a redirected write would hit.
    #[cfg(unix)]
    fn swap_rig() -> (Rig, PathBuf) {
        let mut g = rig();
        fs::create_dir_all(g.ws.join("d")).unwrap();
        fs::write(g.ws.join("d/f.txt"), "old").unwrap();
        g.run_wrote("d/f.txt", "new");
        let out = g.base.join("ssh");
        fs::create_dir_all(&out).unwrap();
        fs::write(out.join("f.txt"), "new").unwrap();
        (g, out)
    }

    #[cfg(unix)]
    fn swap_d_for_link(ws: PathBuf, out: PathBuf) -> impl Fn(&str) {
        move |stage| {
            if stage == "checked" {
                fs::rename(ws.join("d"), ws.join("d_moved")).unwrap();
                std::os::unix::fs::symlink(&out, ws.join("d")).unwrap();
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_parent_swapped_for_a_link_between_the_checks_and_the_write_is_not_followed() {
        let (g, out) = swap_rig();
        let l = g.loaded();
        let v = plan(&l, None, false).unwrap();
        assert!(matches!(v[0].step, Step::Restore { .. }));
        set_hook(swap_d_for_link(g.ws.clone(), out.clone()));
        let r = apply_entry(&l, &v[0]);
        assert!(r.is_err(), "{r:?}");
        assert_eq!(fs::read_to_string(out.join("f.txt")).unwrap(), "new");
        let names: Vec<_> = fs::read_dir(&out).unwrap().collect();
        assert_eq!(
            names.len(),
            1,
            "no temp file in the folder the link leads to"
        );
        assert_eq!(
            fs::read_to_string(g.ws.join("d_moved/f.txt")).unwrap(),
            "new"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_removal_through_a_swapped_parent_deletes_nothing_outside() {
        let mut g = rig();
        fs::create_dir_all(g.ws.join("d")).unwrap();
        g.run_wrote("d/made.txt", "x");
        let out = g.base.join("ssh");
        fs::create_dir_all(&out).unwrap();
        fs::write(out.join("made.txt"), "x").unwrap();
        let l = g.loaded();
        let v = plan(&l, None, false).unwrap();
        assert!(matches!(v[0].step, Step::Remove));
        set_hook(swap_d_for_link(g.ws.clone(), out.clone()));
        assert!(apply_entry(&l, &v[0]).is_err());
        assert!(out.join("made.txt").exists());
        assert!(g.ws.join("d_moved/made.txt").exists());
    }

    #[cfg(unix)]
    #[test]
    fn a_parent_swapped_after_it_was_opened_still_writes_the_original_folder() {
        let (g, out) = swap_rig();
        let l = g.loaded();
        let v = plan(&l, None, false).unwrap();
        let (ws, o) = (g.ws.clone(), out.clone());
        set_hook(move |stage| {
            if stage == "opened" {
                fs::rename(ws.join("d"), ws.join("d_moved")).unwrap();
                std::os::unix::fs::symlink(&o, ws.join("d")).unwrap();
            }
        });
        apply_entry(&l, &v[0]).unwrap();
        assert_eq!(
            fs::read_to_string(g.ws.join("d_moved/f.txt")).unwrap(),
            "old"
        );
        assert_eq!(fs::read_to_string(out.join("f.txt")).unwrap(), "new");
        assert_eq!(fs::read_dir(&out).unwrap().count(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn a_folder_renamed_onto_git_after_the_checks_is_not_written() {
        let (g, _out) = swap_rig();
        let l = g.loaded();
        let v = plan(&l, None, false).unwrap();
        let ws = g.ws.clone();
        set_hook(move |stage| {
            if stage == "checked" {
                fs::rename(ws.join("d"), ws.join(".git")).unwrap();
            }
        });
        assert!(apply_entry(&l, &v[0]).is_err());
        assert_eq!(fs::read_to_string(g.ws.join(".git/f.txt")).unwrap(), "new");
    }

    #[cfg(unix)]
    #[test]
    fn protected_names_are_checked_again_on_the_walked_path_in_any_case() {
        let g = rig();
        let l = g.loaded();
        let h = sha_hex(b"x");
        for rel in [
            ".git/hooks/pre-commit",
            ".GIT/hooks/pre-commit",
            ".Husky/pre-commit",
            ".ENV",
        ] {
            let p = g.ws.join(rel);
            fs::create_dir_all(p.parent().unwrap()).unwrap();
            fs::write(&p, "x").unwrap();
            let e = entry(&g, rel, &h);
            assert!(
                write_back(&l, &e, None).unwrap_err().contains("protected"),
                "{rel}"
            );
            assert!(write_back(&l, &e, Some(b"y")).is_err(), "{rel}");
            assert_eq!(fs::read_to_string(&p).unwrap(), "x", "{rel}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_final_component_turned_into_a_link_or_a_hard_link_is_not_touched() {
        let mut g = rig();
        let a = g.run_wrote("a.txt", "new");
        let out = g.base.join("secret.txt");
        fs::write(&out, "new").unwrap();
        let l = g.loaded();
        let v = plan(&l, None, false).unwrap();
        let (ap, o) = (a.clone(), out.clone());
        set_hook(move |stage| {
            if stage == "checked" {
                fs::remove_file(&ap).unwrap();
                std::os::unix::fs::symlink(&o, &ap).unwrap();
            }
        });
        let r = apply_entry(&l, &v[0]);
        assert!(r.is_err(), "{r:?}");
        assert_eq!(fs::read_to_string(&out).unwrap(), "new");
        assert!(fs::symlink_metadata(&a).unwrap().file_type().is_symlink());
        // a hard link: the other name would keep the old inode, or the secret would be replaced
        let mut g = rig();
        let b = g.run_wrote("b.txt", "new");
        fs::hard_link(&b, g.ws.join("alias.txt")).unwrap();
        let l = g.loaded();
        assert!(skip_reason(&plan(&l, None, false).unwrap()[0]).contains("hard link"));
        // the write itself refuses it too (a link made after the plan)
        let e = entry(&g, "b.txt", &sha_hex(b"new"));
        assert!(
            write_back(&l, &e, Some(b"old"))
                .unwrap_err()
                .contains("hard link")
        );
        assert_eq!(fs::read_to_string(&b).unwrap(), "new");
        assert_eq!(fs::read_to_string(g.ws.join("alias.txt")).unwrap(), "new");
    }

    #[cfg(unix)]
    #[test]
    fn long_and_mixed_case_names_restore_normally() {
        let mut g = rig();
        let long = "n".repeat(255);
        fs::write(g.ws.join(&long), "old").unwrap();
        g.run_wrote(&long, "new");
        fs::write(g.ws.join("Mixed Ünï.TXT"), "old").unwrap();
        g.run_wrote("Mixed Ünï.TXT", "new");
        let l = g.loaded();
        for e in plan(&l, None, false).unwrap() {
            apply_entry(&l, &e).unwrap();
        }
        assert_eq!(fs::read_to_string(g.ws.join(&long)).unwrap(), "old");
        assert_eq!(
            fs::read_to_string(g.ws.join("Mixed Ünï.TXT")).unwrap(),
            "old"
        );
        assert_eq!(fs::read_dir(&g.ws).unwrap().count(), 2, "no temp left");
    }

    #[cfg(unix)]
    #[test]
    fn stress_a_parent_flipped_to_a_link_while_writing_never_reaches_outside() {
        use std::sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        };
        let (g, out) = swap_rig();
        let l = g.loaded();
        let stop = Arc::new(AtomicBool::new(false));
        let (ws, o, s) = (g.ws.clone(), out.clone(), stop.clone());
        let t = std::thread::spawn(move || {
            while !s.load(Ordering::Relaxed) {
                if fs::rename(ws.join("d"), ws.join("d2")).is_ok() {
                    let _ = std::os::unix::fs::symlink(&o, ws.join("d"));
                    let _ = fs::remove_file(ws.join("d"));
                    let _ = fs::rename(ws.join("d2"), ws.join("d"));
                }
            }
        });
        let (new, old) = (sha_hex(b"new"), sha_hex(b"old"));
        let e = entry(&g, "d/f.txt", &new);
        let back = entry(&g, "d/f.txt", &old);
        for _ in 0..400 {
            if write_back(&l, &e, Some(b"old")).is_ok() {
                while write_back(&l, &back, Some(b"new")).is_err() {}
            }
        }
        stop.store(true, Ordering::Relaxed);
        t.join().unwrap();
        assert_eq!(fs::read_to_string(out.join("f.txt")).unwrap(), "new");
        assert_eq!(fs::read_dir(&out).unwrap().count(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn a_protected_folder_renamed_onto_the_checked_name_is_recognised_by_inode() {
        let (g, _out) = swap_rig();
        fs::create_dir_all(g.ws.join(".git")).unwrap();
        fs::write(g.ws.join(".git/f.txt"), "new").unwrap();
        let l = g.loaded();
        let v = plan(&l, None, false).unwrap();
        let ws = g.ws.clone();
        set_hook(move |stage| {
            if stage == "checked" {
                fs::rename(ws.join("d"), ws.join("d_moved")).unwrap();
                fs::rename(ws.join(".git"), ws.join("d")).unwrap();
            }
        });
        let r = apply_entry(&l, &v[0]);
        assert!(r.unwrap_err().contains("protected"));
        assert_eq!(fs::read_to_string(g.ws.join("d/f.txt")).unwrap(), "new");
    }

    #[cfg(unix)]
    #[test]
    fn a_hard_linked_file_is_skipped_in_the_dry_run() {
        let mut g = rig();
        let b = g.run_wrote("b.txt", "new");
        fs::hard_link(&b, g.ws.join("alias.txt")).unwrap();
        assert!(skip_reason(&g.plan()[0]).contains("hard link"));
    }
}
