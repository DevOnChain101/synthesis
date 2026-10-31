//! `synth rewind SESSION`: put the files a run changed back to their state before the run's first
//! write to each (the checkpoints `synth run` takes). A dry run unless `--apply`.
//!
//! Safety: only files inside the session's workspace roots (the parent directory is resolved, so
//! a link cannot lead out); a snapshot that is not the file's exact bytes (secret-masked) is never
//! written back; a file that did not exist at checkpoint time is removed only with `--apply`.
//! A file that is a symlink or not a regular file is never touched; a restore goes through a temp
//! file in the same directory and a rename. A file whose current state differs from the one the
//! run left (hash recorded when its last write completed; none recorded = unknown) is refused
//! unless `--force`; the dry run lists those. A file that did not exist is removed only while it
//! is still what the run left.
//! shortcut: no merge, `--force` overwrites later edits; check-then-rename leaves a short race
//! window against a concurrent writer.

use std::path::{Component, Path, PathBuf};

use synthesis_config::Env;
use synthesis_session::{Reader, file_state_hash};

use crate::homes;

#[derive(clap::Args)]
pub struct RewindArgs {
    /// Session id (from `session.started` or the `synth: session ...` line).
    pub session: String,
    /// Only this checkpoint (default: every checkpoint of the session).
    #[arg(long, value_name = "NAME")]
    pub checkpoint: Option<String>,
    /// Write the files back (default: show what would happen).
    #[arg(long)]
    pub apply: bool,
    /// Also overwrite or remove files changed since the run (or whose end state was not recorded).
    #[arg(long)]
    pub force: bool,
}

/// Writes `bytes` next to `path` and renames over it, keeping the mode of the file it replaces.
fn replace_file(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    let tmp = dir.join(format!(".synth-rewind-{}", uuid::Uuid::new_v4()));
    let r = (|| {
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp)?;
        f.write_all(bytes)?;
        if let Ok(m) = std::fs::metadata(path) {
            f.set_permissions(m.permissions())?;
        }
        f.sync_all()?;
        std::fs::rename(&tmp, path)
    })();
    if r.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    r
}

fn inside(path: &Path, roots: &[PathBuf]) -> bool {
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

pub fn run_cmd(env: &Env, a: RewindArgs) -> Result<u8, String> {
    let h = homes(env)?;
    let r = Reader::open(&h.synthesis).map_err(|e| e.to_string())?;
    let rec = r
        .get_session(&a.session)
        .map_err(|_| format!("no saved session {:?}", a.session))?;
    let roots: Vec<PathBuf> = std::iter::once(rec.workspace_root.clone())
        .chain(rec.additional_dirs.clone())
        .map(PathBuf::from)
        .collect();
    let cps: Vec<_> = r
        .checkpoints(&a.session)
        .map_err(|e| e.to_string())?
        .into_iter()
        .filter(|c| a.checkpoint.as_ref().is_none_or(|n| &c.name == n))
        .collect();
    if cps.is_empty() {
        return Err("this session has no matching checkpoint (a run takes one before the first write to each file)".into());
    }
    let mut seen = std::collections::HashSet::new();
    let mut problems = 0;
    for f in cps.iter().flat_map(|c| &c.files) {
        if !seen.insert(f.path.clone()) {
            continue; // the earliest checkpoint of a file wins
        }
        let path = PathBuf::from(&f.path);
        let shown = synthesis_harness::sanitize_for_display(&f.path);
        let now = file_state_hash(&path);
        if !inside(&path, &roots) {
            problems += 1;
            out!("skip {shown}: outside the session's workspace");
        } else if f.lossy_sha256.is_some() {
            problems += 1;
            out!("skip {shown}: the snapshot is not the exact file (secrets were masked)");
        } else if std::fs::symlink_metadata(&path).is_ok_and(|m| !m.is_file()) {
            problems += 1;
            out!("skip {shown}: a symlink or not a regular file");
        } else if !a.force
            && f.post_sha256.as_deref() != Some(&now)
            && !(f.artifact_id.is_none() && now == "absent")
        {
            problems += 1;
            out!(
                "skip {shown}: changed since the run{} (--force to overwrite)",
                if f.post_sha256.is_none() {
                    " or not recorded"
                } else {
                    ""
                }
            );
        } else if let Some(id) = &f.artifact_id {
            out!(
                "{} {shown}",
                if a.apply { "restore" } else { "would restore" }
            );
            if a.apply {
                let bytes = r
                    .artifact_bytes(&a.session, id)
                    .map_err(|e| e.to_string())?;
                replace_file(&path, &bytes).map_err(|e| format!("{shown}: {e}"))?;
            }
        } else {
            out!(
                "{} {shown} (did not exist before the run)",
                if a.apply { "remove" } else { "would remove" }
            );
            if a.apply && now != "absent" {
                std::fs::remove_file(&path).map_err(|e| format!("{shown}: {e}"))?;
            }
        }
    }
    if !a.apply {
        out!("dry run: pass --apply to do it");
    }
    Ok(u8::from(problems > 0))
}
