//! Checkpoint file restore: rewinds only files the agent changed, never user edits.

use std::collections::BTreeMap;
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};
use synthesis_core::WorkspaceContext;
use synthesis_session::Checkpoint;

use crate::{Error, Result};

/// path (as stored in the checkpoint) -> sha256 hex of what the AGENT last wrote there
/// (`None`: the agent left it absent). The harness keeps this from its write/edit tools.
pub type Ledger = BTreeMap<String, Option<String>>;

pub fn hash_bytes(b: &[u8]) -> String {
    Sha256::digest(b)
        .iter()
        .map(|x| format!("{x:02x}"))
        .collect()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    /// The file differs from what the agent last wrote: the user (or something else) edited it.
    UserModified,
    /// No ledger entry, so agent vs user edits cannot be told apart.
    UnknownAgentState,
    /// Outside the worktree, a symlink, or not a regular file.
    UnsafePath(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileAction {
    Unchanged,
    /// Write the snapshot (artifact id) back.
    Write(String),
    /// The file did not exist at checkpoint time.
    Delete,
    Refuse(Refusal),
    /// The stored artifact is redacted or a secret-file placeholder, not the file's bytes:
    /// never written back.
    RefusedLossySnapshot {
        path: String,
        reason: String,
    },
}

/// What the session crate must rewind itself (events and messages).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SessionRewind {
    pub event_seq: u64,
    pub message_count: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RestorePlan {
    pub files: Vec<(PathBuf, FileAction)>,
    pub rewind: SessionRewind,
}

impl RestorePlan {
    pub fn refusals(&self) -> Vec<&(PathBuf, FileAction)> {
        self.files
            .iter()
            .filter(|(_, a)| matches!(a, FileAction::Refuse(_)))
            .collect()
    }
}

#[derive(Debug)]
pub struct RestoreOutcome {
    pub plan: RestorePlan,
    /// False for dry runs and whenever any file was refused (all-or-nothing).
    pub applied: bool,
}

fn current_hash(p: &Path) -> std::result::Result<Option<String>, Refusal> {
    match fs::symlink_metadata(p) {
        Ok(m) if m.is_file() => fs::read(p)
            .map(|b| Some(hash_bytes(&b)))
            .map_err(|e| Refusal::UnsafePath(e.to_string())),
        Ok(_) => Err(Refusal::UnsafePath("not a regular file".into())),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(None),
        Err(e) => Err(Refusal::UnsafePath(e.to_string())),
    }
}

/// `fetch(artifact_id)` returns snapshot bytes, e.g. `|id| reader.artifact_bytes(sid, id)`.
/// With `dry_run` (or any refusal) nothing is written. Lossy snapshots (`lossy_sha256` set) are
/// never written: they block the restore unless `skip_lossy`, which restores the rest. Messages / events are not touched.
pub fn restore_files(
    ws: &WorkspaceContext,
    cp: &Checkpoint,
    ledger: &Ledger,
    fetch: impl Fn(&str) -> synthesis_session::Result<Vec<u8>>,
    dry_run: bool,
    skip_lossy: bool,
) -> Result<RestoreOutcome> {
    let mut files = Vec::new();
    let mut bytes: BTreeMap<PathBuf, Vec<u8>> = BTreeMap::new();
    for snap in &cp.files {
        let path = match ws.resolve(&snap.path) {
            Ok(p) => p,
            Err(e) => {
                files.push((
                    PathBuf::from(&snap.path),
                    FileAction::Refuse(Refusal::UnsafePath(e.to_string())),
                ));
                continue;
            }
        };
        if snap.lossy_sha256.is_some() {
            files.push((
                path,
                FileAction::RefusedLossySnapshot {
                    path: snap.path.clone(),
                    reason: "snapshot is redacted or withheld; restoring it would destroy the real content"
                        .into(),
                },
            ));
            continue;
        }
        let action = (|| {
            let Some(expected) = ledger.get(&snap.path) else {
                return Ok(FileAction::Refuse(Refusal::UnknownAgentState));
            };
            let now = match current_hash(&path) {
                Ok(h) => h,
                Err(r) => return Ok(FileAction::Refuse(r)),
            };
            if &now != expected {
                return Ok(FileAction::Refuse(Refusal::UserModified));
            }
            Ok::<_, Error>(match &snap.artifact_id {
                None if now.is_none() => FileAction::Unchanged,
                None => FileAction::Delete,
                Some(id) => {
                    let b = fetch(id)?;
                    if now.as_deref() == Some(hash_bytes(&b).as_str()) {
                        FileAction::Unchanged
                    } else {
                        bytes.insert(path.clone(), b);
                        FileAction::Write(id.clone())
                    }
                }
            })
        })()?;
        files.push((path, action));
    }
    let plan = RestorePlan {
        files,
        rewind: SessionRewind {
            event_seq: cp.event_seq,
            message_count: cp.message_count,
        },
    };
    let blocked_by_lossy = !skip_lossy
        && plan
            .files
            .iter()
            .any(|(_, a)| matches!(a, FileAction::RefusedLossySnapshot { .. }));
    if dry_run || blocked_by_lossy || !plan.refusals().is_empty() {
        return Ok(RestoreOutcome {
            plan,
            applied: false,
        });
    }
    for ((path, action), snap) in plan.files.iter().zip(&cp.files) {
        if !matches!(action, FileAction::Write(_) | FileAction::Delete) {
            continue;
        }
        // Re-check right before touching the file (narrows the race with a concurrent edit).
        if current_hash(path).ok().as_ref() != Some(&ledger[&snap.path]) {
            return Err(Error::Refused(format!(
                "{} changed during restore",
                path.display()
            )));
        }
        match action {
            FileAction::Write(_) => {
                if let Some(parent) = path.parent() {
                    fs::create_dir_all(parent)?;
                }
                let tmp = path.with_file_name(format!(
                    ".{}.synt-restore",
                    path.file_name()
                        .map(|n| n.to_string_lossy())
                        .unwrap_or_default()
                ));
                fs::write(&tmp, &bytes[path])?;
                fs::rename(&tmp, path)?;
            }
            FileAction::Delete => fs::remove_file(path)?,
            _ => {}
        }
    }
    Ok(RestoreOutcome {
        plan,
        applied: true,
    })
}
