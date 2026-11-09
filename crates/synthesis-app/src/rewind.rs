//! `synt rewind SESSION`: put the files a run changed back to their state before the run's first
//! write to each (the checkpoints `synt run` takes). A dry run unless `--apply`.
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
use synthesis_session::{Checkpoint, Reader, file_state_hash};

use crate::homes;

#[derive(clap::Args)]
pub struct RewindArgs {
    /// Session id (from `session.started` or the `synt: session ...` line).
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
    let tmp = dir.join(format!(".synt-rewind-{}", uuid::Uuid::new_v4()));
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

/// What a rewind does to one file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step {
    /// Write the snapshot (artifact) back.
    Restore { artifact_id: String },
    /// The file did not exist at checkpoint time: remove what the run created.
    Remove,
    /// Left alone; `forceable`: `--force` would overwrite it (the CLI says so).
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
}

/// The session's checkpoints and the workspace roots a rewind may write in.
pub fn load(
    synthesis_home: &Path,
    session: &str,
) -> Result<(Reader, Vec<PathBuf>, Vec<Checkpoint>), String> {
    let r = Reader::open(synthesis_home).map_err(|e| e.to_string())?;
    let rec = r
        .get_session(session)
        .map_err(|_| format!("no saved session {session:?}"))?;
    let roots = std::iter::once(rec.workspace_root.clone())
        .chain(rec.additional_dirs.clone())
        .map(PathBuf::from)
        .collect();
    let cps = r.checkpoints(session).map_err(|e| e.to_string())?;
    Ok((r, roots, cps))
}

/// Decides, for every file of the matching checkpoints (the earliest checkpoint of a file wins),
/// what a rewind would do now. Writes nothing.
pub fn plan(
    r: &Reader,
    session: &str,
    cps: &[Checkpoint],
    roots: &[PathBuf],
    checkpoint: Option<&str>,
    force: bool,
) -> Result<Vec<Entry>, String> {
    let cps: Vec<_> = cps
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
        let step = if !inside(&path, roots) {
            skip("outside the session's workspace", false)
        } else if f.lossy_sha256.is_some() {
            skip(
                "the snapshot is not the exact file (secrets were masked)",
                false,
            )
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
            // an unreadable snapshot shows size 0 here; applying says why
            size = r.artifact_bytes(session, id).map_or(0, |b| b.len() as u64);
            Step::Restore {
                artifact_id: id.clone(),
            }
        } else {
            size = std::fs::metadata(&path).map_or(0, |m| m.len());
            Step::Remove
        };
        out.push(Entry {
            path,
            shown,
            step,
            size,
        });
    }
    Ok(out)
}

/// Carries out one entry of [`plan`] (a `Skip` does nothing). `Err` is `path: reason`.
pub fn apply_entry(r: &Reader, session: &str, e: &Entry) -> Result<(), String> {
    match &e.step {
        Step::Skip { .. } => Ok(()),
        Step::Restore { artifact_id } => {
            let bytes = r
                .artifact_bytes(session, artifact_id)
                .map_err(|x| format!("{}: {x}", e.shown))?;
            replace_file(&e.path, &bytes).map_err(|x| format!("{}: {x}", e.shown))
        }
        Step::Remove if file_state_hash(&e.path) == "absent" => Ok(()),
        Step::Remove => std::fs::remove_file(&e.path).map_err(|x| format!("{}: {x}", e.shown)),
    }
}

/// An entry and how carrying it out went.
pub type Applied = (Entry, Result<(), String>);

/// Applies a plan the person confirmed. The plan is made again first: if it is not the one they saw
/// (a file changed, a checkpoint was added) nothing is written. A failure on one file does not stop
/// the others; each result is returned with its entry.
pub fn apply_confirmed(
    r: &Reader,
    session: &str,
    cps: &[Checkpoint],
    roots: &[PathBuf],
    checkpoint: Option<&str>,
    seen: &[Entry],
) -> Result<Vec<Applied>, String> {
    let now = plan(r, session, cps, roots, checkpoint, false)?;
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
            let res = apply_entry(r, session, &e);
            (e, res)
        })
        .collect())
}

pub fn run_cmd(env: &Env, a: RewindArgs) -> Result<u8, String> {
    let (r, roots, cps) = load(&homes(env)?.synthesis, &a.session)?;
    let entries = plan(
        &r,
        &a.session,
        &cps,
        &roots,
        a.checkpoint.as_deref(),
        a.force,
    )?;
    let mut problems = 0;
    for e in &entries {
        let shown = &e.shown;
        match &e.step {
            Step::Skip { reason, forceable } => {
                problems += 1;
                out!(
                    "skip {shown}: {reason}{}",
                    if *forceable {
                        " (--force to overwrite)"
                    } else {
                        ""
                    }
                );
            }
            Step::Restore { .. } => out!(
                "{} {shown}",
                if a.apply { "restore" } else { "would restore" }
            ),
            Step::Remove => out!(
                "{} {shown} (did not exist before the run)",
                if a.apply { "remove" } else { "would remove" }
            ),
        }
        if a.apply {
            apply_entry(&r, &a.session, e)?;
        }
    }
    if !a.apply {
        out!("dry run: pass --apply to do it");
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
        store: Store,
        w: synthesis_session::SessionWriter,
    }

    fn rig() -> Rig {
        let tmp = tempfile::tempdir().unwrap();
        let base = synthesis_core::canonicalize(tmp.path()).unwrap();
        let ws = base.join("ws");
        fs::create_dir_all(&ws).unwrap();
        let store = Store::open(&base.join("home"), StoreOptions::default()).unwrap();
        let ctx = WorkspaceContext::open("w1", &ws, &ws, &[], ws.join(".art"), "pg").unwrap();
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
            store,
            w,
        }
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
        fn plan(&self, roots: &[PathBuf]) -> Vec<Entry> {
            let r = self.store.reader().unwrap();
            let cps = r.checkpoints("s1").unwrap();
            plan(&r, "s1", &cps, roots, None, false).unwrap()
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
        let v = g.plan(std::slice::from_ref(&g.ws));
        assert_eq!(v.len(), 2);
        assert!(matches!(v[0].step, Step::Restore { .. }) && v[0].path == a);
        assert_eq!(v[0].size, 4, "the bytes that would be written");
        assert!(matches!(v[1].step, Step::Remove) && v[1].path == b);
        assert_eq!(v[1].size, 7);
        assert_eq!(fs::read_to_string(&a).unwrap(), "new content");
        assert!(b.exists());
    }

    #[test]
    fn nothing_outside_the_roots_or_through_links_or_traversal_is_touched() {
        let mut g = rig();
        let a = g.run_wrote("a.txt", "x");
        // the checkpoint's file is outside the roots this rewind may write in
        let other = g.base.join("other");
        fs::create_dir_all(&other).unwrap();
        let v = g.plan(&[other]);
        assert_eq!(skip_reason(&v[0]), "outside the session's workspace");
        // the file became a symlink out of the workspace
        let outside = g.base.join("secret.txt");
        fs::write(&outside, "keep").unwrap();
        fs::remove_file(&a).unwrap();
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(&outside, &a).unwrap();
            let v = g.plan(std::slice::from_ref(&g.ws));
            assert_eq!(skip_reason(&v[0]), "a symlink or not a regular file");
            // a directory of the path became a link out: the parent resolves outside
            let mut g = rig();
            fs::create_dir_all(g.ws.join("d")).unwrap();
            let f = g.run_wrote("d/f.txt", "x");
            fs::remove_dir_all(g.ws.join("d")).unwrap();
            let out_dir = g.base.join("elsewhere");
            fs::create_dir_all(&out_dir).unwrap();
            fs::write(out_dir.join("f.txt"), "x").unwrap();
            std::os::unix::fs::symlink(&out_dir, g.ws.join("d")).unwrap();
            let v = g.plan(std::slice::from_ref(&g.ws));
            assert_eq!(skip_reason(&v[0]), "outside the session's workspace");
            assert_eq!(fs::read_to_string(out_dir.join("f.txt")).unwrap(), "x");
            let r = g.store.reader().unwrap();
            let cps = r.checkpoints("s1").unwrap();
            let done =
                apply_confirmed(&r, "s1", &cps, std::slice::from_ref(&g.ws), None, &v).unwrap();
            assert!(matches!(done[0].0.step, Step::Skip { .. }) && done[0].1.is_ok());
            assert_eq!(fs::read_to_string(out_dir.join("f.txt")).unwrap(), "x");
            let _ = f;
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
    fn a_secret_file_and_a_file_changed_since_the_run_are_skipped() {
        let mut g = rig();
        fs::write(g.ws.join(".env"), "KEY=1").unwrap();
        g.run_wrote(".env", "KEY=2");
        let a = g.run_wrote("a.txt", "x");
        fs::write(&a, "edited later by the person").unwrap();
        let v = g.plan(std::slice::from_ref(&g.ws));
        assert!(skip_reason(&v[0]).contains("secrets were masked"), "{v:?}");
        assert_eq!(skip_reason(&v[1]), "changed since the run");
    }

    #[test]
    fn a_confirmed_plan_writes_only_if_it_is_still_the_plan_that_was_shown() {
        let mut g = rig();
        fs::write(g.ws.join("a.txt"), "old").unwrap();
        let a = g.run_wrote("a.txt", "new");
        let b = g.run_wrote("b.txt", "made");
        let roots = [g.ws.clone()];
        let seen = g.plan(&roots);
        let r = g.store.reader().unwrap();
        let cps = r.checkpoints("s1").unwrap();
        // the person edits b.txt after the preview: nothing is written, not even a.txt
        fs::write(&b, "changed after preview").unwrap();
        let e = apply_confirmed(&r, "s1", &cps, &roots, None, &seen).unwrap_err();
        assert!(e.contains("nothing was written"), "{e}");
        assert_eq!(fs::read_to_string(&a).unwrap(), "new");
        // a fresh preview applies
        let seen = g.plan(&roots);
        assert!(matches!(seen[1].step, Step::Skip { .. }));
        let done = apply_confirmed(&r, "s1", &cps, &roots, None, &seen).unwrap();
        assert!(done.iter().all(|(_, r)| r.is_ok()));
        assert_eq!(fs::read_to_string(&a).unwrap(), "old");
        assert!(b.exists(), "a skipped file stays");
        // a second apply sees a different plan (a.txt is back to its checkpoint state)
        assert!(apply_confirmed(&r, "s1", &cps, &roots, None, &seen).is_err());
    }
}
